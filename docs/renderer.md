# Use tacet-dmr from Tacet

Renderer mode exposes one audio output to **a trusted Tacet server or LAN subnet** over IPv4. It is opt-in: running with a file argument still starts only the local player. [Build and native runtime setup](playback.md).

For example, if the **renderer** is `192.168.1.32` and the **Tacet server** is `192.168.1.10`, start this on the renderer:

```sh
./tacet-dmr -serve 192.168.1.32:49152 -controller 192.168.1.10 -name 'Study DAC'
```

You can also pass a network interface name instead of a fixed IP, such as `-serve en0:49152` or `-serve eth0:49152`, so DHCP address changes do not require editing a `launchd` or `systemd` service; `tacet-dmr` resolves the interface's LAN IPv4 address (preferring private over link-local) and refuses down or non-LAN interfaces.

Add `-device '<ID from -list-devices>' -exclusive` to request exclusive output. To test silently, use `-device 'null/'` instead. The output opens on Play and is released on Stop, replacement, final EOF, or playback failure. Operational events (track starts with query strings stripped, output driver and sample-rate changes, hardware volume attach/disconnect/reconnect, and playback/volume errors) are logged to stderr; stdout prints only the startup summary. `-name` is a display label; by default the device UUID derives from the machine hostname, not its IP, name flag or port. Set a fixed `-uuid` for machines that change hostname or when running multiple renderer instances on one host.

Select **Study DAC** in Tacet and play a track. No Tacet source change is required. SSDP announces the description and responds to `M-SEARCH` on the interface owning the listen IP. Permit UDP multicast `239.255.255.250:1900` and inbound TCP to the chosen port. To bypass multicast, add `-no-ssdp` and set **on the Tacet server**:

```sh
TACET_RENDERER_URL=http://192.168.1.32:49152/description.xml
```

Use the printed address. Tacet's URL validator requires a private LAN IP, **even when both processes run on the same host**. Choose that same LAN IP for `-controller` in that case. The controller address is the source IP as seen by the renderer (watch for NAT/containers). To allow any controller on a trusted subnet, pass its network address instead, e.g. `-controller 192.168.1.0/24`. Every host in that subnet can control playback and volume. Event callbacks must be reachable at the individual subscriber’s source IP. The renderer must be able to fetch the media URL Tacet supplies, including HTTP(S) redirects to a CDN.

## What works in renderer mode

- **AVTransport:1**: set/clear an HTTP(S) track and its next URI with DIDL metadata; play, pause, stop, time seek, next and previous. Query media, position, available transport actions, and play mode. `SetPlayMode` accepts `NORMAL`, `REPEAT_ONE` and `REPEAT_ALL`. Playback errors are reported via `TransportStatus`; seek depends on a seekable source.
- **RenderingControl:1**: volume (0–100) and mute on `Master`. By default, libmpv handles volume and mute in software (starting at 30 with `-exclusive`, 100 otherwise, or an explicit `-volume 0..100`). On macOS exclusive CoreAudio outputs (`coreaudio/<UID>` with `-exclusive`, or `coreaudio_exclusive/<UID>`), mpv's exclusive driver does not expose `ao-volume`, so `tacet-dmr` uses the device's CoreAudio hardware volume (and hardware mute when supported) directly while keeping libmpv at unity software gain (100). Hardware-controlled devices preserve their current level at startup unless `-volume` is given, reflect external OS/device volume changes via GENA `LastChange`, attach automatically if a USB DAC is plugged in after startup (and reconnect across unplug/replug by device UID), and fall back to libmpv software volume when the device has no writable hardware control. `FactoryDefaults` restores the startup level and unmutes; track changes and Stop/Play retain the current setting.
- **ConnectionManager:1**: sink protocol and static connection information. Conservative HTTP audio MIME list; no native DSD claim.
- **GENA**: subscribe, initial notification, subsequent state changes, renewal and unsubscribe. Position is polled rather than pushed every second.

## Native queue and track transitions

