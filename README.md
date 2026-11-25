# tacet-dmr

A self-contained UPnP/LinkPlay audio renderer written in Rust. Exposes a local audio output (built-in speakers, USB DAC, ALSA `hw:`, or WASAPI endpoint) on your LAN so [Tacet](https://github.com/nchapman/tacet) — or any standard UPnP AV / LinkPlay control point — can discover it and play to it gaplessly.

## Workspace

- **`crates/tacet-audio`**: Reusable cross-platform audiophile playback engine (pure-Rust `symphonia` decoding, lock-free `rtrb` PCM ringbuffer, gapless track transitions, and native macOS CoreAudio HAL/AUHAL, Linux ALSA, and Windows WASAPI sinks). Designed for both `tacet-dmr` and direct in-process embedding in Tauri apps.
- **`crates/tacet-dmr`**: UPnP/LinkPlay renderer library (`AVTransport:1`, `RenderingControl:1`, `ConnectionManager:1`, `PlayQueue:1`, GENA eventing, UDP multicast SSDP) and standalone CLI binary.

## Features

- **Zero external media-player dependencies**: Single native binary with pure-Rust decoding for FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Ogg, MP4/M4A, and WebM/Matroska (no `libmpv` or FFmpeg install required).
- **Native audiophile output backends**:
  - **macOS CoreAudio**: Shared mode via AUHAL, and true **exclusive hog mode** (`kAudioDevicePropertyHogMode`) via direct HAL `AudioDeviceCreateIOProcID` with per-track physical sample-rate switching, **native 3-byte packed 24-bit (`S24Packed`) support** for 24-bit-only USB DACs (such as the AudioQuest DragonFly Black), and native CoreAudio hardware volume/mute.
  - **Linux ALSA**: Direct `hw:` / `plughw:` / `default` output supporting `S16_LE`, `S24_3LE`, `S24_LE`, `S32_LE`, and `FLOAT_LE`.
  - **Windows WASAPI**: Exclusive and shared output (`S16`, `S24`, `S32`, `F32`) with `IAudioEndpointVolume`.
- **Full single-room UPnP / LinkPlay surface**: All 4 services, all 6 LinkPlay `LoopMode` (`0..5`) / UPnP `PlayMode` modes, streaming `BrowseQueue`, GENA `SUBSCRIBE`/`NOTIFY`, and native SSDP discovery.
- **Safety by default**: Explicit `-serve` LAN IP/interface binding, required `-controller` IPv4/CIDR allowlist, and conservative exclusive startup volume (`30` when software-controlled).

## Quick start

```sh
# Build from source (requires Rust 1.85+; on Linux also libasound2-dev + pkg-config)
cargo build --release --bin tacet-dmr

# List available audio outputs
./target/release/tacet-dmr -list-devices

# Run as a UPnP / LinkPlay renderer on macOS with a USB DAC in exclusive hog mode
./target/release/tacet-dmr \
  -serve en0:49152 \
  -controller 192.168.1.0/24 \
  -name "Desk DAC" \
  -device "coreaudio/<device-uid>" \
  -exclusive
```

## Docker (Linux)

```sh
docker run --rm --net=host --device /dev/snd \
  ghcr.io/nchapman/tacet-dmr:dev \
  -serve eth0:49152 \
  -controller 192.168.1.0/24 \
  -name "Linux DAC" \
  -device alsa/default
```

## Testing

```sh
# Full local check: clippy, unit tests, 15-test Python UPnP/LinkPlay interop suite,
# and (if ../tacet is present) unmodified sibling Tacet queue peer test
DMR_TEST_IP="$(ipconfig getifaddr en0)" sh scripts/check.sh
```

See [`docs/renderer.md`](docs/renderer.md) for CLI flags, protocol details, and architecture notes.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
