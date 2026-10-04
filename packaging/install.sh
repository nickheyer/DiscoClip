#!/bin/sh
# Installs DiscoClip from a GitHub release on Linux or macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/nickheyer/DiscoClip/main/packaging/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/nickheyer/DiscoClip/main/packaging/install.sh | sudo sh -s -- --system
#
# Options:
#   --version vX.Y.Z   a release other than the newest
#   --archive FILE     a downloaded release archive instead of a download
#   --prefix DIR       where bin/discoclip goes. Default: /usr/local as root, ~/.local otherwise
#   --system           Linux, as root: the binary under the prefix, the service account,
#                      the systemd unit and /etc/discoclip/discoclip.toml, enabled and started
#   --repo OWNER/NAME  the GitHub repository. Default: nickheyer/DiscoClip
#
# A download is checked against the release's SHA256SUMS before anything is installed.
# Windows: take the .zip from the releases page.

set -eu

repo="nickheyer/DiscoClip"
version=""
archive=""
prefix=""
system=false

usage() { sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version=${2:?}; shift ;;
        --archive) archive=${2:?}; shift ;;
        --prefix) prefix=${2:?}; shift ;;
        --repo) repo=${2:?}; shift ;;
        --system) system=true ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
    shift
done

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) die "$(uname -s) is not supported by this installer. Windows: take the .zip from the releases page." ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) die "no release is built for $(uname -m)" ;;
esac
if [ "$system" = true ]; then
    [ "$os" = linux ] || die "--system is for Linux with systemd"
    [ "$(id -u)" = 0 ] || die "--system needs root"
    [ -d /run/systemd/system ] || die "--system needs systemd"
fi
if [ -z "$prefix" ]; then
    if [ "$(id -u)" = 0 ]; then prefix=/usr/local; else prefix="$HOME/.local"; fi
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "curl or wget is needed to download the release"
fi
if command -v sha256sum >/dev/null 2>&1; then
    checksum() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    checksum() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "sha256sum or shasum is needed to check the download"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [ -z "$archive" ]; then
    if [ -z "$version" ]; then
        fetch "https://api.github.com/repos/$repo/releases/latest" "$work/latest.json"
        version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$work/latest.json" | head -1)
        [ -n "$version" ] || die "could not read the newest release of $repo"
    fi
    name="discoclip-${version}-${os}-${arch}"
    base="https://github.com/$repo/releases/download/$version"
    echo "Downloading $name.tar.gz"
    fetch "$base/$name.tar.gz" "$work/$name.tar.gz"
    fetch "$base/SHA256SUMS" "$work/SHA256SUMS"
    expected=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$work/SHA256SUMS")
    [ -n "$expected" ] || die "$name.tar.gz is not in the release's SHA256SUMS"
    actual=$(checksum "$work/$name.tar.gz")
    [ "$actual" = "$expected" ] || die "checksum mismatch for $name.tar.gz: got $actual, expected $expected"
    echo "Checksum verified"
    archive="$work/$name.tar.gz"
fi

mkdir -p "$work/unpacked"
tar -xzf "$archive" -C "$work/unpacked"
binary=$(find "$work/unpacked" -type f -name discoclip | head -1)
[ -n "$binary" ] || die "no discoclip binary in $archive"
unpacked=$(dirname "$binary")

install -d "$prefix/bin"
install -m 755 "$binary" "$prefix/bin/discoclip"
echo "Installed $prefix/bin/discoclip: $("$prefix/bin/discoclip" --version)"

if [ "$system" = false ]; then
    case ":$PATH:" in
        *":$prefix/bin:"*) ;;
        *) echo "Add $prefix/bin to PATH to run discoclip by name." ;;
    esac
    exit 0
fi

[ -f "$unpacked/packaging/discoclip.service" ] || die "the archive carries no systemd unit"
install -d -m 755 /etc/sysusers.d
install -m 644 "$unpacked/packaging/discoclip.sysusers" /etc/sysusers.d/discoclip.conf
systemd-sysusers discoclip.conf
install -d -m 750 -o root -g discoclip /etc/discoclip
if [ ! -f /etc/discoclip/discoclip.toml ]; then
    install -m 640 -o root -g discoclip "$unpacked/discoclip.example.toml" /etc/discoclip/discoclip.toml
    echo "Wrote /etc/discoclip/discoclip.toml from the example"
fi
sed "s|^ExecStart=/usr/bin/discoclip |ExecStart=$prefix/bin/discoclip |" \
    "$unpacked/packaging/discoclip.service" > "$work/discoclip.service"
install -m 644 "$work/discoclip.service" /etc/systemd/system/discoclip.service
systemctl daemon-reload
if systemctl is-active --quiet discoclip.service; then
    systemctl restart discoclip.service
    echo "Restarted discoclip.service"
else
    systemctl enable --now discoclip.service
    echo "Enabled and started discoclip.service"
fi
bind=$(sed -n 's/^bind = "\([^"]*\)".*/\1/p' /etc/discoclip/discoclip.toml | head -1)
echo "The web app listens on ${bind:-127.0.0.1:8080}. Open /setup once to create the admin account."
