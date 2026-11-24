package renderer

import (
	"errors"
	"fmt"
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

// The renderer owns queue order; an engine session holds just the active entry
// and the next natural successor. Explicit jumps/stops retire the whole session.
type transport struct {
	open                  Factory
	p                     Engine
	active                int64
	next                  stagedEntry
	ended                 bool // natural EOF received, waiting for successor to become playable
	q                     playlist
	state, status         string
	pos, duration         time.Duration
	volume, initialVolume int
	mute                  bool
}

func newTransport(open Factory, initialVolume int) *transport {
	return &transport{open: open, q: playlist{mode: normal}, state: "NO_MEDIA_PRESENT", status: "OK", volume: initialVolume, initialVolume: initialVolume}
}
func (t *transport) release() {
	if t.p != nil {
		t.p.Close()
		t.p = nil
	}
	t.active = 0
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
			return err
		}
		if t.state == "PAUSED_PLAYBACK" {
			t.state = "PLAYING"
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
	p, err := t.open()
	if err != nil {
		return err
	}
	t.p = p
	if err = p.SetVolume(t.volume); err == nil {
		err = p.SetMute(t.mute)
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
		t.q = playlist{mode: normal}
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
		switch m := a["NewPlayMode"]; m {
		case normal, repeatAll, repeatOne:
			t.q.mode = m
		default:
			return nil, &fault{712, "Play mode not supported"}
		}
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
			return nil, failed
		}
		t.state = "PAUSED_PLAYBACK"
	case "Stop":
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
		return fields{"Track": strconv.Itoa(num), "TrackDuration": timestamp(t.duration), "TrackMetaData": cur.metadata, "TrackURI": cur.uri, "RelTime": timestamp(t.pos), "AbsTime": timestamp(t.pos), "RelCount": "2147483647", "AbsCount": "2147483647"}, nil
	case "GetMediaInfo":
		next := t.q.nextTrack()
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
		return fields{"CurrentVolume": strconv.Itoa(t.volume)}, nil
	case "SetVolume":
		v, err := strconv.Atoi(a["DesiredVolume"])
		if err != nil || v < 0 || v > 100 {
			return nil, invalid
		}
		if t.p != nil && t.p.SetVolume(v) != nil {
			return nil, failed
		}
		t.volume = v
	case "GetMute":
		return fields{"CurrentMute": onOff(t.mute)}, nil
	case "SetMute":
		v, err := strconv.ParseBool(a["DesiredMute"])
		if err != nil {
			return nil, invalid
		}
		if t.p != nil && t.p.SetMute(v) != nil {
			return nil, failed
		}
		t.mute = v
	case "ListPresets":
		return fields{"CurrentPresetNameList": "FactoryDefaults"}, nil
	case "SelectPreset":
		if a["PresetName"] != "FactoryDefaults" {
			return nil, invalid
		}
		if _, f := t.call(rc, "SetVolume", fields{"InstanceID": "0", "DesiredVolume": strconv.Itoa(t.initialVolume)}); f != nil {
			return nil, f
		}
		return t.call(rc, "SetMute", fields{"InstanceID": "0", "DesiredMute": "0"})
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
