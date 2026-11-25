#!/bin/sh
# Exercise the real, unmodified sibling Tacet client. Only the temporary renderer
# listens on the supplied private IP, with SSDP off and the same-IP allowlist.
set -eu
: "${DMR_TEST_IP:?set DMR_TEST_IP to the private IPv4 address of this host}"
root=$(cd "$(dirname "$0")/.." && pwd -P)
tacet=$(cd "${TACET_SRC:-$root/../tacet}" && pwd -P)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$root"
if [ -z "${DMR_TEST_BIN:-}" ]; then
    cargo build --bin tacet-dmr
    export DMR_TEST_BIN="$root/target/debug/tacet-dmr"
fi
python3 - "$tacet" "$root" "$tmp/overlay.json" <<'PY'
import json, pathlib, sys
tacet, root, output = map(pathlib.Path, sys.argv[1:])
output.write_text(json.dumps({'Replace': {str(tacet / 'internal/linkplay/dmr_peer_overlay_test.go'): str(root / 'testdata/tacet/queue_peer_test.go')}}))
PY
cd "$tacet"
go test -overlay="$tmp/overlay.json" -run '^TestTacetDMRQueuePeer$' -count=1 -v ./internal/linkplay
