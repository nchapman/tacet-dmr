//go:build darwin

package mpvplayer

import (
	"bytes"
	"errors"
	"fmt"
	"math"
	"strings"
	"sync"
	"unsafe"

	"github.com/ebitengine/purego"
	"github.com/nchapman/tacet-dmr/internal/playback"
)

const (
	caSystemObject      uint32 = 1
	caDevices           uint32 = 0x64657623 // 'dev#'
	caDeviceUID         uint32 = 0x75696420 // 'uid '
	caScopeGlobal       uint32 = 0x676c6f62 // 'glob'
	caScopeOutput       uint32 = 0x6f757470 // 'outp'
	caElementMain       uint32 = 0
	caVirtualMainVolume uint32 = 0x766d7663 // 'vmvc'
	caVolumeScalar      uint32 = 0x766f6c6d // 'volm'
	caMute              uint32 = 0x6d757465 // 'mute'
	cfStringUTF8        uint32 = 0x08000100
)

type caPropAddr struct {
	selector, scope, element uint32
}

type coreAudioHAL interface {
	deviceIDs() ([]uint32, error)
	deviceUID(id uint32) (string, bool)
	isSettable(id uint32, selector uint32) bool
	getFloat(id uint32, selector uint32) (float32, error)
	setFloat(id uint32, selector uint32, v float32) error
	getMute(id uint32) (bool, error)
	setMute(id uint32, m bool) error
}

type darwinCoreAudio struct {
	once      sync.Once
	err       error
	hasProp   func(uint32, *caPropAddr) uint8
	isSet     func(uint32, *caPropAddr, *uint8) int32
	getSize   func(uint32, *caPropAddr, uint32, unsafe.Pointer, *uint32) int32
	getData   func(uint32, *caPropAddr, uint32, unsafe.Pointer, *uint32, unsafe.Pointer) int32
	setData   func(uint32, *caPropAddr, uint32, unsafe.Pointer, uint32, unsafe.Pointer) int32
	cfGetCStr func(uintptr, *byte, int64, uint32) uint8
	cfRelease func(uintptr)
}

var systemCoreAudio darwinCoreAudio

func (a *darwinCoreAudio) load() error {
	a.once.Do(func() {
		ca, err := purego.Dlopen("/System/Library/Frameworks/CoreAudio.framework/CoreAudio", purego.RTLD_NOW)
		if err != nil {
			a.err = err
			return
		}
		cf, err := purego.Dlopen("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation", purego.RTLD_NOW)
		if err != nil {
			a.err = err
			return
		}
		purego.RegisterLibFunc(&a.hasProp, ca, "AudioObjectHasProperty")
		purego.RegisterLibFunc(&a.isSet, ca, "AudioObjectIsPropertySettable")
		purego.RegisterLibFunc(&a.getSize, ca, "AudioObjectGetPropertyDataSize")
		purego.RegisterLibFunc(&a.getData, ca, "AudioObjectGetPropertyData")
		purego.RegisterLibFunc(&a.setData, ca, "AudioObjectSetPropertyData")
		purego.RegisterLibFunc(&a.cfGetCStr, cf, "CFStringGetCString")
		purego.RegisterLibFunc(&a.cfRelease, cf, "CFRelease")
	})
	return a.err
}

func (a *darwinCoreAudio) deviceIDs() ([]uint32, error) {
	if err := a.load(); err != nil {
		return nil, err
	}
	addr := caPropAddr{caDevices, caScopeGlobal, caElementMain}
	var sz uint32
	if rc := a.getSize(caSystemObject, &addr, 0, nil, &sz); rc != 0 {
		return nil, fmt.Errorf("CoreAudio device list size: %d", rc)
	}
	if sz < 4 {
		return nil, nil
	}
	ids := make([]uint32, sz/4)
	sz = uint32(len(ids) * 4)
	if rc := a.getData(caSystemObject, &addr, 0, nil, &sz, unsafe.Pointer(&ids[0])); rc != 0 {
		return nil, fmt.Errorf("CoreAudio device list: %d", rc)
	}
	n := min(int(sz/4), len(ids))
	return ids[:n], nil
}

