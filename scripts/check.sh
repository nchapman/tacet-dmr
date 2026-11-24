#!/bin/sh
# Full deterministic checks. Only optional tests use multicast (loopback only)
# or a sibling Tacet checkout. Never set a physical speaker as the test target.
set -eu
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ "$(uname -s)" = Darwin ]; then
    if [ -z "${DMR_TEST_LIB:-}" ]; then DMR_TEST_LIB="$(brew --prefix mpv)/lib"; fi
    export DMR_TEST_LIB
    # The test executable may not inherit DYLD_* from the go driver.
    printf '%s\n' '#!/bin/sh' 'export DYLD_FALLBACK_LIBRARY_PATH="$DMR_TEST_LIB"' 'exec "$@"' > "$tmp/with-lib"
    chmod +x "$tmp/with-lib"
    set -- -exec "$tmp/with-lib"
else
    set --
fi
go vet -tags nocgo ./...
go test -race -count=1 -tags nocgo "$@" ./...
export DMR_TEST_BIN="$tmp/tacet-dmr"
go build -tags nocgo -o "$DMR_TEST_BIN" ./cmd/tacet-dmr
(cd interop && uv run --locked python -m unittest discover -v)
if [ -n "${DMR_TEST_IP:-}" ]; then sh scripts/test-tacet.sh; fi
