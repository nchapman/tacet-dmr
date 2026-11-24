//go:build darwin

package mpvplayer

import (
	"errors"
	"testing"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type fakeCoreAudioDevice struct {
	uid          string
	hasVMVC      bool
	vmvcSettable bool
	vmvc         float32
	hasVOLM      bool
	volmSettable bool
	volm         float32
	hasMute      bool
	muteSettable bool
	mute         bool
}

type fakeCoreAudioHAL struct {
	devices map[uint32]*fakeCoreAudioDevice
}

func (h *fakeCoreAudioHAL) deviceIDs() ([]uint32, error) {
	ids := make([]uint32, 0, len(h.devices))
	for id := range h.devices {
		ids = append(ids, id)
	}
	return ids, nil
}

func (h *fakeCoreAudioHAL) deviceUID(id uint32) (string, bool) {
	d, ok := h.devices[id]
	if !ok {
		return "", false
	}
	return d.uid, true
}

func (h *fakeCoreAudioHAL) isSettable(id uint32, selector uint32) bool {
	d, ok := h.devices[id]
	if !ok {
		return false
	}
	switch selector {
	case caVirtualMainVolume:
		return d.hasVMVC && d.vmvcSettable
	case caVolumeScalar:
		return d.hasVOLM && d.volmSettable
	case caMute:
		return d.hasMute && d.muteSettable
	default:
		return false
	}
}

func (h *fakeCoreAudioHAL) getFloat(id uint32, selector uint32) (float32, error) {
	d, ok := h.devices[id]
	if !ok {
		return 0, errors.New("missing device")
	}
	switch selector {
	case caVirtualMainVolume:
		return d.vmvc, nil
	case caVolumeScalar:
		return d.volm, nil
	default:
		return 0, errors.New("unknown selector")
	}
}

func (h *fakeCoreAudioHAL) setFloat(id uint32, selector uint32, v float32) error {
	d, ok := h.devices[id]
	if !ok {
		return errors.New("missing device")
	}
	switch selector {
	case caVirtualMainVolume:
		d.vmvc = v
		return nil
	case caVolumeScalar:
		d.volm = v
		return nil
	default:
		return errors.New("unknown selector")
	}
}

func (h *fakeCoreAudioHAL) getMute(id uint32) (bool, error) {
	d, ok := h.devices[id]
	if !ok {
		return false, errors.New("missing device")
	}
	return d.mute, nil
}

func (h *fakeCoreAudioHAL) setMute(id uint32, m bool) error {
	d, ok := h.devices[id]
	if !ok {
		return errors.New("missing device")
	}
	d.mute = m
	return nil
}

func TestCoreAudioExclusiveUIDMatching(t *testing.T) {
	for _, tc := range []struct {
		device    string
		exclusive bool
		wantUID   string
		wantOK    bool
	}{
		{"coreaudio/BuiltInSpeakerDevice", true, "BuiltInSpeakerDevice", true},
		{"coreaudio/AppleUSBAudioEngine:Questyle:M15:0:1", true, "AppleUSBAudioEngine:Questyle:M15:0:1", true},
		{"coreaudio_exclusive/BuiltInSpeakerDevice", false, "BuiltInSpeakerDevice", true},
		{"coreaudio/BuiltInSpeakerDevice", false, "", false},
		{"coreaudio/default", true, "", false},
		{"coreaudio/", true, "", false},
		{"null/", true, "", false},
		{"auto", false, "", false},
	} {
		uid, ok := coreAudioExclusiveUID(tc.device, tc.exclusive)
		if uid != tc.wantUID || ok != tc.wantOK {
			t.Errorf("coreAudioExclusiveUID(%q, %t) = (%q, %t), want (%q, %t)", tc.device, tc.exclusive, uid, ok, tc.wantUID, tc.wantOK)
		}
	}
}

func TestCoreAudioVolumePrefersVMVCAndReResolvesAfterReplug(t *testing.T) {
	hal := &fakeCoreAudioHAL{
		devices: map[uint32]*fakeCoreAudioDevice{
			85: {
				uid:          "DAC-1",
				hasVMVC:      true,
				vmvcSettable: true,
				vmvc:         0.1875,
				hasMute:      true,
				muteSettable: true,
				mute:         false,
			},
		},
	}
	vc := openCoreAudioVolume(hal, "DAC-1")
	if vc == nil {
		t.Fatal("expected CoreAudio volume control")
	}
	got, err := vc.Volume()
	if err != nil || got != 19 {
		t.Fatalf("Volume() = (%d, %v), want (19, nil)", got, err)
	}
	if err := vc.SetVolume(55); err != nil {
		t.Fatalf("SetVolume: %v", err)
	}
	if got, err := vc.Volume(); err != nil || got != 55 {
		t.Fatalf("Volume after set = (%d, %v), want (55, nil)", got, err)
	}
	if err := vc.SetMute(true); err != nil {
		t.Fatalf("SetMute: %v", err)
	}
	if m, err := vc.Mute(); err != nil || !m {
		t.Fatalf("Mute = (%t, %v), want (true, nil)", m, err)
	}

	// Simulate unplug and replug: old ID 85 is recycled to a different device,
	// and DAC-1 returns with a new AudioDeviceID (99).
	hal.devices[85] = &fakeCoreAudioDevice{uid: "OtherDevice", hasVMVC: true, vmvcSettable: true, vmvc: 1.0}
	hal.devices[99] = &fakeCoreAudioDevice{uid: "DAC-1", hasVMVC: true, vmvcSettable: true, vmvc: 0.42, hasMute: true, muteSettable: true}

	if got, err := vc.Volume(); err != nil || got != 42 {
		t.Fatalf("Volume after replug = (%d, %v), want (42, nil)", got, err)
	}
	if err := vc.SetVolume(30); err != nil {
		t.Fatalf("SetVolume after replug: %v", err)
	}
	if hal.devices[85].vmvc != 1.0 {
		t.Fatalf("recycled ID 85 was mutated: %.2f", hal.devices[85].vmvc)
	}
	if got, _ := vc.Volume(); got != 30 {
		t.Fatalf("DAC-1 volume = %d, want 30", got)
	}
}

func TestCoreAudioVolumeFallsBackToVOLMAndReportsUnsupportedMute(t *testing.T) {
	hal := &fakeCoreAudioHAL{
		devices: map[uint32]*fakeCoreAudioDevice{
			12: {
				uid:          "DAC-NoMute",
				hasVOLM:      true,
				volmSettable: true,
				volm:         0.75,
				hasMute:      false,
			},
		},
	}
	vc := openCoreAudioVolume(hal, "DAC-NoMute")
	if vc == nil {
		t.Fatal("expected volume control via volm fallback")
	}
	if got, err := vc.Volume(); err != nil || got != 75 {
		t.Fatalf("Volume = (%d, %v), want (75, nil)", got, err)
	}
	if _, err := vc.Mute(); !errors.Is(err, playback.ErrUnsupported) {
		t.Fatalf("Mute err = %v, want ErrUnsupported", err)
	}
	if err := vc.SetMute(true); !errors.Is(err, playback.ErrUnsupported) {
		t.Fatalf("SetMute err = %v, want ErrUnsupported", err)
	}
}

func TestCoreAudioVolumeReturnsNilForFixedOutputDevice(t *testing.T) {
	hal := &fakeCoreAudioHAL{
		devices: map[uint32]*fakeCoreAudioDevice{
			42: {uid: "FixedDAC", hasVMVC: true, vmvcSettable: false},
		},
	}
	if vc := openCoreAudioVolume(hal, "FixedDAC"); vc != nil {
		t.Fatalf("fixed-output device should fall back to mpv, got %v", vc)
	}
	if vc := openCoreAudioVolume(hal, "MissingDAC"); vc != nil {
		t.Fatalf("missing device should fall back to mpv, got %v", vc)
	}
}

func TestOpenVolumeControlReturnsNilForNonExclusiveOrNull(t *testing.T) {
	if vc := OpenVolumeControl("null/", true); vc != nil {
		t.Fatalf("null/ should use mpv default, got %v", vc)
	}
	if vc := OpenVolumeControl("coreaudio/BuiltInSpeakerDevice", false); vc != nil {
		t.Fatalf("shared coreaudio should use mpv default, got %v", vc)
	}
	if p := VolumeProber("null/", true); p != nil {
		t.Fatal("null/ should not return a volume prober")
	}
	if p := VolumeProber("coreaudio/BuiltInSpeakerDevice", false); p != nil {
		t.Fatal("shared coreaudio should not return a volume prober")
	}
	if p := VolumeProber("coreaudio/BuiltInSpeakerDevice", true); p == nil {
		t.Fatal("exclusive coreaudio should return a hotplug volume prober")
	}
}

func TestSystemCoreAudioReadOnlySmoke(t *testing.T) {
	ids, err := systemCoreAudio.deviceIDs()
	if err != nil {
		t.Fatalf("deviceIDs: %v", err)
	}
	for _, id := range ids {
		uid, ok := systemCoreAudio.deviceUID(id)
		if !ok || uid == "" {
			t.Errorf("device %d returned empty UID", id)
		}
	}
}
