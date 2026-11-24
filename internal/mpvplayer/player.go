// Package mpvplayer adapts an embedded, audio-only libmpv to the playback API.
package mpvplayer

import (
	"errors"
	"fmt"
	"math"
	"runtime"
	"strconv"
	"strings"
	"time"

	"github.com/gen2brain/go-mpv"
	"github.com/nchapman/tacet-dmr/internal/playback"
)

type Player struct {
	m             *mpv.Mpv
	reading       int64
	finalDuration time.Duration
	shutdown      bool
	events        *networkEvents
}

var _ playback.Player = (*Player)(nil)

// Open selects an mpv device ID (empty for automatic). Exclusive requests
// require an explicit native device: some other drivers ignore audio-exclusive.
func Open(device string, exclusive bool) (*Player, error) {
	return open(device, exclusive, false)
}

// OpenNetwork accepts only individual HTTP audio resources from a controller.
// Never hand network-supplied playlists or external file references to mpv.
func OpenNetwork(device string, exclusive bool) (*Player, error) {
	return open(device, exclusive, true)
}

// IsExclusiveTarget reports whether the requested device/flag uses exclusive output.
func IsExclusiveTarget(device string, exclusive bool) bool {
	return exclusive || strings.HasPrefix(device, "coreaudio_exclusive/")
}

// ValidateTarget validates device/exclusive compatibility without opening hardware.
func ValidateTarget(device string, exclusive bool) error {
	if IsExclusiveTarget(device, exclusive) && !exclusiveDevice(runtime.GOOS, device) {
		return errors.New("exclusive output requires a named coreaudio, wasapi, or direct alsa/hw: device for this OS")
	}
	return nil
}

func open(device string, exclusive, network bool) (*Player, error) {
	if err := ValidateTarget(device, exclusive); err != nil {
		return nil, err
	}
	m := mpv.New()
	options := [][2]string{
		{"config", "no"}, {"load-scripts", "no"}, {"ytdl", "no"},
		{"vid", "no"}, {"sid", "no"}, {"audio-display", "no"},
		{"audio-file-auto", "no"}, {"sub-auto", "no"},
		{"idle", "yes"}, {"pause", "yes"}, {"gapless-audio", "weak"},
		{"audio-fallback-to-null", "no"},
	}
	if network {
		options = append(options,
			[2]string{"prefetch-playlist", "yes"},
			[2]string{"demuxer-max-bytes", "32MiB"},
			[2]string{"demuxer-max-back-bytes", "8MiB"},
			[2]string{"stop-playback-on-init-failure", "yes"},
			[2]string{"access-references", "no"},
			[2]string{"demuxer", "lavf"},
			[2]string{"demuxer-lavf-o", "format_whitelist=[wav,flac,mp3,ogg,mov,aac,aiff,matroska,webm,dsf,wv,ape,asf]"},
			[2]string{"network-timeout", "10"},
		)
	}
	if device != "" {
		options = append(options, [2]string{"audio-device", device})
	}
	if exclusive {
		options = append(options, [2]string{"audio-exclusive", "yes"})
	}
	for _, opt := range options {
		if err := m.SetOptionString(opt[0], opt[1]); err != nil {
			m.Destroy()
			return nil, fmt.Errorf("configure mpv %s: %w", opt[0], err)
		}
	}
	if err := m.Initialize(); err != nil {
		m.Destroy()
		return nil, fmt.Errorf("initialize libmpv: %w", err)
	}
	p := &Player{m: m}
	if network {
		if err := p.startNetworkEvents(); err != nil {
			p.Close()
			return nil, err
		}
	}
	return p, nil
}

func exclusiveDevice(os, device string) bool {
	driver, id, ok := strings.Cut(device, "/")
	if !ok || id == "" || id == "default" {
		return false
	}
	switch os {
	case "darwin":
		return driver == "coreaudio" || driver == "coreaudio_exclusive"
	case "windows":
		return driver == "wasapi"
	case "linux":
		return driver == "alsa" && strings.HasPrefix(id, "hw:") && len(id) > 3
	}
	return false
}

func (p *Player) Close() {
	if p.m == nil {
		return
	}
	if p.events != nil {
		close(p.events.stop)
		p.m.Wakeup()
		<-p.events.done
		p.events = nil
	}
	p.m.TerminateDestroy()
	p.m = nil
}

func (p *Player) Devices() ([]playback.Device, error) {
	value, err := p.m.GetProperty("audio-device-list", mpv.FormatNode)
	if err != nil {
		return nil, err
	}
	list, ok := value.([]any)
	if !ok {
		return nil, errors.New("invalid libmpv audio-device-list")
	}
	var devices []playback.Device
	for _, item := range list {
		d, _ := item.(map[string]any)
		name, _ := d["name"].(string)
		desc, _ := d["description"].(string)
		if name == "" {
			return nil, errors.New("unnamed libmpv audio device")
		}
		devices = append(devices, playback.Device{ID: name, Description: desc})
	}
	return devices, nil
}

