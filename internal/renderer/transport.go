package renderer

import (
	"errors"
	"fmt"
	"io"
	"log"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type Engine interface {
	Enqueue(uri string) (int64, error) // add at tail and play if idle; ID unique within handle
	Discard(entry int64) error
	NextNotice(time.Duration) (playback.Notice, error)
	Pause(bool) error
	Seek(time.Duration) error
	Status() playback.Status
	SetVolume(int) error
	SetMute(bool) error
	Close()
}
type Factory func() (Engine, error)
type fields map[string]string
type fault struct {
	code    int
	message string
}

var invalid = &fault{402, "Invalid Args"}
var failed = &fault{501, "Action Failed"}

type deviceOptions struct {
	exclusive     bool
	initialVolume *int
	volume        playback.VolumeControl
	probeVolume   func() playback.VolumeControl
	log           io.Writer
}

type outputReport struct {
	output                 string
	sourceRate, outputRate int
}

// The renderer owns queue order; an engine session holds just the active entry
// and the next natural successor. Explicit jumps/stops retire the whole session.
type transport struct {
	open                  Factory
	p                     Engine
	vol                   playback.VolumeControl // nil: mpv handles volume and mute (default)
	probeVol              func() playback.VolumeControl
	nextProbe, nextSync   time.Time
	volOffline            bool
	applyInitialToHW      bool // explicit -volume pending until hotplugged hardware appears
	explicitInitialVolume bool
	active, loggedEntry   int64
	next                  stagedEntry
	ended                 bool // natural EOF received, waiting for successor to become playable
	q                     playlist
	state, status         string
	pos, duration         time.Duration
	volume, initialVolume int
	mute, hwMute          bool
	logger                *log.Logger
	lastOut               outputReport
}

func clampVolume(v int) int {
	if v < 0 {
		return 0
	}
	if v > 100 {
		return 100
	}
	return v
}

func sanitizeURI(raw string) string {
	u, err := url.Parse(raw)
	if err != nil {
		return "<invalid-uri>"
	}
	u.User = nil
	u.RawQuery = ""
	u.ForceQuery = false
	u.Fragment = ""
	return u.String()
}

func parseUPnPBool(s string) (bool, error) {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "1", "true", "yes":
		return true, nil
	case "0", "false", "no":
		return false, nil
	default:
		return false, errors.New("invalid boolean")
	}
}

func newTransport(open Factory, opts deviceOptions) *transport {
	v := 100
	if opts.exclusive {
		v = 30
	}
	if opts.initialVolume != nil {
		v = clampVolume(*opts.initialVolume)
	}
	t := &transport{
		open:                  open,
		vol:                   opts.volume,
		probeVol:              opts.probeVolume,
		applyInitialToHW:      opts.initialVolume != nil && opts.volume == nil && opts.probeVolume != nil,
		explicitInitialVolume: opts.initialVolume != nil,
		q:                     playlist{mode: normal},
		state:                 "NO_MEDIA_PRESENT",
		status:                "OK",
		volume:                v,
		initialVolume:         v,
	}
	if opts.log != nil {
		t.logger = log.New(opts.log, "", log.LstdFlags)
	}
	if t.vol != nil {
		if hwVol, err := t.vol.Volume(); err == nil {
			t.volume = clampVolume(hwVol)
			if opts.initialVolume == nil {
				t.initialVolume = t.volume
			}
		}
		if hwMute, err := t.vol.Mute(); err == nil {
			t.hwMute = true
			t.mute = hwMute
		}
	}
	return t
}

func (t *transport) logf(format string, args ...any) {
	if t.logger != nil {
		t.logger.Printf(format, args...)
	}
}

func (t *transport) logOutput() {
	if t.p == nil {
		return
	}
	s := t.p.Status()
	if s.Output == "" || s.SourceRate <= 0 || s.OutputRate <= 0 {
		return
	}
	rep := outputReport{output: s.Output, sourceRate: s.SourceRate, outputRate: s.OutputRate}
	if rep == t.lastOut {
		return
	}
	t.lastOut = rep
	t.logf("output: %s · %d Hz source → %d Hz output", rep.output, rep.sourceRate, rep.outputRate)
}

