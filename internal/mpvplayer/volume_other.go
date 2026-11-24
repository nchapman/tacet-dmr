//go:build !darwin

package mpvplayer

import "github.com/nchapman/tacet-dmr/internal/playback"

// OpenVolumeControl returns a platform-specific volume controller when mpv's
// audio backend cannot manage output volume directly, or nil to let mpv handle
// volume and mute by default.
func OpenVolumeControl(string, bool) playback.VolumeControl { return nil }

// VolumeProber returns a hotplug probe function for targets that may acquire a
// platform hardware volume control after startup, or nil when mpv should handle
// volume permanently.
func VolumeProber(string, bool) func() playback.VolumeControl { return nil }
