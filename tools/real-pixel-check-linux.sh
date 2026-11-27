#!/bin/bash
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
OS_ROOT="$ROOT/target/real-pixels/linux"
SOURCE_SHA="$(sed -n 's/^diff_sha=//p' "$ROOT/tools/source-id.txt" 2>/dev/null | head -1 | cut -c1-8)"
RUN_DIR="$OS_ROOT/$(date -u +%Y%m%dT%H%M%SZ)-${SOURCE_SHA:-unknown}"
mkdir -p "$RUN_DIR"
ln -sfn "$RUN_DIR" "$OS_ROOT/latest"
export KYTRIX_REAL_PIXEL_EVIDENCE="$RUN_DIR"
OS_DIR="$RUN_DIR"
LOG="$OS_DIR/run.log"
: > "$LOG"
exec > >(tee -a "$LOG") 2>&1

echo "== kytrix real-pixel gate $(date -u +%Y-%m-%dT%H:%M:%SZ) =="
echo "run_dir=$RUN_DIR"
echo "note: under a busy/concurrent desktop (screenshots, parallel gates, IDE load) --debug-stack can exceed the 300 s gate timeout; the script retries it up to 3 attempts and a single timeout is retried without changing the gate result"
if [ -f "$ROOT/tools/source-id.txt" ]; then echo "source_id=$(tr '\n' ' ' < "$ROOT/tools/source-id.txt")"; fi
echo "root=$ROOT"
echo "host=$(uname -a)"
cd "$ROOT" || exit 2
"$CARGO" build -p ui_kit --example gallery || {
    echo "build FAILED"
    exit 2
}

RUNTIME="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export XDG_RUNTIME_DIR="$RUNTIME"
sockets() {
    find "$RUNTIME" -maxdepth 1 -name 'wayland-*' -type s -printf '%f\n' 2>/dev/null | sort
}
if [ -S "$RUNTIME/wayland-0" ]; then
    PARENT="wayland-0"
else
    PARENT="$(sockets | head -1)"
fi
if [ -z "$PARENT" ]; then
    echo "no parent wayland socket in $RUNTIME"
    exit 2
fi
BEFORE="$(sockets)"
echo "parent=$PARENT before=$(echo "$BEFORE" | tr '\n' ' ')"
WAYLAND_DISPLAY="$PARENT" sway --unsupported-gpu >"$OS_DIR/sway.log" 2>&1 &
SWAY_PID=$!
cleanup() {
    if kill -0 "$SWAY_PID" 2>/dev/null; then
        kill "$SWAY_PID" 2>/dev/null
        for _ in $(seq 1 30); do
            kill -0 "$SWAY_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$SWAY_PID" 2>/dev/null
    fi
    echo "cleanup: nested sway pid $SWAY_PID stopped"
}
trap cleanup EXIT

SOCKET=""
for _ in $(seq 1 80); do
    AFTER="$(sockets)"
    SOCKET="$(comm -13 <(echo "$BEFORE") <(echo "$AFTER") | head -1)"
    [ -n "$SOCKET" ] && break
    sleep 0.25
done
if [ -z "$SOCKET" ]; then
    echo "nested sway did not create a new wayland socket"
    exit 2
fi
export WAYLAND_DISPLAY="$SOCKET"
echo "nested_socket=$SOCKET sway_pid=$SWAY_PID"

PROBE="$OS_DIR/grim-probe.png"
GRIM_OK=0
for _ in $(seq 1 40); do
    if grim "$PROBE" >/dev/null 2>&1; then
        GRIM_OK=1
        break
    fi
    sleep 0.25
done
if [ "$GRIM_OK" -ne 1 ]; then
    echo "grim probe failed on $WAYLAND_DISPLAY"
    exit 2
fi
echo "grim_probe=$(file -b "$PROBE" 2>/dev/null)"

FAIL=0
run_gate() {
    NAME="$1"
    shift
    echo "== gate $NAME: gallery $* =="
    timeout 300 ./target/debug/examples/gallery "$@" >"$OS_DIR/$NAME.log" 2>&1
    CODE=$?
    echo "gate $NAME exit=$CODE"
    tail -n 4 "$OS_DIR/$NAME.log"
    [ "$CODE" -eq 0 ] || FAIL=1
}

run_gate pixel-check --pixel-check
run_gate wheel-check --wheel-check
run_gate real-pixel-check --real-pixel-check
run_gate idle-check --idle-check
run_gate theme-check --theme-check

DEBUG_OK=0
for ATTEMPT in 1 2 3; do
    echo "== gate debug-stack attempt $ATTEMPT =="
    timeout 300 ./target/debug/examples/gallery --debug-stack >"$OS_DIR/debug-stack-$ATTEMPT.log" 2>&1
    CODE=$?
    echo "debug-stack attempt $ATTEMPT exit=$CODE"
    tail -n 4 "$OS_DIR/debug-stack-$ATTEMPT.log"
    if [ "$CODE" -eq 0 ]; then
        cp "$OS_DIR/debug-stack-$ATTEMPT.log" "$OS_DIR/debug-stack.log"
        DEBUG_OK=1
        break
    fi
done
[ "$DEBUG_OK" -eq 1 ] || FAIL=1

echo "gates_done fail=$FAIL"
exit "$([ "$FAIL" -eq 0 ] && echo 0 || echo 3)"
