# Testing

## Running the Suite

Requires Rust 1.89+, `uv`, and Python ≥ 3.11 (for the black-box `async_upnp_client` interoperability suite). Python and the test clients are **not** runtime dependencies.

```sh
# Clippy + Rust unit/integration tests + Python OpenHome & UPnP AV interop suite
sh scripts/check.sh

# Include live multicast SSDP discovery verification
TACET_ENDPOINT_TEST_SSDP=1 sh scripts/check.sh
```

## Coverage

| Layer | Suite | What It Verifies |
| --- | --- | --- |
| Audio engine (`tacet-audio`) | `cargo test -p tacet-audio` | Bit-exact `S16`/`S24Packed`/`S32`/`F32` packing, Songcast big-endian PCM ↔ Q31 conversion, `DriftResampler`, DragonFly Black 24-bit-packed physical format selection, live read-only macOS CoreAudio HAL smoke test, `PcmTap`, `open_pcm`, and exact sample-count gapless PCM transitions. |
| Endpoint protocol (`tacet-endpoint`) | `cargo test -p tacet-endpoint` | End-to-end OpenHome control via `openhome-client` (`Product`, `Playlist` gapless multi-track playback, `Info`, `Time`, `Volume`, GENA events), live UDP **Songcast leader → follower** PCM delivery, minimal UPnP AV (`AVTransport`, `RenderingControl`, `ConnectionManager`, fault codes), cross-device shared volume & source arbitration, `ControllerFilter`, and `-no-upnp` / `-no-sender`. |
| Black-box interop (`interop/`) | `uv run --project interop python -m unittest discover -s interop -v` | Independent Python `async_upnp_client` verification of both OpenHome Media and UPnP AV SCPDs, SOAP actions, SOAP fault codes, GENA `SUBSCRIBE`/`NOTIFY`/renewal, exclusive volume safety policy, and optional multicast SSDP discovery. |
