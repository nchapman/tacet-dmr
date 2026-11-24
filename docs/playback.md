# Trying the playback engine

The local player mode is an audio-only way to test the renderer's output path; it starts no network listener. Use trusted tracks or URLs that you supply yourself. To control this output from Tacet, see [UPnP renderer mode](renderer.md).

## Build and run

Install Go (see `go.mod`) and a compatible **libmpv 2** native runtime (tested with mpv 0.41 on Ubuntu and 0.42 on macOS) for the machine running the player. Use the `nocgo` build tag to load it dynamically via the Go binding. The player and libmpv run in **one process**; neither an `mpv` executable nor a daemon is used. The library and its dependencies must be on the native loader's search path even for `-help` and `go test`.

- macOS: `brew install mpv`. If the dynamic loader cannot find it, set `DYLD_FALLBACK_LIBRARY_PATH="$(brew --prefix mpv)/lib"` when launching the binary.
- Linux: install the distribution's libmpv runtime (`libmpv2` on Debian-family systems); check the version. The build must include ALSA for direct hardware output.
- Windows: use a matching architecture libmpv development/runtime distribution, with its `libmpv-2.dll` (or `mpv-2.dll`) and dependencies next to `tacet-dmr.exe`.

```
go build -tags nocgo -o tacet-dmr ./cmd/tacet-dmr
./tacet-dmr -list-devices
./tacet-dmr ./track01.flac ./track02.flac
./tacet-dmr -interactive 'https://your-media-server.example/track.flac'
./tacet-dmr -device 'alsa/hw:CARD=DAC,DEV=0' -exclusive ./track01.flac
```

Use `tacet-dmr.exe` on Windows. Supply the exact `coreaudio/<UID>` or `wasapi/<endpoint>` identifier for your device on macOS or Windows. Linux direct `alsa/hw:` PCMs may need to be selected manually (`aplay -l`); `default`, `plughw`, PipeWire and PulseAudio are not accepted for the explicit exclusive request. Device auto-selection/shared output is fine without `-exclusive`. Arguments are individual tracks in playback order, not playlist files. In interactive mode, enter `play`, `pause`, `seek 1m20s`, `status`, or `stop`; Ctrl-C also stops playback.

Use `-volume N` to set an explicit initial volume (0–100). By default, libmpv handles volume in software (30 in exclusive mode, 100 in shared mode). On macOS exclusive CoreAudio outputs whose device exposes writable hardware volume, `tacet-dmr` controls the CoreAudio device volume directly while leaving libmpv at unity software gain (preserving the device's current level unless `-volume` is passed); fixed-output DACs without writable hardware volume fall back to libmpv software volume.

Tracks are queued on one libmpv handle before unpausing. `gapless-audio=weak` favors seamless same-format transitions without forcing later tracks to use the first track's rate. Position and format reports are from the engine, not measurements at the DAC. Native DSD, sample-perfect joins, and a physical device lock are **not** certified. In particular, requesting CoreAudio hog mode is not independent proof that the driver granted it. No software drift correction, group playback, or automatic fallback to silent output is configured.

## Verification

```
go vet -tags nocgo ./...
go test -tags nocgo ./...
```

Tests play generated PCM through **real libmpv** using its null output (no speakers needed), with an HTTP server including redirects, duplicate tracks, pause, seek, and error paths. On macOS, if `go test` doesn't forward your loader path, use `go test -tags nocgo -exec "env DYLD_FALLBACK_LIBRARY_PATH=$(brew --prefix mpv)/lib" ./...`.

For hardware qualification, check that another client cannot open the same DAC during exclusive playback and can open it again after stop. Verify the device switches from 44.1 to 96 kHz on mixed-rate tracks. Capture a continuous signal split into two same-format FLAC files and compare the frames across the join. Test each OS/driver combination; a cross-build or null-output test proves none of these hardware properties.

`internal/playback` is the backend-independent transport boundary; `internal/mpvplayer` implements it. A single caller owns a session and consumes its events before closing. UPnP and event subscription state live in `internal/renderer`, outside the backend. For distribution, review the native libmpv build and its dependencies; the Go binding's license alone does not cover them.
