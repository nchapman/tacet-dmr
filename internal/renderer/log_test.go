package renderer

import (
	"bytes"
	"encoding/xml"
	"errors"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type outputEngine struct {
	volumeEngine
	status playback.Status
}

func (e *outputEngine) Status() playback.Status {
	s := e.status
	s.Paused = e.paused
	return s
}

func TestHotplugVolumeProbeAttachesAndPublishesGENA(t *testing.T) {
	var (
		mu       sync.Mutex
		attached *fakeVolumeControl
		eng      *volumeEngine
		logs     bytes.Buffer
	)
	probe := func() playback.VolumeControl {
		mu.Lock()
		defer mu.Unlock()
		if attached == nil {
			return nil
		}
		return attached
	}

	notified := make(chan string, 8)
	cb := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var prop struct {
			Change string `xml:"property>LastChange"`
		}
		if err := xml.NewDecoder(r.Body).Decode(&prop); err != nil {
			t.Error(err)
			return
		}
		notified <- prop.Change
	}))
	defer cb.Close()

	srv := httptest.NewUnstartedServer(nil)
	d := newDevice("DAC", "uuid:test", srv.Listener.Addr().String(), netip.MustParsePrefix("127.0.0.0/24"),
		deviceOptions{exclusive: true, probeVolume: probe, log: &logs},
		func() (Engine, error) {
			eng = &volumeEngine{queueEngine: queueEngine{entries: make(map[int64]string)}}
			return eng, nil
		})
	defer d.Close()
	srv.Config.Handler = d
	srv.Start()
	defer srv.Close()

	// Subscribe while DAC is disconnected: initial volume is exclusive software default 30.
	req, err := http.NewRequest("SUBSCRIBE", srv.URL+"/RenderingControl/events", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("CALLBACK", "<"+cb.URL+">")
	req.Header.Set("NT", "upnp:event")
	req.Header.Set("TIMEOUT", "Second-60")
	res, err := srv.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	select {
	case msg := <-notified:
		if !strings.Contains(msg, `<Volume channel="Master" val="30"`) {
			t.Fatalf("initial event while disconnected: %s", msg)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("missing initial NOTIFY")
	}

	// Plug in DAC at hardware level 64: probe picks it up, switches to hardware, and publishes GENA.
	hw := &fakeVolumeControl{vol: 64, mute: false}
	mu.Lock()
	attached = hw
	mu.Unlock()
	// Reset probe throttle so the next tick attaches immediately in the test.
	d.mu.Lock()
	d.t.nextProbe = time.Time{}
	d.mu.Unlock()

	select {
	case msg := <-notified:
		if !strings.Contains(msg, `<Volume channel="Master" val="64"`) {
			t.Fatalf("hotplug attach event: %s", msg)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("missing NOTIFY after DAC hotplug")
	}

	// Subsequent Play must keep mpv at unity software gain (100), not 30.
	d.mu.Lock()
	_, f1 := d.t.call(av, "SetAVTransportURI", fields{"InstanceID": "0", "CurrentURI": "http://127.0.0.1/a.wav", "CurrentURIMetaData": ""})
	_, f2 := d.t.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"})
	d.mu.Unlock()
	if f1 != nil || f2 != nil {
		t.Fatalf("play after hotplug: %v %v", f1, f2)
	}
	if eng == nil || len(eng.volumes) != 1 || eng.volumes[0] != 100 {
		t.Fatalf("engine should run at unity after hotplug attach, got %+v", eng)
	}
	if len(hw.setCalls) != 0 {
		t.Fatalf("hotplug attach without explicit -volume should not overwrite hardware: %v", hw.setCalls)
	}
	if !strings.Contains(logs.String(), "volume: attached fake hardware") {
		t.Fatalf("expected attach log, got %q", logs.String())
	}
}

func TestHotplugAppliesExplicitInitialVolumeOnAttach(t *testing.T) {
	var hw *fakeVolumeControl
	want := 25
	tpt := newTransport(nil, deviceOptions{
		exclusive:     true,
		initialVolume: &want,
		probeVolume: func() playback.VolumeControl {
			if hw == nil {
				return nil
			}
			return hw
		},
	})
	hw = &fakeVolumeControl{vol: 100, mute: false}
	tpt.nextProbe = time.Time{}
	tpt.refresh()

	if len(hw.setCalls) != 1 || hw.setCalls[0] != 25 || tpt.volume != 25 {
		t.Fatalf("explicit -volume 25 should apply on first DAC attach: calls=%v vol=%d", hw.setCalls, tpt.volume)
	}
}

func TestHardwareDisconnectAndReconnectLogsOncePerTransition(t *testing.T) {
	var logs bytes.Buffer
	hw := &fakeVolumeControl{vol: 50, mute: false}
	tpt := newTransport(nil, deviceOptions{volume: hw, log: &logs})

	// Disconnect across multiple refreshes: should log once, not spam.
	hw.mu.Lock()
	hw.volErr = errors.New("not found")
	hw.mu.Unlock()
	for range 5 {
		tpt.nextProbe = time.Time{}
		tpt.refresh()
	}
	if n := strings.Count(logs.String(), "unavailable"); n != 1 {
		t.Fatalf("expected 1 disconnect log, got %d in %q", n, logs.String())
	}

	// Reconnect at a new level and without hardware mute: logs once and updates state.
	hw.mu.Lock()
	hw.volErr = nil
	hw.vol = 38
	hw.noMute = true
	hw.mu.Unlock()
	tpt.nextProbe = time.Time{}
	tpt.refresh()
	if tpt.volume != 38 || tpt.hwMute {
		t.Fatalf("reconnect state: volume=%d hwMute=%t", tpt.volume, tpt.hwMute)
	}
	if n := strings.Count(logs.String(), "reconnected"); n != 1 {
		t.Fatalf("expected 1 reconnect log, got %d in %q", n, logs.String())
	}
}

func TestOperationalLoggingSanitizesURLAndDeduplicatesOutput(t *testing.T) {
	var logs bytes.Buffer
	var eng *outputEngine
	tpt := newTransport(func() (Engine, error) {
		eng = &outputEngine{
			volumeEngine: volumeEngine{queueEngine: queueEngine{entries: make(map[int64]string)}},
			status:       playback.Status{Output: "coreaudio_exclusive", SourceRate: 96000, OutputRate: 96000},
		}
		return eng, nil
	}, deviceOptions{log: &logs})

	if _, f := tpt.call(av, "SetAVTransportURI", fields{
		"InstanceID":         "0",
		"CurrentURI":         "http://192.168.1.10:4320/media/local/42?token=secret123#frag",
		"CurrentURIMetaData": "",
	}); f != nil {
		t.Fatal(f)
	}
	if _, f := tpt.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"}); f != nil {
		t.Fatal(f)
	}
	// Trigger two identical OutputChanged events plus Ready; output line should log once.
	eng.events = append(eng.events,
		playback.Notice{Kind: playback.OutputChanged},
		playback.Notice{Kind: playback.OutputChanged},
	)
	tpt.refresh()
	// Routine polls must not log.
	beforePoll := logs.Len()
	tpt.call(av, "GetPositionInfo", fields{"InstanceID": "0"})
	tpt.call(av, "GetTransportInfo", fields{"InstanceID": "0"})
	tpt.call(rc, "GetVolume", fields{"InstanceID": "0", "Channel": "Master"})
	if logs.Len() != beforePoll {
		t.Fatalf("routine polls logged unexpectedly: %q", logs.String()[beforePoll:])
	}

	// Pause, resume, stop.
	tpt.call(av, "Pause", fields{"InstanceID": "0"})
	tpt.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"})
	tpt.call(av, "Stop", fields{"InstanceID": "0"})

	got := logs.String()
	if strings.Contains(got, "secret123") || strings.Contains(got, "frag") {
		t.Fatalf("URL query/fragment leaked into log: %q", got)
	}
	for _, want := range []string{
		"play: track 1/1 http://192.168.1.10:4320/media/local/42",
		"output: coreaudio_exclusive · 96000 Hz source → 96000 Hz output",
		"pause",
		"resume",
		"stop",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("log missing %q:\n%s", want, got)
		}
	}
	if n := strings.Count(got, "output: coreaudio_exclusive"); n != 1 {
		t.Errorf("expected deduplicated output log (1), got %d:\n%s", n, got)
	}
}
