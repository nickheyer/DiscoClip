#!/bin/sh
# Builds the .deb and .rpm packages from a built binary with nfpm.
#
#   packaging/package.sh <binary> <amd64|arm64> <outdir> [deb|rpm ...]
#
# The version is $VERSION when set (the release workflow passes the tag's), otherwise
# Cargo.toml's. Without packagers named, both are built. nfpm is needed:
# https://nfpm.goreleaser.com

set -eu

binary=${1:?binary}
arch=${2:?amd64 or arm64}
outdir=${3:?outdir}
shift 3
[ $# -gt 0 ] || set -- deb rpm

case "$arch" in
    amd64|arm64) ;;
    *) echo "package.sh: arch must be amd64 or arm64, not $arch" >&2; exit 1 ;;
esac
command -v nfpm >/dev/null 2>&1 || { echo "package.sh: nfpm is needed: https://nfpm.goreleaser.com" >&2; exit 1; }

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)}
[ -n "$version" ] || { echo "package.sh: no version in Cargo.toml" >&2; exit 1; }
mkdir -p "$outdir"
outdir=$(CDPATH='' cd -- "$outdir" && pwd)

# nfpm reads the binary from a fixed path, relative to the repository root.
mkdir -p "$root/target/package"
cp "$binary" "$root/target/package/discoclip"
chmod 755 "$root/target/package/discoclip"

cd "$root"
for packager in "$@"; do
    VERSION="$version" ARCH="$arch" \
        nfpm package --config packaging/nfpm.yaml --packager "$packager" --target "$outdir/"
done
