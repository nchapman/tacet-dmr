# tacet-endpoint Reference

## CLI Flags

Both single-dash (`-flag`) and double-dash (`--flag`) forms are supported.

| Flag | Default | Description |
| --- | --- | --- |
| `-serve ADDR` | *(none)* | Bind endpoint HTTP server to a LAN IPv4:port or interface:port (e.g. `en0:49152`, `192.168.1.10:49152`). |
| `-room ROOM` | value of `-name` | OpenHome `Product:1` room name. |
| `-name NAME` | `Tacet Endpoint` | OpenHome device name (`friendlyName` on UPnP AV is `"<Name> (UPnP)"`). |
| `-controller CIDR` | *(allow any LAN peer)* | Optional trusted controller IPv4 address or CIDR allowlist (comma-separated, e.g. `192.168.1.0/24`). Gates both OpenHome and UPnP AV. |
| `-device ID` | `auto` | Output target: `auto`, `coreaudio/<uid>` (macOS), `alsa/<name>` (Linux), `wasapi/<id>` (Windows), `null`, or `pcm/<path>`. |
| `-exclusive` | `false` | Request exclusive hardware output (macOS CoreAudio hog mode + physical format switching, Linux ALSA `hw:`, Windows WASAPI Exclusive). |
| `-volume 0-100` | hardware level, else `30` (exclusive) / `100` (shared) | Initial volume level. |
| `-uuid UUID` | derived from hostname | Stable OpenHome UDN override (UPnP AV UDN is deterministically derived from it). |
| `-no-ssdp` | `false` | Disable UDP multicast SSDP announcements; use the printed `TACET_OPENHOME_URL` / `TACET_UPNP_URL`. |
| `-no-upnp` | `false` | Disable the UPnP AV / DLNA `MediaRenderer:1` compatibility device (run pure OpenHome). |
| `-no-sender` | `false` | Disable the Songcast `Sender:1` multi-room transmitter. |
| `-list-devices` | `false` | Print available output device IDs and exit. |
| `-version` | `false` | Print version and exit. |

## Startup Markers

When `-serve` starts, `tacet-endpoint` prints machine-readable lines to stdout:

- `TACET_OPENHOME_URL=http://<ip>:<port>/description.xml`
- `TACET_UPNP_URL=http://<ip>:<port>/upnp/description.xml` *(omitted with `-no-upnp`)*
- `TACET_SONGCAST_URL=ohz://...` or `ohu://<ip>:<port>` *(omitted with `-no-sender`)*

## Dual-Device Single-Port Architecture

A single HTTP server and a single UDP multicast `:1900` SSDP socket serve both devices with distinct UDNs:

### OpenHome Media (`urn:av-openhome-org:device:Source:1`)
- Device XML: `/description.xml`
- Services:
  - `urn:av-openhome-org:service:Product:1` — sources: `Playlist`, `Songcast` (`Receiver`), and `UPnP AV` (`UpnpAv`, when enabled)
  - `urn:av-openhome-org:service:Playlist:1` — device-side gapless playlist, repeat, shuffle
  - `urn:av-openhome-org:service:Info:1` — live codec, sample rate, bit depth, bitrate, lossless flag, and DIDL metadata
  - `urn:av-openhome-org:service:Time:1` — track count, duration, elapsed seconds
  - `urn:av-openhome-org:service:Volume:1` — shared hardware/software volume and mute
  - `urn:av-openhome-org:service:Receiver:1` — Songcast follower receiver (`ohz://` / `ohu://`)
  - `urn:av-openhome-org:service:Sender:1` — Songcast leader transmitter tapping pre-gain Q31 PCM from `tacet-audio` *(unless `-no-sender`)*

### UPnP AV / DLNA Compatibility (`urn:schemas-upnp-org:device:MediaRenderer:1`)
- Device XML: `/upnp/description.xml`
- Services:
  - `AVTransport:1` (`/upnp/AVTransport/{control,events,scpd.xml}`) — single-track + `SetNextAVTransportURI` gapless playback, pause, stop, seek (`REL_TIME`)
  - `RenderingControl:1` (`/upnp/RenderingControl/{control,events,scpd.xml}`) — shared volume/mute + `FactoryDefaults` preset
  - `ConnectionManager:1` (`/upnp/ConnectionManager/{control,events,scpd.xml}`) — honest `GetProtocolInfo` sink MIME list

## Source Arbitration & Shared Volume

- Starting playback via UPnP `AVTransport.Play` automatically switches OpenHome `Product:1` to the `UPnP AV` source, stops any active OpenHome `Playlist` or Songcast `Receiver`, and marks `Sender:1` `Disabled`.
- Starting OpenHome `Playlist.Play` or `Receiver.Play` automatically stops UPnP `AVTransport` (`STOPPED`) and restores OpenHome source ownership.
- Volume and mute changes from either OpenHome `Volume:1` or UPnP `RenderingControl:1` drive the single `tacet-audio` engine and emit GENA updates on both devices.