func (p *Player) Prepare(tracks []string) error {
	if len(tracks) == 0 {
		return errors.New("empty playback queue")
	}
	for _, track := range tracks {
		if track == "" || strings.ContainsRune(track, 0) {
			return errors.New("empty or invalid track")
		}
	}
	if err := p.Pause(true); err != nil {
		return err
	}
	for i, track := range tracks {
		mode := "append"
		if i == 0 {
			mode = "replace"
		}
		if err := p.m.Command([]string{"loadfile", track, mode}); err != nil {
			_ = p.Stop()
			return fmt.Errorf("queue track %d: %w", i+1, err)
		}
	}
	return nil
}

func (p *Player) Pause(paused bool) error { return p.m.SetProperty("pause", mpv.FormatFlag, paused) }
func (p *Player) SetVolume(v int) error   { return p.m.SetPropertyString("volume", strconv.Itoa(v)) }
func (p *Player) SetMute(v bool) error    { return p.m.SetProperty("mute", mpv.FormatFlag, v) }
func (p *Player) Stop() error             { return p.m.Command([]string{"stop"}) }
func (p *Player) Seek(pos time.Duration) error {
	if pos < 0 {
		return errors.New("seek position cannot be negative")
	}
	return p.m.Command([]string{"seek", strconv.FormatFloat(pos.Seconds(), 'f', 6, 64), "absolute+exact"})
}

func safeDuration(sec float64) time.Duration {
	if math.IsNaN(sec) || math.IsInf(sec, 0) || sec <= 0 {
		return 0
	}
	return time.Duration(sec * float64(time.Second))
}

func (p *Player) Position() (pos, duration time.Duration) {
	seconds := func(key string) time.Duration {
		v, _ := p.m.GetProperty(key, mpv.FormatDouble)
		f, _ := v.(float64)
		return safeDuration(f)
	}
	return seconds("time-pos"), seconds("duration")
}

func (p *Player) Status() playback.Status {
	// Media properties are legitimately unavailable before loading and at EOF.
	pos, dur := p.Position()
	number := func(key string) int {
		v, _ := p.m.GetProperty(key, mpv.FormatInt64)
		n, _ := v.(int64)
		return int(n)
	}
	return playback.Status{Track: number("playlist-pos-1"), Paused: p.m.GetPropertyString("pause") == "yes",
		Position: pos, Duration: dur, Output: p.m.GetPropertyString("current-ao"),
		SourceRate: number("audio-params/samplerate"), OutputRate: number("audio-out-params/samplerate")}
}

func (p *Player) NextEvent(timeout time.Duration) (playback.Event, error) {
	n, err := p.NextNotice(timeout)
	return n.Kind, err
}

func (p *Player) NextNotice(timeout time.Duration) (playback.Notice, error) {
	if p.events != nil {
		return p.events.next(timeout)
	}
	return p.readNotice(timeout)
}

func (p *Player) readNotice(timeout time.Duration) (playback.Notice, error) {
	e := p.m.WaitEvent(timeout.Seconds())
	kind := playback.Ignored
	id := p.reading
	var err error
	var duration time.Duration
	switch e.EventID {
	case mpv.EventNone:
		kind = playback.NoEvent
	case mpv.EventStart:
		p.reading = e.StartFile().EntryID
		p.finalDuration = 0
		id = p.reading
	case mpv.EventPlaybackRestart:
		kind = playback.Ready
	case mpv.EventFileLoaded:
		kind = playback.TrackStarted
	case mpv.EventAudioReconfig:
		kind = playback.OutputChanged
	case mpv.EventHook:
		// Only on_unload is registered. The core holds the outgoing file open
		// until HookContinue, so this sample cannot belong to the successor.
		v, _ := p.m.GetProperty("duration", mpv.FormatDouble)
		sec, _ := v.(float64)
		p.finalDuration = safeDuration(sec)
		err = p.m.HookContinue(e.Hook().ID)
	case mpv.EventEnd:
		end := e.EndFile()
		id = end.EntryID
		if p.reading == id {
			p.reading = 0
		}
		switch end.Reason {
		case mpv.EndFileEOF:
			kind = playback.TrackFinished
			duration = p.finalDuration
		case mpv.EndFileStop, mpv.EndFileQuit:
			kind = playback.Stopped
		case mpv.EndFileError:
			err = fmt.Errorf("playback failed: %v", end.Error)
		default:
			err = fmt.Errorf("unexpected track end (%s); supply individual audio tracks", end.Reason)
		}
	case mpv.EventShutdown:
		id = 0
		p.reading = 0
		p.shutdown = true
		kind = playback.Stopped
	case mpv.EventQueueOverflow:
		id = 0
		err = errors.New("libmpv event queue overflow")
	}
	return playback.Notice{Kind: kind, Entry: id, Duration: duration}, err
}
