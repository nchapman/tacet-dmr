# tacet-endpoint

A self-contained **OpenHome Media + Songcast** audiophile network audio endpoint written in Rust — with an optional **UPnP AV / DLNA `MediaRenderer:1`** compatibility device on the same port. Exposes a local audio output (built-in speakers, USB DAC, ALSA `hw:`, or WASAPI endpoint) on your LAN so [Tacet](https://github.com/nchapman/tacet), Linn Kazoo, Lumin, BubbleUPnP, or any OpenHome / UPnP AV control point can discover it, play gapless playlists, and group rooms via Songcast.

## Workspace

- **`crates/tacet-audio`**: Reusable cross-platform audiophile playback engine (pure-Rust `symphonia` decoding including Opus, lock-free `rtrb` Q31 integer PCM ringbuffer, gapless track transitions, direct PCM stream input for Songcast Receiver, pre-gain PCM tap for Songcast Sender, and native macOS CoreAudio HAL/AUHAL, Linux ALSA, and Windows WASAPI sinks). Designed for both `tacet-endpoint` and direct in-process embedding in Tauri apps.
- **`crates/tacet-endpoint`**: OpenHome Media + Songcast + minimal UPnP AV endpoint built on [`openhome-rs`](https://github.com/nchapman/openhome-rs) (`Product:1`, `Playlist:1`, `Info:1`, `Time:1`, `Volume:1`, `Receiver:1`, `Sender:1`, plus optional UPnP `AVTransport:1`, `RenderingControl:1`, `ConnectionManager:1`, dual-device GENA eventing, and dual-device SSDP).

## Features

- **Primary OpenHome Media + Songcast stack**:
  - Device-side `Playlist:1` queue with true gapless transitions, repeat, and shuffle.
  - Multi-room **Songcast** `Sender:1` (tapping pre-gain unity Q31 PCM from `tacet-audio`) and `Receiver:1` (with proportional-integral clock-drift resampling directly into `tacet-audio`'s exclusive/shared output stream).
  - Real-time GENA eventing for all OpenHome services and live `Info:1` / `Time:1` audio metadata (`BitDepth`, `SampleRate`, `BitRate`, `CodecName`, `Lossless`).
- **Minimal UPnP AV / DLNA compatibility device** (enabled by default, disable with `-no-upnp`):
  - Exposes `MediaRenderer:1` (`AVTransport:1`, `RenderingControl:1`, `ConnectionManager:1`) on `/upnp/description.xml` with its own UDN and `"<Name> (UPnP)"` friendly name so dual-stack control points never collide.
  - Shares hardware/software volume and mutually arbitrates the active audio source with OpenHome (`Product:1` `UpnpAv` source).
- **Zero external media-player dependencies**: Single native binary with pure-Rust decoding for FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Opus, Ogg, MP4/M4A, and WebM/Matroska (no `libmpv` or FFmpeg install required).
- **Native audiophile output backends**:
  - **macOS CoreAudio**: Shared mode via AUHAL, and true **exclusive hog mode** (`kAudioDevicePropertyHogMode`) via direct HAL `AudioDeviceCreateIOProcID` with per-track physical sample-rate switching, **native 3-byte packed 24-bit (`S24Packed`) support** for 24-bit-only USB DACs (such as the AudioQuest DragonFly Black), and native CoreAudio hardware volume/mute.
  - **Linux ALSA**: Direct `hw:` / `plughw:` / `default` output supporting `S16_LE`, `S24_3LE`, `S24_LE`, `S32_LE`, and `FLOAT_LE`.
  - **Windows WASAPI**: Exclusive and shared output (`S16`, `S24`, `S32`, `F32`) with `IAudioEndpointVolume`.
- **Safety by default**: Explicit `-serve` LAN IP/interface binding, optional `-controller` IPv4/CIDR allowlist gating both OpenHome and UPnP AV, and conservative exclusive startup volume (`30` when software-controlled).

## Quick start

```sh
# Build from source (requires Rust 1.89+; on Linux also libasound2-dev + pkg-config)
cargo build --release --bin tacet-endpoint

# List available audio outputs
./target/release/tacet-endpoint -list-devices

# Run on macOS with a USB DAC in exclusive hog mode
./target/release/tacet-endpoint \
  -serve en0:49152 \
  -room "Living Room" \
  -name "Desk DAC" \
  -device "coreaudio/<device-uid>" \
  -exclusive
```

## Docker (Linux)

```sh
docker run --rm --net=host --device /dev/snd \
  ghcr.io/nchapman/tacet-endpoint:dev \
  -serve eth0:49152 \
  -room "Living Room" \
  -name "Linux DAC" \
  -device alsa/default
```

## Testing

```sh
# Full local check: clippy, Rust unit + integration tests (OpenHome, Songcast, UPnP AV),
# and Python black-box async_upnp_client interop suite
sh scripts/check.sh
```

See [`docs/renderer.md`](docs/renderer.md) for CLI flags, protocol details, and architecture notes.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