func (t *transport) attachVolume(vol playback.VolumeControl) {
	if t.applyInitialToHW {
		if err := vol.SetVolume(t.volume); err != nil {
			t.logf("error: apply initial volume to %v: %v", vol, err)
			return
		}
		t.applyInitialToHW = false
	}
	t.vol = vol
	t.volOffline = false
	t.nextSync = time.Now().Add(500 * time.Millisecond)
	if m, err := vol.Mute(); err == nil {
		t.hwMute = true
		t.mute = m
	} else {
		t.hwMute = false
	}
	if hwVol, err := vol.Volume(); err == nil {
		t.volume = clampVolume(hwVol)
		if !t.explicitInitialVolume {
			t.initialVolume = t.volume
		}
	}
	if t.p != nil {
		_ = t.p.SetVolume(100)
		_ = t.p.SetMute(t.mute && !t.hwMute)
	}
	muteMode := "software"
	if t.hwMute {
		muteMode = "hardware"
	}
	t.logf("volume: attached %v (level=%d, mute=%s)", vol, t.volume, muteMode)
}

type positionReader interface {
	Position() (pos, duration time.Duration)
}

func (t *transport) samplePosition() {
	if t.p == nil || t.ended {
		return
	}
	var pos, dur time.Duration
	if pr, ok := t.p.(positionReader); ok {
		pos, dur = pr.Position()
	} else {
		s := t.p.Status()
		pos, dur = s.Position, s.Duration
	}
	t.pos = pos
	if dur > 0 {
		t.duration = dur
	}
}

func (t *transport) syncVolume(force, watch bool) {
	if t.vol == nil {
		if t.probeVol == nil {
			return
		}
		if !force && time.Now().Before(t.nextProbe) {
			return
		}
		t.nextProbe = time.Now().Add(time.Second)
		if vol := t.probeVol(); vol != nil {
			t.attachVolume(vol)
		}
		return
	}
	if t.volOffline {
		if !force && time.Now().Before(t.nextProbe) {
			return
		}
	} else if !force && (!watch || time.Now().Before(t.nextSync)) {
		return
	}
	v, err := t.vol.Volume()
	if err != nil {
		if !t.volOffline {
			t.volOffline = true
			t.logf("volume: %v unavailable: %v", t.vol, err)
		}
		t.nextProbe = time.Now().Add(time.Second)
		return
	}
	t.nextSync = time.Now().Add(500 * time.Millisecond)
	t.volume = clampVolume(v)
	if t.volOffline {
		t.volOffline = false
		if m, err := t.vol.Mute(); err == nil {
			t.hwMute = true
			t.mute = m
		} else if errors.Is(err, playback.ErrUnsupported) {
			t.hwMute = false
		}
		if t.p != nil {
			_ = t.p.SetMute(t.mute && !t.hwMute)
		}
		t.logf("volume: %v reconnected (level=%d)", t.vol, t.volume)
		return
	}
	if t.hwMute {
		if m, err := t.vol.Mute(); err == nil {
			t.mute = m
		} else if errors.Is(err, playback.ErrUnsupported) {
			t.hwMute = false
			if t.p != nil {
				_ = t.p.SetMute(t.mute)
			}
		}
	}
}
func (t *transport) release() {
	if t.p != nil {
		t.p.Close()
		t.p = nil
	}
	t.active = 0
	t.loggedEntry = 0
	t.lastOut = outputReport{}
	t.next = stagedEntry{}
	t.ended = false
	t.state = "STOPPED"
	if len(t.q.items) == 0 {
		t.state = "NO_MEDIA_PRESENT"
	}
}
func validURI(s string) bool {
	u, err := url.Parse(s)
	return err == nil && (u.Scheme == "http" || u.Scheme == "https") && u.Hostname() != "" && u.User == nil && !strings.ContainsAny(s, "\x00\r\n\\")
}
func (t *transport) play() error {
	if t.p != nil {
		if err := t.p.Pause(false); err != nil {
			t.logf("error: resume playback: %v", err)
			return err
		}
		if t.state == "PAUSED_PLAYBACK" {
			t.state = "PLAYING"
			t.logf("resume")
		}
		return nil
	}
	return t.openTrack(false)
}

