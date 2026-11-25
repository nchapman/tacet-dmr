#!/bin/sh
# Run the full local verification suite: Rust clippy + tests, Python UPnP/LinkPlay
# interop suite, and (when sibling ../tacet is present) unmodified Tacet queue peer.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --bin tacet-dmr

export DMR_TEST_BIN="$root/target/debug/tacet-dmr"
uv run --project interop python -m unittest discover -s interop -v

if [ -d "${TACET_SRC:-$root/../tacet}" ] && [ -n "${DMR_TEST_IP:-}" ]; then
    sh scripts/test-tacet.sh
fi
