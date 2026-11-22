#!/bin/sh
# Build release binaries and package the shellux and nulix tarballs.
#
#   shellux-VERSION-TARGET.tar.gz  bin/shellux bin/epsh LICENSE LICENSE-epsh README.md
#   nulix-VERSION-TARGET.tar.gz    bin/nulix bin/nulix-server bin/nulixctl LICENSE
#                                  README.md nulix.conf.example
#
# Each tarball gets a matching .sha256 file. Used by CI (release.yml) and locally.
# Environment: VERSION (default: the shellux crate version), OUT (default: dist/),
# SKIP_BUILD=1 to package already built binaries, CARGO_TARGET_DIR as usual.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/crates/shell/Cargo.toml" | head -n 1)}
TARGET=$(rustc -vV | sed -n 's/^host: //p')
OUT=${OUT:-$ROOT/dist}
BIN_DIR=${CARGO_TARGET_DIR:-$ROOT/target}/release

if [ -z "$VERSION" ]; then
    echo "package.sh: cannot read the shellux version" >&2
    exit 1
fi

if [ "${SKIP_BUILD:-0}" != 1 ]; then
    (cd "$ROOT" && cargo build --release --locked -p epsh -p shellux -p nulix)
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi
}

# package NAME FILE... : FILE is SOURCE:DEST, DEST relative to the archive root.
package() {
    name=$1
    shift
    stem=$name-$VERSION-$TARGET
    stage=$(mktemp -d)
    mkdir -p "$stage/$stem/bin"
    for spec in "$@"; do
        src=${spec%%:*}
        dest=${spec#*:}
        if [ ! -f "$src" ]; then
            echo "package.sh: missing $src" >&2
            rm -rf "$stage"
            exit 1
        fi
        cp "$src" "$stage/$stem/$dest"
    done
    chmod 0755 "$stage/$stem"/bin/*
    mkdir -p "$OUT"
    tar -C "$stage" -czf "$OUT/$stem.tar.gz" "$stem"
    rm -rf "$stage"
    (cd "$OUT" && sha256 "$stem.tar.gz" > "$stem.tar.gz.sha256")
    echo "$OUT/$stem.tar.gz"
}

package shellux \
    "$BIN_DIR/shellux:bin/shellux" \
    "$BIN_DIR/epsh:bin/epsh" \
    "$ROOT/LICENSE:LICENSE" \
    "$ROOT/crates/epsh/LICENSE:LICENSE-epsh" \
    "$ROOT/README.md:README.md"

package nulix \
    "$BIN_DIR/nulix:bin/nulix" \
    "$BIN_DIR/nulix-server:bin/nulix-server" \
    "$BIN_DIR/nulixctl:bin/nulixctl" \
    "$ROOT/LICENSE:LICENSE" \
    "$ROOT/README.md:README.md" \
    "$ROOT/docs/nulix.conf.example:nulix.conf.example"
