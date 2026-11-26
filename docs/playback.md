# Playback Engine (`tacet-audio`)

`tacet-audio` is a self-contained Rust audio playback engine used by `tacet-endpoint` and designed for direct in-process embedding in Rust/Tauri desktop applications.

## Build Requirements

- **macOS**: Rust 1.89+ (links Apple `CoreAudio` and `AudioToolbox` system frameworks; no Homebrew/mpv runtime required).
- **Linux**: Rust 1.89+, `libasound2-dev`, and `pkg-config` at build time (`libasound2` at runtime).
- **Windows**: Rust 1.89+ (links Win32 WASAPI system APIs; no external DLLs required).

```sh
cargo build --release --bin tacet-endpoint
```

## Output Backends & Sample Formats

Internally the pipeline carries interleaved left-aligned Q31 `i32` PCM from `symphonia` (including FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Opus, Ogg, MP4/M4A, WebM/Matroska) or live Songcast PCM streams to a lock-free `rtrb` SPSC ringbuffer, keeping integer tracks in the integer domain all the way to the output buffer:

- **macOS (`coreaudio/<uid>`)**:
  - Shared mode (`-exclusive` off): `AUHAL` (`kAudioUnitSubType_HALOutput`) with `f32` interleaved audio so macOS handles system mixing and resampling.
  - Exclusive mode (`-exclusive` on): Direct CoreAudio HAL (`AudioDeviceCreateIOProcID`) acquiring `kAudioDevicePropertyHogMode` and switching `kAudioStreamPropertyPhysicalFormat` per track. Natively packs `S16`, **`S24Packed` (3-byte LE for 24-bit-only USB DACs such as the AudioQuest DragonFly Black)**, `S24In32Left`, `S24In32Right`, `S32`, and `F32`.
  - Hardware volume/mute via CoreAudio HAL (`kAudioDevicePropertyVolumeScalar`, `kAudioDevicePropertyMute`) with unity software pass-through.
- **Linux (`alsa/<name>`)**: Direct `hw:` / `plughw:` / `default` output supporting `S16`, `S24Packed` (`S24_3LE`), `S24In32Right` (`S24_LE`), `S32`, and `F32`.
- **Windows (`wasapi/<id>`)**: Exclusive and shared WASAPI (`IAudioClient`) with `IAudioEndpointVolume`.
- **Headless / Test**: `null` (real-time paced) and `pcm/<path>` (writes packed `S16LE` PCM to disk).

## Volume, Gapless, and Songcast Hooks

- When hardware volume is available, `tacet-audio` controls the device directly and leaves PCM at exact unity pass-through. Otherwise it applies integer Q16 gain (defaulting to `30` in exclusive mode, `100` in shared mode, or `-volume N`).
- Same-rate, same-channel successor tracks transition sample-accurately inside the same ringbuffer without reopening the audio device. Tracks with a different sample rate or channel count drain cleanly and reconfigure the output stream for the new format.
- **`Player::set_pcm_tap`** delivers unity-gain Q31 PCM frames prior to software volume scaling so Songcast `Sender:1` rebroadcasts full-scale audio while each room controls its own local volume independently.
- **`Player::open_pcm`** opens a direct push-based PCM stream into the same native hardware backend (including CoreAudio exclusive hog mode, ALSA `hw:`, and WASAPI exclusive) for Songcast `Receiver:1` playback with PI-controlled clock-drift resampling (`DriftResampler`).
