#!/bin/sh
# Local end-to-end check of scripts/package.sh and install.sh (odd/tasks/packaging.md).
# Packages the release binaries into a scratch directory, installs both packages
# from it through install.sh (BASE_URL=file://...), runs the installed commands,
# and checks the refusal paths. SKIP_BUILD=1 reuses existing release binaries.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT INT TERM
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/crates/shell/Cargo.toml" | head -n 1)
TARGET=$(rustc -vV | sed -n 's/^host: //p')
DIST=$SCRATCH/dist
failures=0

check() {
    if eval "$2"; then
        echo "ok   - $1"
    else
        echo "FAIL - $1"
        failures=$((failures + 1))
    fi
}

OUT=$DIST VERSION=$VERSION sh "$ROOT/scripts/package.sh" >/dev/null

for pkg in shellux nulix; do
    tar -tzf "$DIST/$pkg-$VERSION-$TARGET.tar.gz" | sed "s|^$pkg-$VERSION-$TARGET/||" | grep -v '/$' | grep -v '^$' |
        LC_ALL=C sort > "$SCRATCH/$pkg.list"
done
check "shellux tarball contents" \
    "[ \"\$(tr '\n' ' ' < $SCRATCH/shellux.list)\" = 'LICENSE LICENSE-epsh README.md bin/epsh bin/shellux ' ]"
check "nulix tarball contents" \
    "[ \"\$(tr '\n' ' ' < $SCRATCH/nulix.list)\" = 'LICENSE README.md bin/nulix bin/nulix-server bin/nulixctl nulix.conf.example ' ]"
check "checksum files match" \
    "(cd $DIST && for f in *.sha256; do want=\$(cut -d ' ' -f 1 \$f); got=\$( (sha256sum \${f%.sha256} 2>/dev/null || shasum -a 256 \${f%.sha256}) | cut -d ' ' -f 1); [ \"\$want\" = \"\$got\" ] || exit 1; done)"

install_pkg() {
    PACKAGE=$1 VERSION=$VERSION PREFIX=$2 BASE_URL=file://$3 sh "$ROOT/install.sh"
}

PREFIX=$SCRATCH/prefix
check "install shellux" "install_pkg shellux $PREFIX $DIST > $SCRATCH/shellux.log 2>&1"
check "installed shellux runs" "[ \"\$($PREFIX/bin/shellux -c 'echo \$((6*7))')\" = 42 ]"
check "installed epsh runs" "[ \"\$($PREFIX/bin/epsh -c 'echo ok')\" = ok ]"
check "install nulix" "install_pkg nulix $PREFIX $DIST > $SCRATCH/nulix.log 2>&1"
for bin in nulix nulix-server nulixctl; do
    check "installed $bin --help" "$PREFIX/bin/$bin --help > /dev/null 2>&1"
done

# A tampered checksum installs nothing.
cp -R "$DIST" "$SCRATCH/bad"
echo "0000000000000000000000000000000000000000000000000000000000000000  shellux-$VERSION-$TARGET.tar.gz" \
    > "$SCRATCH/bad/shellux-$VERSION-$TARGET.tar.gz.sha256"
check "tampered checksum is refused" \
    "! install_pkg shellux $SCRATCH/bad-prefix $SCRATCH/bad > $SCRATCH/bad.log 2>&1 && grep -q 'checksum mismatch' $SCRATCH/bad.log && [ ! -e $SCRATCH/bad-prefix/bin/shellux ]"
check "unsupported platform is refused" \
    "! INSTALL_OS=FreeBSD INSTALL_ARCH=x86_64 install_pkg shellux $SCRATCH/x $DIST > $SCRATCH/os.log 2>&1 && grep -q 'no prebuilt binary for FreeBSD/x86_64' $SCRATCH/os.log"
check "unknown package is refused" \
    "! install_pkg epsh $SCRATCH/y $DIST > $SCRATCH/pkg.log 2>&1 && grep -q 'PACKAGE must be shellux or nulix' $SCRATCH/pkg.log"

# Rendered packaging: no placeholders, real checksums, and each PKGBUILD's
# package() installs the expected files from the real tarball.
RENDER=$SCRATCH/render
mkdir -p "$SCRATCH/dist-both"
cp "$DIST"/*.sha256 "$SCRATCH/dist-both/"
for pkg in shellux nulix; do # the macOS sums are stand-ins off macOS
    [ -f "$SCRATCH/dist-both/$pkg-$VERSION-aarch64-apple-darwin.tar.gz.sha256" ] ||
        cp "$DIST/$pkg-$VERSION-$TARGET.tar.gz.sha256" "$SCRATCH/dist-both/$pkg-$VERSION-aarch64-apple-darwin.tar.gz.sha256"
done
check "render packaging" "sh $ROOT/scripts/render-packaging.sh $VERSION $SCRATCH/dist-both $RENDER > /dev/null"
check "no placeholders left" "! grep -rq '@[A-Z_0-9]*@' $RENDER"
check "formula carries the linux checksum" \
    "grep -q \"\$(cut -d ' ' -f 1 $DIST/shellux-$VERSION-$TARGET.tar.gz.sha256)\" $RENDER/homebrew/shellux.rb"
check "nulix formula depends on shellux" "grep -q 'depends_on \"shellux\"' $RENDER/homebrew/nulix.rb"
check "SRCINFO lists version and checksum" \
    "grep -q \"pkgver = $VERSION\" $RENDER/aur/shellux-bin/.SRCINFO && grep -q \"sha256sums_x86_64 = \$(cut -d ' ' -f 1 $DIST/shellux-$VERSION-$TARGET.tar.gz.sha256)\" $RENDER/aur/shellux-bin/.SRCINFO"
check "missing checksum is refused" \
    "! sh $ROOT/scripts/render-packaging.sh $VERSION $DIST $SCRATCH/render-bad > $SCRATCH/render.log 2>&1 && grep -q 'missing' $SCRATCH/render.log"
if [ "$TARGET" = x86_64-unknown-linux-gnu ]; then
    for pkg in shellux nulix; do
        src=$SCRATCH/aur-src-$pkg
        mkdir -p "$src" "$SCRATCH/aur-pkg-$pkg"
        tar -C "$src" -xzf "$DIST/$pkg-$VERSION-$TARGET.tar.gz"
        check "PKGBUILD package() for $pkg-bin" \
            "(cd $src && pkgdir=$SCRATCH/aur-pkg-$pkg bash -c 'set -e; source \"\$1\"; package' bash $RENDER/aur/$pkg-bin/PKGBUILD)"
    done
    check "shellux-bin installs its files" \
        "[ -x $SCRATCH/aur-pkg-shellux/usr/bin/shellux ] && [ -x $SCRATCH/aur-pkg-shellux/usr/bin/epsh ] && [ -f $SCRATCH/aur-pkg-shellux/usr/share/licenses/shellux-bin/LICENSE-epsh ]"
    check "nulix-bin installs its files" \
        "[ -x $SCRATCH/aur-pkg-nulix/usr/bin/nulix ] && [ -x $SCRATCH/aur-pkg-nulix/usr/bin/nulixctl ] && [ -f $SCRATCH/aur-pkg-nulix/usr/share/doc/nulix/nulix.conf.example ]"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all install checks passed"
