#!/bin/sh
# Package a release archive for the current host (or specified TARGET / OS / ARCH).
set -eu
out=${1:-dist}
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

version=${VERSION:-$(git describe --tags --exact-match 2>/dev/null || git rev-parse --short HEAD 2>/dev/null || echo dev)}
export TACET_DMR_VERSION="$version"

target=${TARGET:-}
if [ -n "$target" ]; then
    cargo build --release --locked --bin tacet-dmr --target "$target"
    bin_dir="target/$target/release"
else
    cargo build --release --locked --bin tacet-dmr
    bin_dir="target/release"
fi

os=${PKG_OS:-$(uname -s | tr '[:upper:]' '[:lower:]')}
arch=${PKG_ARCH:-$(uname -m)}
case "$arch" in
    x86_64) arch=amd64 ;;
    aarch64|arm64) arch=arm64 ;;
esac

ext=""
if [ "$os" = "windows" ]; then
    ext=".exe"
fi

name="tacet-dmr-$os-$arch"
mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir="$stage/$name"
mkdir -p "$dir"
cp "$bin_dir/tacet-dmr$ext" "$dir/tacet-dmr$ext"
cp LICENSE README.md "$dir/"
cp -R docs "$dir/"

if [ "$os" = "windows" ]; then
    if command -v 7z >/dev/null 2>&1; then
        (cd "$stage" && 7z a -tzip -bso0 -bsp0 "$name.zip" "$name")
    elif command -v zip >/dev/null 2>&1; then
        (cd "$stage" && zip -q -r "$name.zip" "$name")
    else
        python3 - "$stage" "$name" <<'PY'
import pathlib, sys, zipfile
stage = pathlib.Path(sys.argv[1])
name = sys.argv[2]
with zipfile.ZipFile(stage / f"{name}.zip", "w", zipfile.ZIP_DEFLATED) as zf:
    for p in sorted((stage / name).rglob("*")):
        if p.is_file():
            zf.write(p, p.relative_to(stage))
PY
    fi
    mv "$stage/$name.zip" "$out/$name.zip"
else
    COPYFILE_DISABLE=1 tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
fi

echo "Packaged $out/$name"
