# tacet-dmr Renderer Reference

## CLI Flags

| Flag | Default | Description |
| --- | --- | --- |
| `-serve ADDR` | *(none)* | Host UPnP/LinkPlay renderer on a LAN IPv4:port or interface:port (e.g. `en0:49152`, `192.168.1.10:49152`). |
| `-controller CIDR` | *(required with `-serve`)* | Trusted controller IPv4 address or CIDR allowlist (comma-separated, e.g. `192.168.1.0/24`). |
| `-name NAME` | `Tacet Renderer` | UPnP `friendlyName`. |
| `-device ID` | `auto` | Output target: `auto`, `coreaudio/<uid>` (macOS), `alsa/<name>` (Linux), `wasapi/<id>` (Windows), `null`, or `pcm/<path>`. |
| `-exclusive` | `false` | Request exclusive hardware output (macOS CoreAudio hog mode + physical format switching, Linux ALSA `hw:`, Windows WASAPI Exclusive). |
| `-volume 0-100` | hardware level, else `30` (exclusive) / `100` (shared) | Initial volume level. |
| `-uuid UUID` | derived from hostname | Stable UPnP UDN override. |
| `-no-ssdp` | `false` | Disable UDP multicast SSDP announcements; use the printed `TACET_RENDERER_URL`. |
| `-list-devices` | `false` | Print available output device IDs and exit. |
| `-version` | `false` | Print version and exit. |

## Audio Engine (`tacet-audio`)

- **Decoding**: Pure-Rust `symphonia` (FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Ogg, MP4/M4A, WebM/Matroska) into left-aligned Q31 `i32` PCM with gapless trimming enabled.
- **Bit-exact integer path**: At unity volume (`100`, unmuted) or when hardware volume is active, `S16`, `S24Packed` (3-byte LE), `S24In32Left`, `S24In32Right`, and `S32` samples pass from decoder to DMA buffer in the integer domain without floating-point conversion.
- **Gapless transitions**:
  - Same sample rate and channel count: Track B streams directly into the same lock-free `rtrb` ringbuffer with zero gap and zero device reopen; `TrackChanged` fires on the exact frame boundary.
  - Different sample rate or channel count: Track A drains completely, then the output stream reconfigures hardware sample rate/format for Track B.
- **Honest `GetProtocolInfo`**: Only containerized MIME types natively decoded by `tacet-audio` are advertised; unsupported codecs (e.g. raw `audio/L16`, Opus, DSF, WavPack, APE, WMA) are omitted so control points transcode automatically.

## UPnP / LinkPlay Services

- `AVTransport:1` (`/AVTransport/control`, `/AVTransport/events`, `/AVTransport/scpd.xml`)
- `RenderingControl:1` (`/RenderingControl/control`, `/RenderingControl/events`, `/RenderingControl/scpd.xml`)
- `ConnectionManager:1` (`/ConnectionManager/control`, `/ConnectionManager/events`, `/ConnectionManager/scpd.xml`)
- `PlayQueue:1` (`urn:schemas-wiimu-com:service:PlayQueue:1`, `/PlayQueue/control`, `/PlayQueue/scpd.xml` — no event endpoint, matching LinkPlay hardware)
- Full `LoopMode` `0..5` ↔ `PlayMode` mapping (`REPEAT_ALL`, `REPEAT_ONE`, `SHUFFLE`, `SHUFFLE_NOREPEAT`, `NORMAL`, `SHUFFLE_REPEAT_ONE`).
