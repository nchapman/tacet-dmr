package renderer

import (
	"context"
	"encoding/xml"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type volumeEngine struct {
	queueEngine
	volumes []int
	mutes   []bool
}

func (e *volumeEngine) SetVolume(v int) error {
	e.volumes = append(e.volumes, v)
	return nil
}

func (e *volumeEngine) SetMute(m bool) error {
	e.mutes = append(e.mutes, m)
	return nil
}

type fakeVolumeControl struct {
	mu       sync.Mutex
	vol      int
	mute     bool
	noMute   bool
	quantize func(int) int
	volErr   error
	muteErr  error
	setCalls []int
}

func (f *fakeVolumeControl) Volume() (int, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.volErr != nil {
		return 0, f.volErr
	}
	return f.vol, nil
}

func (f *fakeVolumeControl) SetVolume(v int) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.volErr != nil {
		return f.volErr
	}
	f.setCalls = append(f.setCalls, v)
	if f.quantize != nil {
		v = f.quantize(v)
	}
	f.vol = v
	return nil
}

func (f *fakeVolumeControl) Mute() (bool, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.noMute {
		return false, playback.ErrUnsupported
	}
	if f.muteErr != nil {
		return false, f.muteErr
	}
	return f.mute, nil
}

func (f *fakeVolumeControl) SetMute(m bool) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.noMute {
		return playback.ErrUnsupported
	}
	if f.muteErr != nil {
		return f.muteErr
	}
	f.mute = m
	return nil
}

func (f *fakeVolumeControl) String() string { return "fake hardware" }

func TestDefaultVolumeUsesEngine(t *testing.T) {
	var eng *volumeEngine
	level := 30
	tpt := newTransport(func() (Engine, error) {
		eng = &volumeEngine{queueEngine: queueEngine{entries: make(map[int64]string)}}
		return eng, nil
	}, &level, nil)

	out, f := tpt.call(rc, "GetVolume", fields{"InstanceID": "0", "Channel": "Master"})
	if f != nil || out["CurrentVolume"] != "30" {
		t.Fatalf("GetVolume: out=%v fault=%v", out, f)
	}
	if _, f := tpt.call(av, "SetAVTransportURI", fields{"InstanceID": "0", "CurrentURI": "http://127.0.0.1/a.wav", "CurrentURIMetaData": ""}); f != nil {
		t.Fatalf("SetAVTransportURI: %v", f)
	}
	if _, f := tpt.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"}); f != nil {
		t.Fatalf("Play: %v", f)
	}
	if eng == nil || len(eng.volumes) != 1 || eng.volumes[0] != 30 {
		t.Fatalf("engine startup volume: %+v", eng)
	}
	if _, f := tpt.call(rc, "SetVolume", fields{"InstanceID": "0", "Channel": "Master", "DesiredVolume": "62"}); f != nil {
		t.Fatalf("SetVolume: %v", f)
	}
	if _, f := tpt.call(rc, "SetMute", fields{"InstanceID": "0", "Channel": "Master", "DesiredMute": "1"}); f != nil {
		t.Fatalf("SetMute: %v", f)
	}
	if len(eng.volumes) != 2 || eng.volumes[1] != 62 || len(eng.mutes) != 2 || !eng.mutes[1] {
		t.Fatalf("engine calls: volumes=%v mutes=%v", eng.volumes, eng.mutes)
	}
}

