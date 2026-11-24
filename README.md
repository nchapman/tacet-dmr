# tacet-dmr

Play music from [Tacet](https://github.com/nchapman/tacet) on a DAC attached to another computer. `tacet-dmr` is a single-process Go UPnP audio renderer with an embedded libmpv playback engine. It supports Tacet’s native LinkPlay queue, one-track lookahead, volume, seek, and an optional exclusive-output request. It does not implement synchronized multiroom playback.

## Native binary

Download the archive for your OS and CPU from [Releases](https://github.com/nchapman/tacet-dmr/releases). CI also attaches `tacet-dmr-binaries` to successful [workflow runs](https://github.com/nchapman/tacet-dmr/actions). Archives include the executable and documentation, **not libmpv**. Install a matching-architecture libmpv 2 runtime (mpv 0.41 or 0.42); see [platform setup](docs/playback.md). Verify the download using the accompanying `SHA256SUMS`.

- **macOS:** `darwin-arm64` for Apple Silicon, `darwin-amd64` for Intel. With Homebrew mpv, use `export DYLD_FALLBACK_LIBRARY_PATH="$(brew --prefix mpv)/lib"` if needed.
- **Linux:** `linux-amd64` or `linux-arm64` for glibc-based systems. Ubuntu 26.04 provides `libmpv2`.
- **Windows:** `windows-amd64` or `windows-arm64`. Put the matching libmpv DLL and its dependencies next to `tacet-dmr.exe`.

On the renderer machine, replace **both example addresses** with your own: `-serve` binds to the renderer’s LAN IPv4 address, and `-controller` allows the Tacet server’s IPv4 address. On `main` you can instead pass a trusted LAN subnet, e.g. `-controller 192.168.1.0/24`, to let any host on that subnet control playback.

```sh
./tacet-dmr -list-devices
./tacet-dmr -serve 192.168.1.32:49152 -controller 192.168.1.10 -name 'Study DAC'
```

Select **Study DAC** in Tacet. Add `-device` with an ID from the first command to select a specific output. Use only on a trusted LAN; this is not an authenticated internet service.

## Linux container

The image includes libmpv and supports `linux/amd64` and `linux/arm64`. The `dev` tag tracks `main`; versioned releases get version tags. If a newly created GHCR package is not yet public, authenticate or build locally with `docker build -t tacet-dmr .`.

```sh
# Native Linux host; pick the control node for the sound card you want.
image=ghcr.io/nchapman/tacet-dmr:dev
sound_gid=$(stat -c '%g' /dev/snd/controlC0)
docker run --rm --device /dev/snd --group-add "$sound_gid" "$image" -list-devices

docker run --rm --name tacet-dmr --hostname study-dac --network host \
  --device /dev/snd --group-add "$sound_gid" "$image" \
  -serve 192.168.1.32:49152 -controller 192.168.1.10 \
  -device 'alsa/hw:CARD=DAC,DEV=0' -name 'Study DAC'
```

Replace the ALSA ID with your device’s ID. Linux host networking lets SSDP multicast, SOAP, and callbacks use the real LAN interface. The image runs as a non-root user; `--privileged` is not required. For a silent test omit the sound-device and group flags and use `-device 'null/'`. Docker Desktop is not a substitute for native CoreAudio or WASAPI: use the native binary on macOS or Windows.

## Development

```sh
sh scripts/check.sh        # vet, race tests, real libmpv and HTTP interoperability
sh scripts/package.sh      # six archives plus SHA256SUMS in dist/ (needs zip and tar)
```

[Renderer setup and capabilities](docs/renderer.md) · [Playback and hardware qualification](docs/playback.md) · [Test suite](docs/testing.md). Pushing a `vMAJOR.MINOR.PATCH` tag (optionally with a prerelease suffix) runs CI, publishes the multi-platform GHCR image, and attaches the verified archives to a GitHub Release.
