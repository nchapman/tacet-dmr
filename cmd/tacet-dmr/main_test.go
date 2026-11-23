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
	if err := run(context.Background(), []string{"-help"}, nil, io.Discard); !errors.Is(err, flag.ErrHelp) {
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
	if err := run(context.Background(), []string{"-device=null/", path}, nil, &out); err == nil || !strings.Contains(err.Error(), "track 1") {
		t.Fatalf("failed file: got %v (%s)", err, &out)
	}
}