func TestPlatformVolumeOverrideKeepsEngineAtUnity(t *testing.T) {
	var eng *volumeEngine
	hw := &fakeVolumeControl{
		vol:      19,
		mute:     false,
		quantize: func(v int) int { return (v / 2) * 2 }, // simulate coarse hardware steps
	}
	tpt := newTransport(func() (Engine, error) {
		eng = &volumeEngine{queueEngine: queueEngine{entries: make(map[int64]string)}}
		return eng, nil
	}, nil, hw)

	// Startup reads current hardware level without overwriting it.
	if len(hw.setCalls) != 0 {
		t.Fatalf("unexpected startup hardware write: %v", hw.setCalls)
	}
	out, f := tpt.call(rc, "GetVolume", fields{"InstanceID": "0", "Channel": "Master"})
	if f != nil || out["CurrentVolume"] != "19" {
		t.Fatalf("GetVolume: out=%v fault=%v", out, f)
	}

	// Opening playback leaves mpv at unity software volume (100, unmuted).
	if _, f := tpt.call(av, "SetAVTransportURI", fields{"InstanceID": "0", "CurrentURI": "http://127.0.0.1/a.wav", "CurrentURIMetaData": ""}); f != nil {
		t.Fatalf("SetAVTransportURI: %v", f)
	}
	if _, f := tpt.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"}); f != nil {
		t.Fatalf("Play: %v", f)
	}
	if len(eng.volumes) != 1 || eng.volumes[0] != 100 || len(eng.mutes) != 1 || eng.mutes[0] {
		t.Fatalf("engine must stay at unity when hardware volume+mute active: volumes=%v mutes=%v", eng.volumes, eng.mutes)
	}

	// Live SetVolume/SetMute route to hardware and report quantized readback.
	if _, f := tpt.call(rc, "SetVolume", fields{"InstanceID": "0", "Channel": "Master", "DesiredVolume": "55"}); f != nil {
		t.Fatalf("SetVolume: %v", f)
	}
	if _, f := tpt.call(rc, "SetMute", fields{"InstanceID": "0", "Channel": "Master", "DesiredMute": "1"}); f != nil {
		t.Fatalf("SetMute: %v", f)
	}
	if len(eng.volumes) != 1 || len(eng.mutes) != 1 {
		t.Fatalf("live hardware volume/mute must not call engine: volumes=%v mutes=%v", eng.volumes, eng.mutes)
	}
	out, _ = tpt.call(rc, "GetVolume", fields{"InstanceID": "0", "Channel": "Master"})
	if out["CurrentVolume"] != "54" {
		t.Fatalf("quantized readback: got %q want 54", out["CurrentVolume"])
	}
	out, _ = tpt.call(rc, "GetMute", fields{"InstanceID": "0", "Channel": "Master"})
	if out["CurrentMute"] != "1" {
		t.Fatalf("hardware mute readback: got %q want 1", out["CurrentMute"])
	}

	// FactoryDefaults restores startup level (quantized) and unmutes hardware.
	if _, f := tpt.call(rc, "SelectPreset", fields{"InstanceID": "0", "PresetName": "FactoryDefaults"}); f != nil {
		t.Fatalf("SelectPreset: %v", f)
	}
	out, _ = tpt.call(rc, "GetVolume", fields{"InstanceID": "0", "Channel": "Master"})
	if out["CurrentVolume"] != "18" {
		t.Fatalf("after FactoryDefaults: got %q want 18", out["CurrentVolume"])
	}
	out, _ = tpt.call(rc, "GetMute", fields{"InstanceID": "0", "Channel": "Master"})
	if out["CurrentMute"] != "0" {
		t.Fatalf("after FactoryDefaults mute: got %q want 0", out["CurrentMute"])
	}

	// Hardware write failure returns UPnP 501 without corrupting stored volume.
	hw.mu.Lock()
	hw.volErr = errors.New("device unplugged")
	hw.mu.Unlock()
	if _, f := tpt.call(rc, "SetVolume", fields{"InstanceID": "0", "Channel": "Master", "DesiredVolume": "70"}); f == nil || f.code != 501 {
		t.Fatalf("expected 501 on hardware error, got %v", f)
	}
	out, _ = tpt.call(rc, "GetVolume", fields{"InstanceID": "0", "Channel": "Master"})
	if out["CurrentVolume"] != "18" {
		t.Fatalf("cached volume changed on failed write: got %q want 18", out["CurrentVolume"])
	}
}

