#!/bin/bash
set -u

ROOT="$1"
OS_DIR="$2"
EXIT_FILE="$3"
cd "$ROOT" || exit 2
export KYTRIX_REAL_PIXEL_EVIDENCE="$OS_DIR"
FAIL=0

run_with_timeout() {
    SECONDS_LIMIT="$1"
    shift
    "$@" &
    PID=$!
    LIMIT=$((SECONDS_LIMIT * 10))
    for _ in $(seq 1 "$LIMIT"); do
        if ! kill -0 "$PID" 2>/dev/null; then
            wait "$PID"
            return $?
        fi
        sleep 0.1
    done
    kill -9 "$PID" 2>/dev/null
    wait "$PID" 2>/dev/null
    return 124
}

run_gate() {
    NAME="$1"
    shift
    echo "== gate $NAME: gallery $* =="
    run_with_timeout 300 ./target/debug/examples/gallery "$@" >"$OS_DIR/$NAME.log" 2>&1
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
    run_with_timeout 300 ./target/debug/examples/gallery --debug-stack >"$OS_DIR/debug-stack-$ATTEMPT.log" 2>&1
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
echo "$([ "$FAIL" -eq 0 ] && echo 0 || echo 3)" >"$EXIT_FILE"
