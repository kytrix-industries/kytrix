#!/bin/bash
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
TMP="${TMPDIR:-/tmp}"
OS_DIR="$ROOT/target/real-pixels/macos"
LOG="$OS_DIR/run.log"
EXIT_FILE="$TMP/kytrix-gates.exit"
rm -rf "$OS_DIR"
mkdir -p "$OS_DIR"
: > "$LOG"
exec > >(tee -a "$LOG") 2>&1

echo "== kytrix real-pixel gate $(date -u +%Y-%m-%dT%H:%M:%SZ) =="
echo "root=$ROOT"
echo "host=$(uname -a)"
echo "note: under a busy/concurrent desktop (screenshots, parallel gates, IDE load) --debug-stack can exceed the 300 s gate timeout; the script retries it up to 3 attempts and a single timeout is retried without changing the gate result"
if [ -f "$ROOT/tools/source-id.txt" ]; then echo "source_id=$(tr '\n' ' ' < "$ROOT/tools/source-id.txt")"; fi
echo "precondition: macOS screen capture requires Screen Recording permission; plain ssh sessions only capture the wallpaper, so this script launches gallery from Terminal (TCC attribution = Terminal) via osascript"
cd "$ROOT" || exit 2
"$CARGO" build -p ui_kit --example gallery || {
    echo "build FAILED"
    exit 2
}

if ! pgrep -x Terminal >/dev/null; then
    open -a Terminal
    sleep 1
fi
echo "terminal_running=$(pgrep -x Terminal | head -1)"

rm -f "$EXIT_FILE" "$TMP/kytrix-gates.sh"
cp "$ROOT/tools/macos-gates-runner.sh" "$TMP/kytrix-gates.sh"
chmod +x "$TMP/kytrix-gates.sh"

window_ids() {
    osascript -e 'tell application "Terminal" to get id of every window' 2>/dev/null |
        tr -cs '0-9' '\n' | sed '/^$/d'
}
echo "probe: capture via Terminal"
WINDOW_BEFORE="$(window_ids)"
PROBE="$OS_DIR/tcc-probe.png"
rm -f "$PROBE"
osascript -e "tell application \"Terminal\" to do script \"screencapture -x '$PROBE'\"" >/dev/null 2>&1
for _ in $(seq 1 40); do
    [ -f "$PROBE" ] && break
    sleep 0.25
done
if [ -f "$PROBE" ]; then
    echo "tcc_probe=$(file -b "$PROBE")"
else
    echo "tcc_probe=missing (Terminal capture did not produce a PNG)"
fi

osascript -e "tell application \"Terminal\" to do script \"bash '$TMP/kytrix-gates.sh' '$ROOT' '$OS_DIR' '$EXIT_FILE' >>'$LOG' 2>&1\"" >/dev/null 2>&1
for _ in $(seq 1 2400); do
    [ -f "$EXIT_FILE" ] && break
    sleep 0.5
done
CODE="$(cat "$EXIT_FILE" 2>/dev/null || echo 3)"
WINDOW_AFTER="$(window_ids)"
for ID in $WINDOW_AFTER; do
    if echo "$WINDOW_BEFORE" | grep -qx "$ID"; then
        continue
    fi
    osascript -e "tell application \"Terminal\" to close (every window whose id is $ID)" >/dev/null 2>&1
    echo "cleanup: closed Terminal window $ID"
done

echo "gates_exit=$CODE evidence=$OS_DIR"
exit "$CODE"
