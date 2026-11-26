#!/bin/sh
# Run the full local verification suite: Rust clippy + tests and Python
# OpenHome + UPnP AV black-box interop suite.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --bin tacet-endpoint

export TACET_ENDPOINT_BIN="$root/target/debug/tacet-endpoint"
uv run --project interop python -m unittest discover -s interop -v
