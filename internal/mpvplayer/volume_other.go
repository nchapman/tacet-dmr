//go:build !darwin

package mpvplayer

import "github.com/nchapman/tacet-dmr/internal/playback"

// OpenVolumeControl returns a platform-specific volume controller when mpv's
// audio backend cannot manage output volume directly, or nil to let mpv handle
// volume and mute by default.
func OpenVolumeControl(string, bool) playback.VolumeControl { return nil }
