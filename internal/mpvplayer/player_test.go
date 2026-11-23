package mpvplayer

import (
	"bytes"
	"encoding/binary"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

// A generated mono 8 kHz / 16-bit PCM file. The null output still exercises
// real libmpv demuxing, decoding, queueing and timing without touching a DAC.
func wav(frames int) []byte {
	b := make([]byte, 44+frames*2)
	copy(b, "RIFF")
	binary.LittleEndian.PutUint32(b[4:], uint32(len(b)-8))
	copy(b[8:], "WAVEfmt ")
	binary.LittleEndian.PutUint32(b[16:], 16)
	binary.LittleEndian.PutUint16(b[20:], 1)
	binary.LittleEndian.PutUint16(b[22:], 1)
	binary.LittleEndian.PutUint32(b[24:], 8000)
	binary.LittleEndian.PutUint32(b[28:], 16000)
	binary.LittleEndian.PutUint16(b[32:], 2)
	binary.LittleEndian.PutUint16(b[34:], 16)
	copy(b[36:], "data")
	binary.LittleEndian.PutUint32(b[40:], uint32(frames*2))
	return b
}

func server(t *testing.T, frames int) string {
	t.Helper()
	data := wav(frames)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/handoff" {
			http.Redirect(w, r, "/music", http.StatusFound)
			return
		}
		http.ServeContent(w, r, "music.wav", time.Time{}, bytes.NewReader(data))
	}))
	t.Cleanup(srv.Close)
	return srv.URL + "/handoff"
}

func player(t *testing.T) *Player {
	t.Helper()
	p, err := Open("null/", false)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(p.Close)
	return p
}

func wait(t *testing.T, p *Player, match func(playback.Event) bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		e, err := p.NextEvent(50 * time.Millisecond)
		if err != nil {
			t.Fatal(err)
		}
		if match(e) {
			return
		}
	}
	t.Fatalf("timed out, player status: %+v", p.Status())
}

func TestQueueAdvancesThroughRepeatedHTTPTrack(t *testing.T) {
	u := server(t, 1200)
	p := player(t)
	if err := p.Prepare([]string{u, u}); err != nil {
		t.Fatal(err)
	}
	if count := p.m.GetPropertyString("playlist-count"); count != "2" || !p.Status().Paused {
		t.Fatalf("queue not prepared before play: %s tracks, status %+v", count, p.Status())
	}
	if err := p.Pause(false); err != nil {
		t.Fatal(err)
	}
	var order []playback.Event
	wait(t, p, func(e playback.Event) bool {
		if e == playback.TrackStarted || e == playback.TrackFinished {
			order = append(order, e)
		}
		return len(order) == 4
	})
	if order[0] != playback.TrackStarted || order[1] != playback.TrackFinished || order[2] != playback.TrackStarted || order[3] != playback.TrackFinished {
		t.Fatalf("unexpected event order: %v", order)
	}
}

func TestPreparedTrackSupportsSeekResumePauseAndStop(t *testing.T) {
	u := server(t, 8000*12)
	p := player(t)
	if err := p.Prepare([]string{u}); err != nil {
		t.Fatal(err)
	}
	wait(t, p, func(e playback.Event) bool { return e == playback.TrackStarted })
	s := p.Status()
	if !s.Paused || s.Track != 1 || s.Duration != 12*time.Second {
		t.Fatalf("prepared: %+v", s)
	}
	if err := p.Seek(4 * time.Second); err != nil {
		t.Fatal(err)
	}
	wait(t, p, func(playback.Event) bool { return p.Status().Position >= 4*time.Second })
	if err := p.Pause(false); err != nil {
		t.Fatal(err)
	}
	wait(t, p, func(playback.Event) bool { return p.Status().Position > 4100*time.Millisecond })
	if err := p.Pause(true); err != nil {
		t.Fatal(err)
	}
	s = p.Status()
	if !s.Paused || s.Output != "null" || s.SourceRate != 8000 {
		t.Fatalf("paused: %+v", s)
	}
	if err := p.Stop(); err != nil {
		t.Fatal(err)
	}
	wait(t, p, func(e playback.Event) bool { return e == playback.Stopped })
}

func TestUndecodableMediaIsReportedAsFailure(t *testing.T) {
	s := httptest.NewServer(http.NotFoundHandler())
	defer s.Close()
	p := player(t)
	if err := p.Prepare([]string{s.URL}); err != nil {
		t.Fatal(err)
	}
	if err := p.Pause(false); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		e, err := p.NextEvent(50 * time.Millisecond)
		if err != nil {
			return
		}
		if e == playback.TrackFinished {
			t.Fatal("reported EOF for failed HTTP request")
		}
	}
	t.Fatal("no failure reported")
}

// Weak gapless must allow a new output format at a rate change rather than
// forcing the second file through a resampler to keep the first AO open.
func TestNetworkHandoffCanChangeSampleRate(t *testing.T) {
	first := server(t, 8000)
	data := wav(32000)
	binary.LittleEndian.PutUint32(data[24:], 16000)
	binary.LittleEndian.PutUint32(data[28:], 32000)
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.ServeContent(w, r, "16k.wav", time.Time{}, bytes.NewReader(data))
	}))
	defer second.Close()
	p, err := OpenNetwork("null/", false)
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if _, err = p.Enqueue(first); err != nil {
		t.Fatal(err)
	}
	if _, err = p.Enqueue(second.URL); err != nil {
		t.Fatal(err)
	}
	if err = p.Pause(false); err != nil {
		t.Fatal(err)
	}
	for _, rate := range []int{8000, 16000} {
		wait(t, p, func(e playback.Event) bool {
			if e != playback.Ready {
				return false
			}
			s := p.Status()
			return s.SourceRate == rate && s.OutputRate == rate
		})
	}
}

func TestExclusiveDevicePolicy(t *testing.T) {
	for _, tc := range []struct {
		os, id string
		ok     bool
	}{{"linux", "alsa/hw:CARD=DAC,DEV=0", true}, {"linux", "alsa/plughw:1", false}, {"linux", "pipewire/auto", false}, {"linux", "alsa/hw:", false}, {"darwin", "auto", false}, {"darwin", "coreaudio/", false}, {"darwin", "coreaudio/SomeUID", true}, {"windows", "wasapi/{GUID}", true}, {"windows", "wasapi/default", false}, {"windows", "null/", false}} {
		if exclusiveDevice(tc.os, tc.id) != tc.ok {
			t.Errorf("%s %s: want %t", tc.os, tc.id, tc.ok)
		}
	}
}
