#!/bin/sh
# Install Shellux or Nulix from a GitHub release of so1Shell.
#
#   curl -fsSL https://raw.githubusercontent.com/Tarqui-Mamani-Yimy/proySO1/main/install.sh | sh
#   curl -fsSL .../install.sh | PACKAGE=nulix sh
#
# Environment:
#   PACKAGE   shellux (default) or nulix; nulix uses shellux as its default shell
#   VERSION   release version without the leading v (default: the latest release)
#   PREFIX    install root; binaries go to $PREFIX/bin (default: $HOME/.local)
#   REPO      GitHub owner/name (default: Tarqui-Mamani-Yimy/proySO1)
#   BASE_URL  directory holding the release files (default: the GitHub release)
set -eu

REPO=${REPO:-Tarqui-Mamani-Yimy/proySO1}
PACKAGE=${PACKAGE:-shellux}
PREFIX=${PREFIX:-${HOME:?HOME is not set}/.local}

die() {
    echo "install.sh: $*" >&2
    exit 1
}

case $PACKAGE in
    shellux | nulix) ;;
    *) die "PACKAGE must be shellux or nulix, got '$PACKAGE'" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

os=${INSTALL_OS:-$(uname -s)}
arch=${INSTALL_ARCH:-$(uname -m)}
case "$os/$arch" in
    Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-gnu ;;
    Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
    *) die "no prebuilt binary for $os/$arch; build from source with cargo (see README.md)" ;;
esac

if [ -z "${VERSION:-}" ]; then
    latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
        die "cannot find the latest release of $REPO"
    VERSION=${latest##*/v}
    case $VERSION in
        '' | */*) die "cannot read the latest release version from $latest" ;;
    esac
fi
BASE_URL=${BASE_URL:-https://github.com/$REPO/releases/download/v$VERSION}

stem=$PACKAGE-$VERSION-$target
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $stem.tar.gz"
curl -fsSL -o "$tmp/$stem.tar.gz" "$BASE_URL/$stem.tar.gz" || die "download failed: $BASE_URL/$stem.tar.gz"
curl -fsSL -o "$tmp/$stem.tar.gz.sha256" "$BASE_URL/$stem.tar.gz.sha256" ||
    die "download failed: $BASE_URL/$stem.tar.gz.sha256"

expected=$(cut -d ' ' -f 1 "$tmp/$stem.tar.gz.sha256")
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$stem.tar.gz" | cut -d ' ' -f 1)
else
    actual=$(shasum -a 256 "$tmp/$stem.tar.gz" | cut -d ' ' -f 1)
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || die "checksum mismatch for $stem.tar.gz"

tar -C "$tmp" -xzf "$tmp/$stem.tar.gz"
mkdir -p "$PREFIX/bin"
for bin in "$tmp/$stem"/bin/*; do
    name=$(basename "$bin")
    cp "$bin" "$PREFIX/bin/$name.tmp.$$"
    chmod 0755 "$PREFIX/bin/$name.tmp.$$"
    mv -f "$PREFIX/bin/$name.tmp.$$" "$PREFIX/bin/$name"
    echo "Installed $PREFIX/bin/$name"
done

case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "Add $PREFIX/bin to your PATH to run the installed commands." ;;
esac
if [ "$PACKAGE" = nulix ] && ! command -v shellux >/dev/null 2>&1 && [ ! -x "$PREFIX/bin/shellux" ]; then
    echo "Nulix starts shellux by default; install it with PACKAGE=shellux, or set shell = ... in ~/.config/nulix/nulix.conf."
fi