Tacet detects `urn:schemas-wiimu-com:service:PlayQueue:1` in the device description and switches to its native queue controller. It sends a named list, appending batches for long queues. The renderer owns one in-memory queue, advances on natural EOF without waiting for Tacet, and reports its current index even when two entries have the same URL. Tacet owns shuffle order. These are the wire actions implemented:

| PlayQueue action | Behavior |
| --- | --- |
| `CreateQueue`, `AppendTracksInQueue` | Replace (initially stopped) or append validated HTTP(S) tracks and their DIDL metadata. |
| `PlayQueueWithIndex`, `GetQueueIndex` | Play or report a one-based index by list name or `CurrentQueue`. An empty list reports 0. |
| `MoveTracksInQueue`, `RemoveTracksInQueue` | Move one entry or remove a one-based inclusive range. Reordering or deleting other entries preserves current playback. Removing the current entry stops it and selects its successor, or the last remaining entry; Tacet explicitly plays its chosen replacement. |
| `BrowseQueue` | `CurrentQueue` or the list name returns a `PlayList` with ordered `Track1`…`TrackN`, URL and escaped DIDL. `TotalQueue` is reserved for the inventory (zero or one named list). |
| `SetQueueLoopMode`, `GetQueueLoopMode` | `0` repeats all, `1` repeats one, `4` stops at the end. Manual Next/Previous ignore repeat-one. Playback errors stop instead of starting a repeat loop. |

A normal UPnP controller can use `SetAVTransportURI` to replace the native queue and `SetNextAVTransportURI` to set, replace or clear the following track. Setting just the next URI while a named queue is active returns a transport fault rather than discarding the queue. Stop preserves the selection; process restart clears it.

Both modes stage one upcoming entry in the current libmpv session. `GetQueueIndex.PreloadingIndex` identifies that entry (one-based; 0 if none), not a promise that its download is complete. The current index and metadata change when the successor becomes playable, not when it is queued. If it fails to open, playback stops on the previous selection with `ERROR_OCCURRED`.

The [PCM capture test](testing.md) confirms a sample-continuous join for two same-format HTTP WAV tracks. Network stalls, very short tracks, codec padding, and device reconfiguration can still interrupt output. Mixed-rate playback lets the audio output change rate rather than forcing resampling to conceal the transition. No DAC timing or bit-perfect claim.

Limits: 10,000 entries, 32 MiB of retained URI/metadata, and a 4 MiB PlayQueue SOAP request. Invalid batches are rejected without partially changing the queue. The vendor service has no event endpoint: Tacet polls its index, while standard AVTransport GENA reports track changes. This implements the queue API Tacet uses, **not** the WiiM HTTP API, multiroom grouping, synchronized playback or DLNA certification.

See the [compatibility suite and hardware reference](testing.md) for the full test command, exact service coverage, and the read-only device profiler.

## Trust boundary and checks

This is a LAN protocol, without TLS or controller authentication. Use a trusted network and firewall; don't port-forward it. SSDP identity is visible to that multicast segment. HTTP requests must come from the configured controller address or subnet and use the advertised IP:port. A trusted controller can make the renderer fetch remote audio and change the output volume. Only submit trusted audio. Event callbacks must be HTTP URLs with the controller's literal IP; no callback redirects.

The automated tests exercise the generated `goupnp/dcps/av1` control point against the HTTP server and a real libmpv null output. For an optional real SSDP search test on a host with multicast-capable loopback, run `TACET_DMR_TEST_SSDP=1 go test ./internal/renderer -run TestLoopbackDiscovery -count=1`. To repeat the native-queue interoperability test on macOS, run `DMR_TEST_IP=<this-host-private-IPv4> DMR_TEST_LIB="$(brew --prefix mpv)/lib" sh scripts/test-tacet.sh`. It uses a Go test overlay against `../tacet` (override with `TACET_SRC`), an ephemeral same-host renderer with SSDP off, and generated WAV audio on null output. It does not edit Tacet. Test audible playback, discovery across your LAN, and DAC exclusivity separately.
