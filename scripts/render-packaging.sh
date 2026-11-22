#!/bin/sh
# Render the Homebrew formulas and AUR PKGBUILDs for one release.
#
#   sh scripts/render-packaging.sh VERSION DIST OUT
#
# DIST holds the release's *.tar.gz.sha256 files (both targets, from
# scripts/package.sh or downloaded from the GitHub Release). OUT receives
# homebrew/{shellux,nulix}.rb and aur/{shellux-bin,nulix-bin}/{PKGBUILD,.SRCINFO},
# ready to copy into a Homebrew tap and the AUR repositories.
set -eu

[ $# -eq 3 ] || {
    echo "usage: render-packaging.sh VERSION DIST OUT" >&2
    exit 2
}
VERSION=$1
DIST=$2
OUT=$3
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

sum() {
    file=$DIST/$1-$VERSION-$2.tar.gz.sha256
    [ -f "$file" ] || {
        echo "render-packaging.sh: missing $file" >&2
        exit 1
    }
    value=$(cut -d ' ' -f 1 "$file")
    case $value in
        *[!0-9a-f]* | '') echo "render-packaging.sh: bad checksum in $file" >&2; exit 1 ;;
    esac
    [ ${#value} -eq 64 ] || { echo "render-packaging.sh: bad checksum in $file" >&2; exit 1; }
    printf '%s' "$value"
}

SHELLUX_LINUX=$(sum shellux x86_64-unknown-linux-gnu)
SHELLUX_DARWIN=$(sum shellux aarch64-apple-darwin)
NULIX_LINUX=$(sum nulix x86_64-unknown-linux-gnu)
NULIX_DARWIN=$(sum nulix aarch64-apple-darwin)

render() {
    mkdir -p "$(dirname "$2")"
    sed -e "s/@VERSION@/$VERSION/g" \
        -e "s/@SHELLUX_LINUX_SHA256@/$SHELLUX_LINUX/g" \
        -e "s/@SHELLUX_DARWIN_SHA256@/$SHELLUX_DARWIN/g" \
        -e "s/@NULIX_LINUX_SHA256@/$NULIX_LINUX/g" \
        -e "s/@NULIX_DARWIN_SHA256@/$NULIX_DARWIN/g" \
        -e '/^# .*template; scripts\/render-packaging.sh/d' \
        "$1" > "$2"
    if grep -q '@[A-Z_0-9]*@' "$2"; then
        echo "render-packaging.sh: unfilled placeholder in $2" >&2
        exit 1
    fi
}

render "$ROOT/packaging/homebrew/shellux.rb.in" "$OUT/homebrew/shellux.rb"
render "$ROOT/packaging/homebrew/nulix.rb.in" "$OUT/homebrew/nulix.rb"

# .SRCINFO is what the AUR web interface reads; makepkg --printsrcinfo is
# not available off Arch, so bash (the PKGBUILD language) writes it from the
# PKGBUILD's own fields.
for pkg in shellux-bin nulix-bin; do
    render "$ROOT/packaging/aur/$pkg/PKGBUILD.in" "$OUT/aur/$pkg/PKGBUILD"
    bash -c '
        set -eu
        source "$1"
        printf "pkgbase = %s\n" "$pkgname"
        printf "\tpkgdesc = %s\n\tpkgver = %s\n\tpkgrel = %s\n\turl = %s\n" "$pkgdesc" "$pkgver" "$pkgrel" "$url"
        for v in "${arch[@]}"; do printf "\tarch = %s\n" "$v"; done
        for v in "${license[@]}"; do printf "\tlicense = %s\n" "$v"; done
        for v in "${depends[@]}"; do printf "\tdepends = %s\n" "$v"; done
        for v in "${optdepends[@]+"${optdepends[@]}"}"; do printf "\toptdepends = %s\n" "$v"; done
        for v in "${provides[@]}"; do printf "\tprovides = %s\n" "$v"; done
        for v in "${conflicts[@]}"; do printf "\tconflicts = %s\n" "$v"; done
        for v in "${source_x86_64[@]}"; do printf "\tsource_x86_64 = %s\n" "$v"; done
        for v in "${sha256sums_x86_64[@]}"; do printf "\tsha256sums_x86_64 = %s\n" "$v"; done
        printf "\npkgname = %s\n" "$pkgname"
    ' bash "$OUT/aur/$pkg/PKGBUILD" > "$OUT/aur/$pkg/.SRCINFO"
done

echo "rendered packaging for $VERSION into $OUT"
