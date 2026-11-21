#!/bin/sh
# Packages the release binaries of the current Linux or macOS host into dist/, with the asset names and layout of
# docs/spec/07-peripheral-build.md (A.4.2, A.4.3, I.2):
#   dist/<name>/bin/opencode + package.json  ->  dist/<name>.tar.gz (linux) or dist/<name>.zip (darwin)
#   <name> = opencode-<linux|darwin>-<x64|arm64>[-baseline][-musl]; x64 is also packaged under its -baseline name
#   dist/opencode-desktop-<linux|mac>-<arch>.<tar.gz|zip>  ->  opencode-desktop (no installer; see README)
# The archive root holds only the executable. Nothing is signed, uploaded or published.
#
# Usage: scripts/package.sh [--skip-build] [--skip-desktop]
set -eu

skip_build=0
skip_desktop=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) skip_build=1 ;;
        --skip-desktop) skip_desktop=1 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
dist="$root/dist"
release="$root/target/release"

case "$(uname -s)" in
    Linux) os=linux; desktop_os=linux; ext=tar.gz ;;
    Darwin) os=darwin; desktop_os=mac; ext=zip ;;
    *) echo "unsupported system: $(uname -s) (use scripts/package.ps1 on Windows)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch=x64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# musl hosts get the -musl suffix and a libc field, as the original's build does.
libc=
if [ "$os" = linux ]; then
    if [ -f /etc/alpine-release ] || (ldd --version 2>&1 | grep -qi musl); then
        libc=musl
    fi
fi

version=${OPENCODE_VERSION:-}
if [ -z "$version" ]; then
    version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$root/Cargo.toml" | head -n 1)
fi
[ -n "$version" ] || { echo "no workspace version in Cargo.toml" >&2; exit 1; }

if [ "$skip_build" -eq 0 ]; then
    if [ "$skip_desktop" -eq 0 ]; then
        (cd "$root" && cargo build --release --locked -p opencode-cli -p opencode-desktop)
    else
        (cd "$root" && cargo build --release --locked -p opencode-cli)
    fi
fi

cli="$release/opencode"
[ -f "$cli" ] || { echo "missing $cli; build first or drop --skip-build" >&2; exit 1; }
mkdir -p "$dist"

# pack <output> <directory> <file>: the file sits at the archive root.
pack() {
    rm -f "$1"
    if [ "$ext" = zip ]; then
        (cd "$2" && zip -q "$1" "$3")
    else
        tar -czf "$1" -C "$2" "$3"
    fi
}

assets=
cli_asset() {
    name=$1
    stage="$dist/$name"
    rm -rf "$stage"
    mkdir -p "$stage/bin"
    cp "$cli" "$stage/bin/opencode"
    chmod 755 "$stage/bin/opencode"
    {
        printf '{\n  "name": "%s",\n  "version": "%s",\n  "preferUnplugged": true,\n' "$name" "$version"
        printf '  "os": ["%s"],\n  "cpu": ["%s"]' "$os" "$arch"
        if [ -n "$libc" ]; then printf ',\n  "libc": ["%s"]' "$libc"; fi
        printf '\n}\n'
    } > "$stage/package.json"
    pack "$dist/$name.$ext" "$stage/bin" opencode
    assets="$assets $dist/$name.$ext"
}

suffix=${libc:+-$libc}
cli_asset "opencode-$os-$arch$suffix"
if [ "$arch" = x64 ]; then
    cli_asset "opencode-$os-x64-baseline$suffix"
fi

if [ "$skip_desktop" -eq 0 ]; then
    [ -f "$release/opencode-desktop" ] || { echo "missing $release/opencode-desktop; build first or pass --skip-desktop" >&2; exit 1; }
    pack "$dist/opencode-desktop-$desktop_os-$arch.$ext" "$release" opencode-desktop
    assets="$assets $dist/opencode-desktop-$desktop_os-$arch.$ext"
fi

if command -v sha256sum >/dev/null 2>&1; then sum="sha256sum"; else sum="shasum -a 256"; fi
(cd "$dist" && for asset in $assets; do $sum "$(basename "$asset")"; done > SHA256SUMS)

for asset in $assets; do
    ls -lh "$asset" | awk '{print $NF "  " $5}'
done
