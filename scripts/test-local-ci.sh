#!/bin/sh
# Exercise local CI without compiling, committing, or running the conformance suite.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' 0
trap 'exit 1' 1 2 3 15
mkdir -p "$TMP/bin" "$TMP/crates/epsh"
cp "$ROOT/Makefile" "$TMP/Makefile"
printf 'version = "1.2.3"\n' > "$TMP/crates/epsh/Cargo.toml"

export TEST_LOG="$TMP/commands.log"
export PATH="$TMP/bin:$PATH"
cat > "$TMP/bin/tool" <<'TOOL'
#!/bin/sh
name=$(basename "$0")
if [ "$name" = git ] && [ "${1:-}" = -C ]; then
    [ -n "${FAKE_GIT_ROOT:-}" ] || exit 1
    printf '%s\n' "$FAKE_GIT_ROOT"
    exit 0
fi
printf '%s %s\n' "$name" "$*" >> "$TEST_LOG"
[ "$(wc -l < "$TEST_LOG")" -ne "${FAIL_AT:-0}" ] || exit 7
TOOL
chmod +x "$TMP/bin/tool"
for tool in cargo perl git; do
    cp "$TMP/bin/tool" "$TMP/bin/$tool"
done
cat > "$TMP/expected" <<'EXPECTED'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build
cargo test --workspace
perl check.pl -p ./target/debug/epsh -s check-epsh.t
EXPECTED

for FAIL_AT in 0 1 2 3 4 5; do
    export FAIL_AT
    : > "$TEST_LOG"
    status=0
    make -s -C "$TMP" ci > "$TMP/output" 2>&1 || status=$?
    if [ "$FAIL_AT" -eq 0 ]; then
        if [ "$status" -ne 0 ]; then
            cat "$TMP/output" >&2
            exit 1
        fi
        count=5
    else
        [ "$status" -ne 0 ] || { echo "CI ignored failure $FAIL_AT" >&2; exit 1; }
        count=$FAIL_AT
    fi
    head -n "$count" "$TMP/expected" > "$TMP/prefix"
    cmp "$TMP/prefix" "$TEST_LOG"
done
printf 'PASS: CI order and fail-fast behavior at all five steps\n'

export FAIL_AT=0
make -s -C "$TMP" bump-version > "$TMP/output" 2>&1
grep -q '^version = "1.2.4"$' "$TMP/crates/epsh/Cargo.toml"
make -s -C "$TMP" bump-version V=2.0.0 > "$TMP/output" 2>&1
grep -q '^version = "2.0.0"$' "$TMP/crates/epsh/Cargo.toml"
printf 'PASS: version bump with default and explicit version (no in-place sed flags)\n'

WORKSPACE="$TMP/exported workspace"
STRESS="$WORKSPACE/crates/epsh/tests/stress"
mkdir -p "$STRESS" "$WORKSPACE/target/debug" "$TMP/custom target/debug"
cp "$ROOT/crates/epsh/tests/stress/run.sh" "$STRESS/run.sh"
printf '[workspace]\nmembers = []\n' > "$WORKSPACE/Cargo.toml"
printf 'echo smoke\n' > "$STRESS/smoke.sh"
cat > "$WORKSPACE/target/debug/epsh" <<'ENGINE'
#!/bin/sh
printf '%s\n' "$0" >> "$STRESS_LOG"
printf 'smoke\n'
ENGINE
chmod +x "$WORKSPACE/target/debug/epsh"
cp "$WORKSPACE/target/debug/epsh" "$TMP/custom target/debug/epsh"
printf '#!/bin/sh\nprintf "smoke\\n"\n' > "$TMP/bin/dash"
chmod +x "$TMP/bin/dash"
export STRESS_LOG="$TMP/stress.log"

check_stress() {
    expected=$1
    shift
    : > "$STRESS_LOG"
    (cd "$TMP" && sh "$STRESS/run.sh" "$@") > "$TMP/output" 2>&1
    grep -Fxq "$expected" "$STRESS_LOG"
    ! grep -q MISMATCH "$TMP/output"
}

unset CARGO_TARGET_DIR FAKE_GIT_ROOT
check_stress "$WORKSPACE/target/debug/epsh"
export FAKE_GIT_ROOT="$WORKSPACE"
check_stress "$WORKSPACE/target/debug/epsh"
export CARGO_TARGET_DIR="$TMP/custom target"
check_stress "$TMP/custom target/debug/epsh"
export CARGO_TARGET_DIR='custom target'
check_stress 'custom target/debug/epsh'
check_stress "$WORKSPACE/target/debug/epsh" "$WORKSPACE/target/debug/epsh"
printf 'PASS: stress defaults, exported-tree fallback, target-dir overrides, and explicit binary\n'
