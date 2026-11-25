# Testing

## Running the Suite

Requires Rust 1.85+, `uv`, and Python ≥ 3.11 (for the black-box UPnP/LinkPlay interoperability suite). Python and the test clients are **not** runtime dependencies.

```sh
# Clippy + Rust unit tests + 15-test Python UPnP/LinkPlay interop suite
sh scripts/check.sh

# Include live SSDP multicast discovery + unmodified sibling ../tacet queue peer test
TACET_DMR_TEST_SSDP=1 DMR_TEST_IP="$(ipconfig getifaddr en0)" sh scripts/check.sh
```

## Coverage

| Layer | Suite | What It Verifies |
| --- | --- | --- |
| Audio engine (`tacet-audio`) | `cargo test -p tacet-audio` | Bit-exact `S16`/`S24Packed`/`S32`/`F32` packing, DragonFly Black 24-bit-packed physical format selection, live read-only macOS CoreAudio HAL smoke test, null-sink playback lifecycle, and exact sample-count gapless PCM transitions. |
| Renderer protocol (`tacet-dmr`) | `cargo test -p tacet-dmr` | All 6 LinkPlay `LoopMode` (`0..5`) / UPnP `PlayMode` mappings, shuffle permutation + repeat semantics, `QueueContext` validation, CIDR/IPv4-mapped controller allowlist, and GENA callback host validation. |
| Black-box UPnP/LinkPlay | `interop/` (`unittest`) | All 4 services (`AVTransport`, `RenderingControl`, `ConnectionManager`, `PlayQueue`), SOAP fault codes, GENA `SUBSCRIBE`/`NOTIFY`/renewal, lookahead queue edits, volume safety, and optional multicast SSDP discovery. |
| Real control point | `scripts/test-tacet.sh` | Unmodified sibling `../tacet` `TestTacetDMRQueuePeer` driving the compiled `tacet-dmr` binary over HTTP/SOAP/GENA. |