func (a *darwinCoreAudio) deviceUID(id uint32) (string, bool) {
	if id == 0 || a.load() != nil {
		return "", false
	}
	addr := caPropAddr{caDeviceUID, caScopeGlobal, caElementMain}
	// CoreFoundation handles can be tagged pointers (NSTaggedPointerString),
	// so keep CFStringRef as uintptr rather than unsafe.Pointer.
	var ref uintptr
	sz := uint32(unsafe.Sizeof(ref))
	if rc := a.getData(id, &addr, 0, nil, &sz, unsafe.Pointer(&ref)); rc != 0 || ref == 0 {
		return "", false
	}
	defer a.cfRelease(ref)
	buf := make([]byte, 1024)
	if a.cfGetCStr(ref, &buf[0], int64(len(buf)), cfStringUTF8) == 0 {
		return "", false
	}
	if idx := bytes.IndexByte(buf, 0); idx >= 0 {
		return string(buf[:idx]), true
	}
	return "", false
}

func (a *darwinCoreAudio) isSettable(id uint32, selector uint32) bool {
	if a.load() != nil {
		return false
	}
	addr := caPropAddr{selector, caScopeOutput, caElementMain}
	if a.hasProp(id, &addr) == 0 {
		return false
	}
	var settable uint8
	return a.isSet(id, &addr, &settable) == 0 && settable != 0
}

func (a *darwinCoreAudio) getFloat(id uint32, selector uint32) (float32, error) {
	if err := a.load(); err != nil {
		return 0, err
	}
	addr := caPropAddr{selector, caScopeOutput, caElementMain}
	var v float32
	sz := uint32(unsafe.Sizeof(v))
	if rc := a.getData(id, &addr, 0, nil, &sz, unsafe.Pointer(&v)); rc != 0 {
		return 0, fmt.Errorf("CoreAudio read 0x%08x: %d", selector, rc)
	}
	return v, nil
}

func (a *darwinCoreAudio) setFloat(id uint32, selector uint32, v float32) error {
	if err := a.load(); err != nil {
		return err
	}
	addr := caPropAddr{selector, caScopeOutput, caElementMain}
	if rc := a.setData(id, &addr, 0, nil, uint32(unsafe.Sizeof(v)), unsafe.Pointer(&v)); rc != 0 {
		return fmt.Errorf("CoreAudio write 0x%08x: %d", selector, rc)
	}
	return nil
}

func (a *darwinCoreAudio) getMute(id uint32) (bool, error) {
	if err := a.load(); err != nil {
		return false, err
	}
	addr := caPropAddr{caMute, caScopeOutput, caElementMain}
	var v uint32
	sz := uint32(unsafe.Sizeof(v))
	if rc := a.getData(id, &addr, 0, nil, &sz, unsafe.Pointer(&v)); rc != 0 {
		return false, fmt.Errorf("CoreAudio read mute: %d", rc)
	}
	return v != 0, nil
}

func (a *darwinCoreAudio) setMute(id uint32, m bool) error {
	if err := a.load(); err != nil {
		return err
	}
	addr := caPropAddr{caMute, caScopeOutput, caElementMain}
	var v uint32
	if m {
		v = 1
	}
	if rc := a.setData(id, &addr, 0, nil, uint32(unsafe.Sizeof(v)), unsafe.Pointer(&v)); rc != 0 {
		return fmt.Errorf("CoreAudio write mute: %d", rc)
	}
	return nil
}

type coreAudioVolume struct {
	hal          coreAudioHAL
	uid          string
	mu           sync.Mutex
	cachedID     uint32
	volSel       uint32
	muteChecked  bool
	muteSettable bool
}

var _ playback.VolumeControl = (*coreAudioVolume)(nil)

// OpenVolumeControl returns a CoreAudio hardware volume controller for macOS
// exclusive CoreAudio outputs that expose writable device volume. mpv's
// coreaudio_exclusive driver does not implement ao-volume/ao-mute, so routing
// volume directly through CoreAudio keeps hardware volume control working while
// hog mode is held. Devices without writable hardware volume (and shared or
// non-CoreAudio outputs) return nil so mpv handles volume by default.
func OpenVolumeControl(device string, exclusive bool) playback.VolumeControl {
	uid, ok := coreAudioExclusiveUID(device, exclusive)
	if !ok {
		return nil
	}
	return openCoreAudioVolume(&systemCoreAudio, uid)
}

// VolumeProber returns a probe for exclusive CoreAudio targets so USB DACs
// connected after startup (such as headphone-activated dongles) attach to
// CoreAudio hardware volume automatically once they appear.
func VolumeProber(device string, exclusive bool) func() playback.VolumeControl {
	uid, ok := coreAudioExclusiveUID(device, exclusive)
	if !ok {
		return nil
	}
	return func() playback.VolumeControl {
		return openCoreAudioVolume(&systemCoreAudio, uid)
	}
}

