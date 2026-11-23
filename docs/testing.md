# Renderer compatibility suite

```sh
sh scripts/check.sh
```

Requires Go (and a working race detector), [libmpv setup](playback.md), `uv`, and Python ≥3.11 (uv can provision it). First use downloads the locked test dependencies. On macOS the runner locates libmpv with `brew --prefix mpv`, or uses the supplied `DMR_TEST_LIB`. Python and the test clients are **not renderer runtime dependencies**.

The command runs `go vet`, Go race tests, builds the CLI, and runs the independent Python control point against fresh CLI processes on loopback. Every mutation test targets a renderer the suite starts itself, with generated seekable WAV audio and silent `null/` or PCM file output. No running Tacet, speakers, cloud service, or audible output is required. Two extra integration paths are opt-in:

```sh
# Real M-SEARCH on multicast-capable loopback (not the physical LAN).
TACET_DMR_TEST_SSDP=1 sh scripts/check.sh
# Also run unmodified Tacet's queue client against a same-host, IP-allowlisted test renderer.
DMR_TEST_IP=YOUR_OWN_PRIVATE_IPV4 TACET_DMR_TEST_SSDP=1 sh scripts/check.sh
```

`TACET_SRC` overrides the default `../tacet` checkout for the second path. Without these flags, ordinary CI still exercises description discovery, SOAP, eventing, audio, and the full action contract; it does not test multicast delivery. `go test -tags nocgo ./...` by itself runs the Go tests, **not** the Python suite.

## What “covered” means

| Layer | Oracle | What it proves |
| --- | --- | --- |
| Reference codec | Redacted hardware signatures + `async-upnp-client` | All 88 observed action signatures can be loaded from descriptions, serialized with ordered/typed inputs and decoded from synthetic responses. No hardware action is dispatched by this test. |
| Renderer wire behavior | Independent, strict `async-upnp-client` running outside the renderer process | Every advertised action must succeed in a behavioral scenario. The suite checks the action set, ordered signatures, decoded results, queue identity with duplicate URIs, escaped DIDL, time seek, output controls, and typed SOAP faults. Advertising a new action without a scenario fails the suite. |
| GENA + SSDP | Real notification server, real HTTP and optional real UDP | Subscribe, initial event, renewal on same SID, LastChange XML, plain ConnectionManager property sets, subsequent updates and unsubscribe. Go additionally checks sequence numbers and callback restrictions. Optional Python discovery follows the actual `LOCATION` through to SOAP. |
| Other controllers | `goupnp/dcps/av1`, generated `wiimplay/upnp`, optional sibling Tacet | Standard AV control; older typed WiiM `GetQueueIndex` decoder; Tacet's named queues, batching beyond 50, moves/removals, indexing and repeat-all. |
| Backend and edge cases | Go unit and libmpv integration tests | Natural EOF vs error/stop, all repeat modes, queue edits without restarting the selected item, media redirects, rejected inputs and playback lifecycle. |

### Gapless acceptance test

`internal/mpvplayer/gapless_test.go` generates a continuous 32,000-frame mono 8 kHz, 16-bit waveform, splits it at frame 13,579 and serves the pieces over HTTP. For both the native queue and AVTransport next URI, the decoded PCM file must equal the entire original byte-for-byte, including across the redirected second URL. Before lookahead, reopening the PCM output left only the final 18,421 frames. A 404 successor must instead leave exactly the first 13,579 frames, its index, and error status. The PCM output is unclocked: this detects dropped, duplicated or inserted samples, not wall-clock delay or hardware behavior. A separate clocked null-output test checks an 8→16 kHz rate change.

## Hardware reference and sources

An AVTransport:1 SSDP search located **two WiiM Ultra Receivers and one Audio Pro C10** advertising Wiimu PlayQueue. All three exposed the same ordered action/argument/type inventory. [`interop/testdata/wiim-ultra.json`](../interop/testdata/wiim-ultra.json) contains only this structural data: no locations, friendly names, identifiers, queue entries, metadata, credentials or SOAP values.

| Service | Actions advertised by those devices | Actions exercised against `tacet-dmr` |
| --- | ---: | ---: |
| AVTransport:1 | 19 | 15 |
| RenderingControl:1 | 22 | 6 |
| ConnectionManager:1 | 3 | 3 |
| Wiimu PlayQueue:1 | 34 | 9 |
| Tencent QPlay:1 | 10 | 0 (not advertised) |

In addition to fetching descriptions, we called only `GetQueueLoopMode` and `GetQueueIndex(CurrentQueue)` on these speakers. Loop mode returned on all three. One Ultra's index reply contained `CurrentIndex`, `PreloadingIndex`, `CurrentPage`, `TrackNums`; the other Ultra and the C10 returned SOAP fault 501. So an advertised action does not guarantee success in every device state. For its own single-page queue, `tacet-dmr` reports the staged successor's one-based index (0 for none), 1 for a nonempty page (0 for empty), and the actual track count. Those are renderer semantics, not inferred universal firmware defaults.

To refresh a reference for an **explicitly selected** speaker:

```sh
cd interop
uv run --locked python profile_device.py http://SPEAKER_LAN_IPV4:PORT/description.xml --query > /tmp/speaker-actions.json
diff -u testdata/wiim-ultra.json /tmp/speaker-actions.json
```

This uses the same dynamic control point with a restricted HTTP transport: literal private IPv4, same origin, bounded XML responses, no proxy or redirect, and fresh connections for embedded servers. Without `--query` it only GETs XML descriptions. With the flag it also permits the two exact read-only queue getters. It never subscribes or changes playback on a physical speaker. Inspect differences before updating the fixture; new firmware is evidence, not an instruction to advertise an unimplemented feature.

Primary references:

- [UPnP Device Architecture](https://upnp.org/specs/arch/UPnP-arch-DeviceArchitecture-v1.0.pdf): SSDP, description and SCPD resolution, SOAP faults, GENA subscription lifecycle and sequencing.
- UPnP AV v1 templates: [MediaRenderer](https://upnp.org/specs/av/UPnP-av-MediaRenderer-v1-Device.pdf), [AVTransport](https://upnp.org/specs/av/UPnP-av-AVTransport-v1-Service.pdf), [RenderingControl](https://upnp.org/specs/av/UPnP-av-RenderingControl-v1-Service.pdf), [ConnectionManager](https://upnp.org/specs/av/UPnP-av-ConnectionManager-v1-Service.pdf). The AV:1 URI state variables are `string`. Standard `GetMediaInfo` does **not** require the hardware's extra `TrackSource` and `PreSwitchMode` outputs.
- Wiimu SOAP extensions: the three device SCPDs and read-only replies above; Tacet's `internal/linkplay/renderer.go` for QueueContext, batching and index semantics; [wiimplay's generated service client](https://pkg.go.dev/github.com/shumatech/wiimplay/upnp) as an older independent witness. The older removal signature omits `Action`; these three devices advertise it as a string. The tests do not treat all firmware generations as identical.
- [WiiM HTTP API v1.2](https://www.wiimhome.com/pdf/HTTP%20API%20for%20WiiM%20Products.pdf) and [Arylic HTTP API](https://developer.arylic.com/httpapi) describe **a different protocol** (`/httpapi.asp?command=…`, including state-changing GETs). They are not specifications for Wiimu SOAP. This is not an HTTP API implementation, synchronized multiroom test, DLNA certification, or proof of hardware playback quality.
