#!/bin/sh
# Packs a release archive from a built binary.
#
#   packaging/archive.sh <binary> <os-arch> <outdir>
#
# <os-arch> names the archive: linux-x86_64, linux-aarch64, macos-aarch64, macos-x86_64
# or windows-x86_64. The version is $VERSION when set (the release workflow passes the
# tag's), otherwise Cargo.toml's. Linux and macOS get a .tar.gz with the systemd unit,
# the installer and the examples beside the binary; Windows gets a .zip made with 7z.

set -eu

binary=${1:?binary}
osarch=${2:?os-arch}
outdir=${3:?outdir}

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)}
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }

name="discoclip-v${version}-${osarch}"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$outdir"

case "$osarch" in
    windows-*) cp "$binary" "$stage/$name/discoclip.exe" ;;
    *) cp "$binary" "$stage/$name/discoclip" && chmod 755 "$stage/$name/discoclip" ;;
esac
cp "$root/LICENSE" "$root/README.md" "$root/API.md" "$root/discoclip.example.toml" "$stage/$name/"
mkdir -p "$stage/$name/packaging"
cp "$root/packaging/prometheus.yml" "$stage/$name/packaging/"
case "$osarch" in
    windows-*) ;;
    *)
        cp "$root/packaging/install.sh" "$stage/$name/packaging/"
        chmod 755 "$stage/$name/packaging/install.sh"
        ;;
esac
case "$osarch" in
    linux-*)
        cp "$root/packaging/discoclip.service" "$root/packaging/discoclip.sysusers" "$stage/$name/packaging/"
        ;;
esac

outdir=$(CDPATH='' cd -- "$outdir" && pwd)
case "$osarch" in
    windows-*)
        archive="$outdir/$name.zip"
        rm -f "$archive"
        (cd "$stage" && 7z a -tzip -bso0 -bsp0 "$archive" "$name")
        ;;
    *)
        archive="$outdir/$name.tar.gz"
        (cd "$stage" && tar -czf "$archive" "$name")
        ;;
esac
echo "$archive"
