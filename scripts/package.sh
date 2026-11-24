#!/bin/sh
# Build the same native-runtime-dependent CLI shipped in GitHub Releases.
set -eu
cd "$(dirname "$0")/.."
export COPYFILE_DISABLE=1
version=${VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo dev)}
out=${1:-dist}
mkdir -p "$out"
out=$(cd "$out" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/assets"
for os in darwin linux windows; do
    for arch in amd64 arm64; do
        name=tacet-dmr-$os-$arch
        dir=$tmp/$name
        mkdir "$dir"
        bin=tacet-dmr
        if [ "$os" = windows ]; then bin=tacet-dmr.exe; fi
        CGO_ENABLED=0 GOOS=$os GOARCH=$arch go build -tags nocgo -trimpath -ldflags="-s -w -X main.version=${version}" -o "$dir/$bin" ./cmd/tacet-dmr
        cp README.md "$dir/"
        cp -R docs "$dir/"
        if [ "$os" = windows ]; then
            (cd "$tmp" && zip -qr "assets/$name.zip" "$name")
        else
            tar -czf "$tmp/assets/$name.tar.gz" -C "$tmp" "$name"
        fi
    done
done
(cd "$tmp/assets" && if command -v sha256sum >/dev/null; then sha256sum *.tar.gz *.zip; else shasum -a 256 *.tar.gz *.zip; fi) > "$tmp/assets/SHA256SUMS"
rm -f "$out"/tacet-dmr-*.tar.gz "$out"/tacet-dmr-*.zip "$out/SHA256SUMS"
cp "$tmp/assets/"* "$out/"
