package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
	"github.com/nchapman/tacet-dmr/internal/renderer"
)

func TestMediaSources(t *testing.T) {
	file := filepath.Join(t.TempDir(), "01 a song.wav")
	if err := os.WriteFile(file, nil, 0600); err != nil {
		t.Fatal(err)
	}
	for _, source := range []string{file, "https://example.org/music?id=123&token=a%2Fb", "http://192.168.1.4:4320/media/local/123"} {
		if got, err := mediaSource(source); err != nil || got != source {
			t.Errorf("source changed or rejected: %q, %v", got, err)
		}
	}
	for _, source := range []string{"", "file:///audio.flac", "http://", t.TempDir(), file + ".absent", "bad\x00source"} {
		if _, err := mediaSource(source); err == nil {
			t.Errorf("accepted %q", source)
		}
	}
}

func TestHelpDoesNotRequireTracks(t *testing.T) {
	if err := run(context.Background(), []string{"-help"}, nil, io.Discard, io.Discard); !errors.Is(err, flag.ErrHelp) {
		t.Fatal(err)
	}
}

// A scripted engine can exercise unsolicited shutdown, which isn't something
// the caller can reliably induce with a real audio device.
type stoppingPlayer struct {
	playback.Player
	events  []playback.Event
	stopped bool
}

func (*stoppingPlayer) Prepare([]string) error { return nil }
func (*stoppingPlayer) Pause(bool) error       { return nil }
func (p *stoppingPlayer) Stop() error          { p.stopped = true; return nil }
func (p *stoppingPlayer) NextEvent(time.Duration) (playback.Event, error) {
	if len(p.events) == 0 {
		return playback.NoEvent, errors.New("unexpected event read")
	}
	e := p.events[0]
	p.events = p.events[1:]
	return e, nil
}

func TestUnexpectedStopReportsUnfinishedTrack(t *testing.T) {
	p := &stoppingPlayer{events: []playback.Event{playback.TrackFinished, playback.Stopped}}
	err := play(context.Background(), p, []string{"one", "two"}, nil, io.Discard)
	if err == nil || !strings.Contains(err.Error(), "track 2") {
		t.Fatalf("unexpected shutdown: got %v", err)
	}
}

func TestRequestedStopSucceeds(t *testing.T) {
	p := &stoppingPlayer{}
	commands := make(chan string, 1)
	commands <- "stop"
	if err := play(context.Background(), p, []string{"one", "two"}, commands, io.Discard); err != nil || !p.stopped {
		t.Fatalf("requested stop: stopped=%t, err=%v", p.stopped, err)
	}
}

func TestQueueFileFailureIsNotReportedAsSuccess(t *testing.T) {
	path := filepath.Join(t.TempDir(), "bad.flac")
	if err := os.WriteFile(path, []byte("not audio"), 0600); err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	if err := run(context.Background(), []string{"-device=null/", path}, nil, &out, io.Discard); err == nil || !strings.Contains(err.Error(), "track 1") {
		t.Fatalf("failed file: got %v (%s)", err, &out)
	}
}

type stubVolumeControl struct {
	level  int
	writes []int
	err    error
}

func (s *stubVolumeControl) Volume() (int, error) { return s.level, s.err }
func (s *stubVolumeControl) SetVolume(v int) error {
	if s.err != nil {
		return s.err
	}
	s.writes = append(s.writes, v)
	s.level = v
	return nil
}
func (*stubVolumeControl) Mute() (bool, error) { return false, playback.ErrUnsupported }
func (*stubVolumeControl) SetMute(bool) error  { return playback.ErrUnsupported }
func (*stubVolumeControl) String() string      { return "stub hardware" }