func (t *transport) resetTrack() { t.release(); t.pos = 0; t.duration = 0; t.status = "OK" }

// Jumps preserve pause/stop for AVTransport navigation; PlayQueueWithIndex
// explicitly requests playback. Automatic joins use the existing handle instead.
func (t *transport) selectTrack(i int, state string) *fault {
	t.q.index = i
	t.resetTrack()
	if state == "STOPPED" || state == "NO_MEDIA_PRESENT" {
		return nil
	}
	if err := t.openTrack(state == "PAUSED_PLAYBACK"); err != nil {
		t.status = "ERROR_OCCURRED"
		return failed
	}
	return nil
}
func (t *transport) openTrack(paused bool) error {
	t.syncVolume(true, true)
	p, err := t.open()
	if err != nil {
		t.logf("error: open playback: %v", err)
		return err
	}
	t.p = p
	if t.vol != nil {
		// Platform hardware volume controls the output level; keep mpv at
		// unity software gain so samples are not double-attenuated.
		err = p.SetVolume(100)
		if err == nil {
			if t.hwMute {
				err = p.SetMute(false)
			} else {
				err = p.SetMute(t.mute)
			}
		}
	} else {
		if err = p.SetVolume(t.volume); err == nil {
			err = p.SetMute(t.mute)
		}
	}
	if err == nil {
		err = p.Pause(true)
	}
	if err == nil {
		t.active, err = p.Enqueue(t.q.current().uri)
	}
	if err == nil {
		err = t.syncNext()
	}
	if err == nil && !paused {
		err = p.Pause(false)
	}
	if err != nil {
		t.logf("error: start track %s: %v", sanitizeURI(t.q.current().uri), err)
		t.release()
		return err
	}
	t.state = "TRANSITIONING"
	if paused {
		t.state = "PAUSED_PLAYBACK"
	}
	t.status = "OK"
	t.pos = 0
	return nil
}
func timestamp(d time.Duration) string {
	if d < 0 {
		d = 0
	}
	s := int64(d / time.Second)
	return fmt.Sprintf("%d:%02d:%02d", s/3600, s/60%60, s%60)
}
func parseTime(s string) (time.Duration, error) {
	parts := strings.Split(s, ":")
	if len(parts) != 3 {
		return 0, errors.New("invalid time")
	}
	h, err := strconv.ParseUint(parts[0], 10, 32)
	if err != nil || h > 1000000 {
		return 0, errors.New("invalid hours")
	}
	m, err := strconv.ParseUint(parts[1], 10, 8)
	if err != nil || m > 59 {
		return 0, errors.New("invalid minutes")
	}
	for _, c := range parts[2] {
		if (c < '0' || c > '9') && c != '.' {
			return 0, errors.New("invalid seconds")
		}
	}
	sec, err := time.ParseDuration(parts[2] + "s")
	if err != nil || sec >= time.Minute {
		return 0, errors.New("invalid seconds")
	}
	return time.Duration(h)*time.Hour + time.Duration(m)*time.Minute + sec, nil
}
func (t *transport) call(service, name string, a fields) (fields, *fault) {
	out, f := t.dispatch(service, name, a)
	if f == nil && t.p != nil {
		if err := t.syncNext(); err != nil {
			t.logf("error: stage next track: %v", err)
			t.fail()
			return nil, failed
		}
	}
	return out, f
}
func (t *transport) dispatch(service, name string, a fields) (fields, *fault) {
	if service == pq {
		return t.queueAction(name, a)
	}
	if service != cm && a["InstanceID"] != "0" {
		code := 718
		if service == rc {
			code = 702
		}
		return nil, &fault{code, "Invalid InstanceID"}
	}
	if ch, ok := a["Channel"]; ok && ch != "Master" {
		return nil, invalid
	}
	cur := t.q.current()
	num, nr := t.q.trackNumbers()
	switch name {
	case "SetAVTransportURI":
		uri := a["CurrentURI"]
		if uri != "" && !validURI(uri) {
			return nil, &fault{714, "Illegal MIME-Type or unsupported URI"}
		}
		t.q = playlist{mode: normal, shuffle: t.q.shuffle}
		if uri != "" {
			t.q.items = []track{{uri, a["CurrentURIMetaData"]}}
		}
		t.resetTrack()
	case "SetNextAVTransportURI":
		if len(t.q.items) == 0 || t.q.name != "" {
			return nil, &fault{701, "Transition not available"}
		}
		uri := a["NextURI"]
		if uri != "" && !validURI(uri) {
			return nil, &fault{714, "Unsupported URI"}
		}
		t.q.items = []track{cur}
		t.q.index = 0
		if uri != "" {
			t.q.items = append(t.q.items, track{uri, a["NextURIMetaData"]})
		}
		t.q.rebuildOrder()
	case "Next", "Previous":
		delta := 1
		if name == "Previous" {
			delta = -1
		}
		i, ok := t.q.step(delta, false)
		if !ok {
			return nil, &fault{701, "Transition not available"}
		}
		return nil, t.selectTrack(i, t.state)
	case "SetPlayMode":
		m := a["NewPlayMode"]
		if _, ok := loopCodes[m]; !ok {
			return nil, &fault{712, "Play mode not supported"}
		}
		t.q.setMode(m)
	case "GetCurrentTransportActions":
		var list []string
		if num != 0 {
			list = append(list, "Play", "Stop")
			if t.p != nil {
				list = append(list, "Pause")
				if !t.ended {
					list = append(list, "Seek")
				}
			}
			if _, ok := t.q.step(1, false); ok {
				list = append(list, "Next")
			}
			if _, ok := t.q.step(-1, false); ok {
				list = append(list, "Previous")
			}
		}
		return fields{"Actions": strings.Join(list, ",")}, nil
	case "Play":
		if a["Speed"] != "1" {
			return nil, &fault{717, "Play speed not supported"}
		}
		if len(t.q.items) == 0 {
			return nil, &fault{701, "Transition not available"}
		}
		if err := t.play(); err != nil {
			t.status = "ERROR_OCCURRED"
			return nil, failed
		}
	case "Pause":
		if t.p == nil {
			return nil, &fault{701, "Transition not available"}
		}
		if err := t.p.Pause(true); err != nil {
			t.logf("error: pause playback: %v", err)
			return nil, failed
		}
		if t.state != "PAUSED_PLAYBACK" {
			t.logf("pause")
		}
		t.state = "PAUSED_PLAYBACK"
	case "Stop":
		if t.p != nil {
			t.logf("stop")
		}
		t.release()
		t.pos = 0
		t.status = "OK"
	case "Seek":
		if a["Unit"] != "REL_TIME" && a["Unit"] != "ABS_TIME" {
			return nil, &fault{710, "Seek mode not supported"}
		}
		pos, err := parseTime(a["Target"])
		if err != nil {
			return nil, &fault{711, "Illegal seek target"}
		}
		if t.p == nil || t.ended {
			return nil, &fault{701, "Transition not available"}
		}
		if err := t.p.Seek(pos); err != nil {
			return nil, &fault{711, "Illegal seek target"}
		}
	case "GetTransportInfo":
		return fields{"CurrentTransportState": t.state, "CurrentTransportStatus": t.status, "CurrentSpeed": "1"}, nil
	case "GetPositionInfo":
		t.samplePosition()
		return fields{"Track": strconv.Itoa(num), "TrackDuration": timestamp(t.duration), "TrackMetaData": cur.metadata, "TrackURI": cur.uri, "RelTime": timestamp(t.pos), "AbsTime": timestamp(t.pos), "RelCount": "2147483647", "AbsCount": "2147483647"}, nil
	case "GetMediaInfo":
		next := t.q.nextTrack()
		if nr <= 1 && t.duration == 0 {
			t.samplePosition()
		}
		duration := t.duration
		if nr > 1 {
			duration = 0
		} // Total duration of a remote queue is unknown.
		return fields{"NrTracks": strconv.Itoa(nr), "MediaDuration": timestamp(duration), "NextURI": next.uri, "NextURIMetaData": next.metadata, "CurrentURI": cur.uri, "CurrentURIMetaData": cur.metadata, "PlayMedium": "NETWORK", "RecordMedium": "NOT_IMPLEMENTED", "WriteStatus": "NOT_IMPLEMENTED"}, nil
	case "GetDeviceCapabilities":
		return fields{"PlayMedia": "NETWORK", "RecMedia": "NOT_IMPLEMENTED", "RecQualityModes": "NOT_IMPLEMENTED"}, nil
	case "GetTransportSettings":
		return fields{"PlayMode": t.q.mode, "RecQualityMode": "NOT_IMPLEMENTED"}, nil
	case "GetVolume":
		t.syncVolume(true, true)
		return fields{"CurrentVolume": strconv.Itoa(t.volume)}, nil
	case "SetVolume":
		v, err := strconv.Atoi(a["DesiredVolume"])
		if err != nil || v < 0 || v > 100 {
			return nil, invalid
		}
		if t.vol == nil || t.volOffline {
			t.syncVolume(true, true)
		}
		t.applyInitialToHW = false
		if t.vol != nil {
			if err := t.vol.SetVolume(v); err != nil {
				t.logf("error: volume write: %v", err)
				return nil, failed
			}
			if actual, err := t.vol.Volume(); err == nil {
				t.volume = clampVolume(actual)
			} else {
				t.volume = v
			}
		} else {
			if t.p != nil {
				if err := t.p.SetVolume(v); err != nil {
					t.logf("error: volume write: %v", err)
					return nil, failed
				}
			}
			t.volume = v
		}
	case "GetMute":
		t.syncVolume(true, true)
		return fields{"CurrentMute": onOff(t.mute)}, nil
	case "SetMute":
		v, err := parseUPnPBool(a["DesiredMute"])
		if err != nil {
			return nil, invalid
		}
		if t.vol == nil || t.volOffline {
			t.syncVolume(true, true)
		}
		if t.vol != nil && t.hwMute {
			if err := t.vol.SetMute(v); err != nil {
				t.logf("error: mute write: %v", err)
				return nil, failed
			}
			if actual, err := t.vol.Mute(); err == nil {
				t.mute = actual
			} else {
				t.mute = v
			}
		} else {
			if t.p != nil {
				if err := t.p.SetMute(v); err != nil {
					t.logf("error: mute write: %v", err)
					return nil, failed
				}
			}
			t.mute = v
		}
	case "ListPresets":
		return fields{"CurrentPresetNameList": "FactoryDefaults"}, nil
	case "SelectPreset":
		if a["PresetName"] != "FactoryDefaults" {
			return nil, invalid
		}
		if _, f := t.dispatch(rc, "SetVolume", fields{"InstanceID": "0", "Channel": "Master", "DesiredVolume": strconv.Itoa(t.initialVolume)}); f != nil {
			return nil, f
		}
		return t.dispatch(rc, "SetMute", fields{"InstanceID": "0", "Channel": "Master", "DesiredMute": "0"})
	case "GetProtocolInfo":
		return fields{"Sink": protocols}, nil
	case "GetCurrentConnectionIDs":
		return fields{"ConnectionIDs": "0"}, nil
	case "GetCurrentConnectionInfo":
		if a["ConnectionID"] != "0" {
			return nil, &fault{706, "Invalid connection reference"}
		}
		return fields{"RcsID": "0", "AVTransportID": "0", "PeerConnectionID": "-1", "Direction": "Input", "Status": "OK"}, nil
	default:
		return nil, &fault{401, "Invalid Action"}
	}
	return nil, nil
}
func onOff(b bool) string {
	if b {
		return "1"
	}
	return "0"
}