func TestPlatformVolumeWithoutHardwareMuteFallsBackToEngineMute(t *testing.T) {
	var eng *volumeEngine
	hw := &fakeVolumeControl{vol: 40, noMute: true}
	tpt := newTransport(func() (Engine, error) {
		eng = &volumeEngine{queueEngine: queueEngine{entries: make(map[int64]string)}}
		return eng, nil
	}, nil, hw)

	if _, f := tpt.call(rc, "SetMute", fields{"InstanceID": "0", "Channel": "Master", "DesiredMute": "1"}); f != nil {
		t.Fatalf("SetMute before Play: %v", f)
	}
	if _, f := tpt.call(av, "SetAVTransportURI", fields{"InstanceID": "0", "CurrentURI": "http://127.0.0.1/a.wav", "CurrentURIMetaData": ""}); f != nil {
		t.Fatalf("SetAVTransportURI: %v", f)
	}
	if _, f := tpt.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"}); f != nil {
		t.Fatalf("Play: %v", f)
	}
	if len(eng.volumes) != 1 || eng.volumes[0] != 100 {
		t.Fatalf("engine volume must stay at unity: %v", eng.volumes)
	}
	if len(eng.mutes) != 1 || !eng.mutes[0] {
		t.Fatalf("engine must receive software mute fallback: %v", eng.mutes)
	}
	if _, f := tpt.call(rc, "SetMute", fields{"InstanceID": "0", "Channel": "Master", "DesiredMute": "0"}); f != nil {
		t.Fatalf("live SetMute: %v", f)
	}
	if len(eng.mutes) != 2 || eng.mutes[1] {
		t.Fatalf("live software mute fallback: %v", eng.mutes)
	}
}

func TestExternalHardwareVolumeChangePublishesGENA(t *testing.T) {
	hw := &fakeVolumeControl{vol: 25, mute: false}
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
	d := newDevice("Desk", "uuid:test", srv.Listener.Addr().String(), netip.MustParsePrefix("127.0.0.0/24"), nil, hw, nil)
	defer d.Close()
	srv.Config.Handler = d
	srv.Start()
	defer srv.Close()

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
	if res.StatusCode != 200 {
		t.Fatalf("subscribe: %s", res.Status)
	}

	select {
	case msg := <-notified:
		if !strings.Contains(msg, `<Volume channel="Master" val="25"`) {
			t.Fatalf("initial event: %s", msg)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("missing initial NOTIFY")
	}

	// Simulate an external OS/hardware volume change while idle.
	hw.mu.Lock()
	hw.vol = 42
	hw.mu.Unlock()

	select {
	case msg := <-notified:
		if !strings.Contains(msg, `<Volume channel="Master" val="42"`) {
			t.Fatalf("external volume event: %s", msg)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("missing NOTIFY after external hardware volume change")
	}
}

func TestServeExplicitInitialVolumeAppliesToHardwareAndValidatesRange(t *testing.T) {
	hw := &fakeVolumeControl{vol: 80}
	want := 35
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if err := Serve(ctx, Config{Listen: "127.0.0.1:0", Controller: "127.0.0.1", Name: "DAC", NoSSDP: true, InitialVolume: &want, Volume: hw}, nil, io.Discard); err != nil {
		t.Fatalf("Serve: %v", err)
	}
	if len(hw.setCalls) != 1 || hw.setCalls[0] != 35 || hw.vol != 35 {
		t.Fatalf("expected initial hardware SetVolume(35), got calls=%v vol=%d", hw.setCalls, hw.vol)
	}

	for _, bad := range []int{-1, 101} {
		v := bad
		ctx, cancel := context.WithCancel(t.Context())
		cancel()
		if err := Serve(ctx, Config{Listen: "127.0.0.1:0", Controller: "127.0.0.1", Name: "DAC", NoSSDP: true, InitialVolume: &v}, nil, io.Discard); err == nil {
			t.Fatalf("Serve accepted out-of-range InitialVolume %d", bad)
		}
	}
}