func TestLocalVolumeWiring(t *testing.T) {
	t.Run("hardware present preserves device level and forces unity software gain", func(t *testing.T) {
		hw := &stubVolumeControl{level: 19}
		var soft int
		if err := configureLocalVolume(func(v int) error { soft = v; return nil }, hw, nil, true); err != nil {
			t.Fatal(err)
		}
		if len(hw.writes) != 0 || soft != 100 {
			t.Fatalf("hw writes=%v soft=%d, want no hw writes and soft=100", hw.writes, soft)
		}
	})
	t.Run("hardware present with explicit volume writes hardware and keeps unity software gain", func(t *testing.T) {
		hw := &stubVolumeControl{level: 80}
		want := 35
		var soft int
		if err := configureLocalVolume(func(v int) error { soft = v; return nil }, hw, &want, true); err != nil {
			t.Fatal(err)
		}
		if len(hw.writes) != 1 || hw.writes[0] != 35 || soft != 100 {
			t.Fatalf("hw writes=%v soft=%d, want [35] and soft=100", hw.writes, soft)
		}
	})
	t.Run("hardware and software errors surface with distinct context", func(t *testing.T) {
		want := 25
		hw := &stubVolumeControl{err: errors.New("boom")}
		if err := configureLocalVolume(func(int) error { return nil }, hw, &want, true); err == nil || !strings.Contains(err.Error(), "set initial volume") {
			t.Fatalf("got %v, want set initial volume error", err)
		}
		hw = &stubVolumeControl{level: 25}
		if err := configureLocalVolume(func(int) error { return errors.New("mpv") }, hw, nil, true); err == nil || !strings.Contains(err.Error(), "set software gain") {
			t.Fatalf("got %v, want set software gain error", err)
		}
	})
	t.Run("software fallback defaults to 30 exclusive and 100 shared", func(t *testing.T) {
		var exclSoft, sharedSoft int
		if err := configureLocalVolume(func(v int) error { exclSoft = v; return nil }, nil, nil, true); err != nil {
			t.Fatal(err)
		}
		if err := configureLocalVolume(func(v int) error { sharedSoft = v; return nil }, nil, nil, false); err != nil {
			t.Fatal(err)
		}
		if exclSoft != 30 || sharedSoft != 100 {
			t.Fatalf("excl=%d shared=%d, want 30 and 100", exclSoft, sharedSoft)
		}
	})
}

func TestServeVolumeWiring(t *testing.T) {
	hw := &stubVolumeControl{level: 19}
	var withHW renderer.Config
	configureServerVolume(&withHW, hw, nil, true)
	if withHW.Volume != hw || withHW.InitialVolume != nil || !withHW.Exclusive {
		t.Fatalf("hardware exclusive should preserve nil InitialVolume, got %+v", withHW)
	}

	var softExcl renderer.Config
	configureServerVolume(&softExcl, nil, nil, true)
	if softExcl.Volume != nil || softExcl.StartupVolume() != 30 {
		t.Fatalf("software exclusive should default to 30, got %+v", softExcl)
	}

	// Exercise the full run(-serve) path with an injected hardware override.
	prevOpen := openVolumeControl
	prevProbe := volumeProber
	openVolumeControl = func(string, bool) playback.VolumeControl { return hw }
	volumeProber = func(string, bool) func() playback.VolumeControl { return nil }
	t.Cleanup(func() {
		openVolumeControl = prevOpen
		volumeProber = prevProbe
	})
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	var out bytes.Buffer
	if err := run(ctx, []string{"-serve", "127.0.0.1:0", "-controller", "127.0.0.1", "-no-ssdp", "-exclusive"}, nil, &out, io.Discard); err != nil {
		t.Fatalf("run(-serve): %v", err)
	}
	if len(hw.writes) != 0 || !strings.Contains(out.String(), "Volume: stub hardware") {
		t.Fatalf("writes=%v out=%q", hw.writes, out.String())
	}

	// When the hardware device is absent at startup but a prober is active,
	// startup announces it is watching for hardware and preserves nil InitialVolume.
	openVolumeControl = func(string, bool) playback.VolumeControl { return nil }
	volumeProber = func(string, bool) func() playback.VolumeControl { return func() playback.VolumeControl { return nil } }
	out.Reset()
	ctx2, cancel2 := context.WithCancel(t.Context())
	cancel2()
	if err := run(ctx2, []string{"-serve", "127.0.0.1:0", "-controller", "127.0.0.1", "-no-ssdp", "-exclusive"}, nil, &out, io.Discard); err != nil {
		t.Fatalf("run(-serve watching): %v", err)
	}
	if !strings.Contains(out.String(), "Volume: mpv software (watching for hardware device)") {
		t.Fatalf("unexpected watching banner: %q", out.String())
	}
}