func coreAudioExclusiveUID(device string, exclusive bool) (string, bool) {
	driver, uid, ok := strings.Cut(device, "/")
	if !ok || uid == "" || uid == "default" {
		return "", false
	}
	if driver == "coreaudio_exclusive" || (exclusive && driver == "coreaudio") {
		return uid, true
	}
	return "", false
}

func openCoreAudioVolume(hal coreAudioHAL, uid string) playback.VolumeControl {
	c := &coreAudioVolume{hal: hal, uid: uid}
	id, err := c.resolveLocked()
	if err != nil {
		return nil
	}
	if _, ok := c.volumeSelectorLocked(id); !ok {
		return nil
	}
	return c
}

func (c *coreAudioVolume) String() string {
	return "CoreAudio hardware (" + c.uid + ")"
}

func (c *coreAudioVolume) resetCacheLocked() {
	c.cachedID = 0
	c.volSel = 0
	c.muteChecked = false
	c.muteSettable = false
}

// Always verify the device UID before touching a numeric AudioDeviceID:
// unplugging and replugging a USB DAC can reassign IDs across devices.
func (c *coreAudioVolume) resolveLocked() (uint32, error) {
	if c.cachedID != 0 {
		if uid, ok := c.hal.deviceUID(c.cachedID); ok && uid == c.uid {
			return c.cachedID, nil
		}
		c.resetCacheLocked()
	}
	ids, err := c.hal.deviceIDs()
	if err != nil {
		return 0, err
	}
	for _, id := range ids {
		if uid, ok := c.hal.deviceUID(id); ok && uid == c.uid {
			c.cachedID = id
			return id, nil
		}
	}
	return 0, fmt.Errorf("CoreAudio device %q not found", c.uid)
}

func (c *coreAudioVolume) volumeSelectorLocked(id uint32) (uint32, bool) {
	if c.volSel != 0 {
		return c.volSel, true
	}
	if c.hal.isSettable(id, caVirtualMainVolume) {
		c.volSel = caVirtualMainVolume
		return c.volSel, true
	}
	if c.hal.isSettable(id, caVolumeScalar) {
		c.volSel = caVolumeScalar
		return c.volSel, true
	}
	return 0, false
}

func (c *coreAudioVolume) muteSettableLocked(id uint32) bool {
	if !c.muteChecked {
		c.muteSettable = c.hal.isSettable(id, caMute)
		c.muteChecked = true
	}
	return c.muteSettable
}

func (c *coreAudioVolume) Volume() (int, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	id, err := c.resolveLocked()
	if err != nil {
		return 0, err
	}
	sel, ok := c.volumeSelectorLocked(id)
	if !ok {
		return 0, playback.ErrUnsupported
	}
	v, err := c.hal.getFloat(id, sel)
	if err != nil {
		c.volSel = 0
		return 0, err
	}
	f := float64(v)
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return 0, errors.New("CoreAudio returned non-finite volume")
	}
	pct := int(math.Round(f * 100))
	if pct < 0 {
		pct = 0
	} else if pct > 100 {
		pct = 100
	}
	return pct, nil
}

func (c *coreAudioVolume) SetVolume(v int) error {
	if v < 0 || v > 100 {
		return errors.New("volume must be 0–100")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	id, err := c.resolveLocked()
	if err != nil {
		return err
	}
	sel, ok := c.volumeSelectorLocked(id)
	if !ok {
		return playback.ErrUnsupported
	}
	if err := c.hal.setFloat(id, sel, float32(v)/100.0); err != nil {
		c.volSel = 0
		return err
	}
	return nil
}

func (c *coreAudioVolume) Mute() (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	id, err := c.resolveLocked()
	if err != nil {
		return false, err
	}
	if !c.muteSettableLocked(id) {
		return false, playback.ErrUnsupported
	}
	m, err := c.hal.getMute(id)
	if err != nil {
		c.muteChecked = false
		return false, err
	}
	return m, nil
}

func (c *coreAudioVolume) SetMute(m bool) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	id, err := c.resolveLocked()
	if err != nil {
		return err
	}
	if !c.muteSettableLocked(id) {
		return playback.ErrUnsupported
	}
	if err := c.hal.setMute(id, m); err != nil {
		c.muteChecked = false
		return err
	}
	return nil
}
