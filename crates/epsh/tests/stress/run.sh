#!/bin/sh
# Stress test runner — compares epsh vs dash
set -e

DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
if [ -n "${1:-}" ]; then
    EPSH="$1"
elif [ -n "${CARGO_TARGET_DIR:-}" ]; then
    EPSH="$CARGO_TARGET_DIR/debug/epsh"
else
    ROOT=$(git -C "$DIR" rev-parse --show-toplevel 2>/dev/null) || ROOT=
    if [ -z "$ROOT" ] || ! grep -q '^\[workspace\]' "$ROOT/Cargo.toml" 2>/dev/null; then
        ROOT="$DIR"
        while ! grep -q '^\[workspace\]' "$ROOT/Cargo.toml" 2>/dev/null; do
            PARENT=$(dirname "$ROOT")
            if [ "$PARENT" = "$ROOT" ]; then
                echo "Cannot locate the Cargo workspace root; set CARGO_TARGET_DIR or pass a binary." >&2
                exit 1
            fi
            ROOT="$PARENT"
        done
    fi
    EPSH="$ROOT/target/debug/epsh"
fi
DASH="${2:-dash}"

printf "%-25s %10s %10s %8s\n" "TEST" "EPSH" "DASH" "RATIO"
printf "%-25s %10s %10s %8s\n" "----" "----" "----" "-----"

for test in "$DIR"/*.sh; do
    name=$(basename "$test" .sh)
    [ "$name" = "run" ] && continue

    # Time epsh
    t1=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
    out_e=$("$EPSH" "$test" 2>/dev/null) || true
    t2=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
    ms_e=$(( (t2 - t1) / 1000000 ))

    # Time dash
    t1=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
    out_d=$("$DASH" "$test" 2>/dev/null) || true
    t2=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
    ms_d=$(( (t2 - t1) / 1000000 ))

    # Check correctness
    if [ "$out_e" != "$out_d" ]; then
        status="MISMATCH"
    else
        status=""
    fi

    # Ratio
    if [ "$ms_d" -gt 0 ]; then
        ratio=$(echo "scale=1; $ms_e / $ms_d" | bc 2>/dev/null || echo "?")
    else
        ratio="?"
    fi

    printf "%-25s %8sms %8sms %7sx %s\n" "$name" "$ms_e" "$ms_d" "$ratio" "$status"
done
