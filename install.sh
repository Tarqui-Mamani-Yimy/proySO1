#!/bin/sh
# Install Shellux or Nulix from a GitHub release of so1Shell.
#
#   curl -fsSL https://raw.githubusercontent.com/Tarqui-Mamani-Yimy/proySO1/main/install.sh | sh
#   curl -fsSL .../install.sh | PACKAGE=nulix sh
#
# Environment:
#   PACKAGE   shellux (default) or nulix; nulix uses shellux as its default shell
#   VERSION   release version, with or without a leading v (default: the latest release)
#   PREFIX    install root; binaries go to $PREFIX/bin (default: $HOME/.local)
#   REPO      GitHub owner/name (default: Tarqui-Mamani-Yimy/proySO1)
#   BASE_URL  directory holding the release files (default: the GitHub release)
set -eu

# Everything runs from main, called on the last line, so a download cut short
# by `curl | sh` defines a partial function and executes nothing.
main() {
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
        # Without any release, GitHub redirects /releases/latest to /releases.
        case $latest in
            */releases/tag/v*) VERSION=${latest##*/tag/v} ;;
            *) die "$REPO has no published release yet; set VERSION or build from source" ;;
        esac
    fi
    VERSION=${VERSION#v}
    BASE_URL=${BASE_URL:-https://github.com/$REPO/releases/download/v$VERSION}

    stem=$PACKAGE-$VERSION-$target
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM

    echo "Downloading $stem.tar.gz"
    if ! curl -fsSL -o "$tmp/$stem.tar.gz" "$BASE_URL/$stem.tar.gz"; then
        case $target in
            *-apple-darwin) die "macOS builds are not published for v$VERSION yet; build from source with cargo (see README.md)" ;;
            *) die "download failed: $BASE_URL/$stem.tar.gz" ;;
        esac
    fi
    curl -fsSL -o "$tmp/$stem.tar.gz.sha256" "$BASE_URL/$stem.tar.gz.sha256" ||
        die "download failed: $BASE_URL/$stem.tar.gz.sha256"

    expected=$(cut -d ' ' -f 1 "$tmp/$stem.tar.gz.sha256")
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$tmp/$stem.tar.gz" | cut -d ' ' -f 1)
    else
        actual=$(shasum -a 256 "$tmp/$stem.tar.gz" | cut -d ' ' -f 1)
    fi
    [ -n "$expected" ] && [ "$expected" = "$actual" ] || die "checksum mismatch for $stem.tar.gz"

    tar -C "$tmp" -xzf "$tmp/$stem.tar.gz" || die "cannot extract $stem.tar.gz"
    mkdir -p "$PREFIX/bin"
    # Copy every binary next to its destination first, then rename them all, so a
    # failed copy never leaves a mix of old and new versions behind.
    staged=
    for bin in "$tmp/$stem"/bin/*; do
        name=$(basename "$bin")
        if ! cp "$bin" "$PREFIX/bin/.$name.new.$$" || ! chmod 0755 "$PREFIX/bin/.$name.new.$$"; then
            rm -f "$PREFIX/bin/.$name.new.$$"
            for done_name in $staged; do rm -f "$PREFIX/bin/.$done_name.new.$$"; done
            die "cannot write to $PREFIX/bin"
        fi
        staged="$staged $name"
    done
    for name in $staged; do
        mv -f "$PREFIX/bin/.$name.new.$$" "$PREFIX/bin/$name"
        echo "Installed $PREFIX/bin/$name"
    done

    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) echo "Add $PREFIX/bin to your PATH to run the installed commands." ;;
    esac
    if [ "$PACKAGE" = nulix ] && ! command -v shellux >/dev/null 2>&1 && [ ! -x "$PREFIX/bin/shellux" ]; then
        echo "Nulix starts shellux by default; install it with PACKAGE=shellux, or set shell = ... in ~/.config/nulix/nulix.conf."
    fi
}

main "$@"
