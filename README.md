# tacet-endpoint

Turn any Mac, Linux box, or Windows PC (and its USB DAC) into a bit-exact **OpenHome Media + Songcast** network audio endpoint — with optional **UPnP AV / DLNA** compatibility on the same port.

Single native Rust binary. Zero `libmpv` or FFmpeg dependencies. Built for [Tacet](https://github.com/nchapman/tacet), Linn Kazoo, Lumin, BubbleUPnP, and any OpenHome or UPnP AV control point.

## Why tacet-endpoint?

- **Audiophile native output (`tacet-audio`)**: Pure-Rust `symphonia` decoding (FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Opus, Ogg, MP4/M4A, WebM) into a lock-free Q31 integer ringbuffer with bit-exact pass-through at unity gain or when hardware volume is active.
  - **macOS CoreAudio**: Shared mode (AUHAL) or true **exclusive hog mode** (`kAudioDevicePropertyHogMode`) with per-track physical sample-rate switching, native 3-byte packed 24-bit (`S24Packed`) for 24-bit-only USB DACs (e.g. AudioQuest DragonFly Black), and hardware volume/mute.
  - **Linux ALSA**: Direct `hw:` / `plughw:` / `default` (`S16_LE`, `S24_3LE`, `S24_LE`, `S32_LE`, `FLOAT_LE`).
  - **Windows WASAPI**: Exclusive and shared output (`S16`, `S24`, `S32`, `F32`) with `IAudioEndpointVolume`.
- **OpenHome Media + Songcast multi-room**: Powered by [`openhome-rs`](https://github.com/nchapman/openhome-rs) (`Product:1`, `Playlist:1`, `Info:1`, `Time:1`, `Volume:1`, `Receiver:1`, `Sender:1`). Device-side gapless playlists, live codec/bit-depth/sample-rate metadata, and Songcast leader/follower streaming with closed-loop PI clock-drift compensation.
- **Optional UPnP AV / DLNA compatibility**: Exposes `MediaRenderer:1` (`AVTransport:1`, `RenderingControl:1`, `ConnectionManager:1`) on `/upnp/description.xml` with its own UDN and `"<Name> (UPnP)"` friendly name, sharing volume and arbitrating the active source cleanly with OpenHome. Disable with `-no-upnp` for pure OpenHome.
- **Safe defaults**: Explicit `-serve` interface/IP binding, conservative `30%` startup volume in exclusive mode when software-controlled, optional `-controller` IPv4/CIDR allowlist gating both protocols, and network `file://` URI rejection.

## Quick start

### Build from source

Requires **Rust 1.89+** (on Linux, also `libasound2-dev` and `pkg-config`).

```sh
cargo build --release --bin tacet-endpoint
```

### 1. Find your audio device

```sh
./target/release/tacet-endpoint -list-devices
```

### 2. Start the endpoint

```sh
# macOS with a USB DAC in exclusive hog mode
./target/release/tacet-endpoint \
  -serve en0:49152 \
  -room "Living Room" \
  -name "Desk DAC" \
  -device "coreaudio/<device-uid>" \
  -exclusive
```

`tacet-endpoint` prints its active discovery URLs on startup and announces both devices over SSDP:

```text
TACET_OPENHOME_URL=http://192.168.1.10:49152/description.xml
TACET_UPNP_URL=http://192.168.1.10:49152/upnp/description.xml
TACET_SONGCAST_URL=ohz://239.255.255.250:51972/...
```

To restrict access to specific LAN hosts or subnets, pass `-controller`:

```sh
./target/release/tacet-endpoint \
  -serve en0:49152 \
  -controller 192.168.1.0/24 \
  -name "Desk DAC"
```

### Docker (Linux)

```sh
docker run --rm --net=host --device /dev/snd \
  ghcr.io/nchapman/tacet-endpoint:dev \
  -serve eth0:49152 \
  -room "Living Room" \
  -name "Linux DAC" \
  -device alsa/default
```

### Local playback

Without `-serve`, `tacet-endpoint` plays local audio files directly through `tacet-audio`:

```sh
./target/release/tacet-endpoint -device "coreaudio/<device-uid>" -exclusive track1.flac track2.flac
```

## Scope

- **Does**: Network audio playback (OpenHome `Playlist:1`, Songcast `Receiver:1` / `Sender:1`, UPnP `AVTransport:1`), gapless transitions, hardware/software volume control, and multi-room Songcast grouping.
- **Does not**: Host a media library / UPnP ContentDirectory server, or decode DSD/WMA/APE/WavPack (those MIME types are intentionally omitted from `GetProtocolInfo` so control points transcode automatically).

## Workspace

| Crate | Purpose |
| --- | --- |
| [`crates/tacet-audio`](crates/tacet-audio) | Reusable cross-platform audiophile playback engine (embeddable in Rust/Tauri apps). |
| [`crates/tacet-endpoint`](crates/tacet-endpoint) | OpenHome Media + Songcast + UPnP AV endpoint library and standalone CLI binary. |

## Documentation

- [`docs/renderer.md`](docs/renderer.md) — CLI flags, startup markers, service endpoints, and source arbitration.
- [`docs/playback.md`](docs/playback.md) — `tacet-audio` pipeline, sample formats, hardware volume, and Songcast hooks.
- [`docs/testing.md`](docs/testing.md) — Running the Rust unit/integration suites and Python `async_upnp_client` interop tests (`sh scripts/check.sh`).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
