#!/usr/bin/env bash
# Kytrix ui_core 验证流水线（POSIX 侧：macOS / Linux）。
#
# 用途：
#   1) 远端真机上的等价脚本（由 `scripts/verify.ps1` 通过 ssh 调用）；
#   2) 在 macOS / Linux 上直接跑（远端调试）。
#
# 用法：
#   scripts/verify.sh [--platform NAME] [--bench] [--tsv PATH]
#
#   --platform NAME  报告里用的平台名（默认 posix；verify.ps1 传 mac / linux）
#   --bench          额外编译基准（`cargo bench --no-run`；不跑测量）
#   --tsv PATH       追加机器可读结果（`platform<TAB>step<TAB>status<TAB>seconds`），
#                    verify.ps1 用它做汇总；文件会先被清空
#
# 环境：
#   PATH 由脚本自己补齐（非交互 shell 的 PATH 极简，cargo 在 $HOME/.cargo/bin）。
#   CARGO_TARGET_DIR 未设置时默认 $HOME/kytrix-review-target（/tmp 可能是 tmpfs）。
#
# 退出码：任一步失败 ⇒ 1；全部通过（或跳过）⇒ 0。

set -u

platform="posix"
bench=0
tsv=""

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --platform)
            platform="${2:-}"
            if [ -z "$platform" ]; then
                echo "error: --platform needs a value" >&2
                exit 2
            fi
            shift 2
            ;;
        --bench)
            bench=1
            shift
            ;;
        --tsv)
            tsv="${2:-}"
            if [ -z "$tsv" ]; then
                echo "error: --tsv needs a value" >&2
                exit 2
            fi
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

# 非交互 zsh/sh 的 PATH 极简：显式补齐（macOS 的 /opt/homebrew 与 Linux 的 /usr/local 都带上）。
PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.cargo/bin:${PATH:-}"
export PATH

if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    CARGO_TARGET_DIR="${HOME}/kytrix-review-target"
    export CARGO_TARGET_DIR
fi
if [ -z "${CARGO_TERM_COLOR:-}" ]; then
    CARGO_TERM_COLOR=never
    export CARGO_TERM_COLOR
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root" || exit 2

echo "== verify.sh: platform=${platform} root=${root} target=${CARGO_TARGET_DIR} bench=${bench}"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found on PATH (${PATH})" >&2
    exit 2
fi

# nightly 是可选工具链：远端没装 nightly 时 clippy/fmt 记为 skip，而不是伪装成失败。
nightly=0
if cargo +nightly --version >/dev/null 2>&1; then
    nightly=1
fi

fails=0
passes=0
skips=0

if [ -n "$tsv" ]; then
    mkdir -p "$(dirname "$tsv")"
    : >"$tsv"
fi

record() {
    name="$1"
    status="$2"
    seconds="$3"
    if [ -n "$tsv" ]; then
        printf '%s\t%s\t%s\t%s\n' "$platform" "$name" "$status" "$seconds" >>"$tsv"
    fi
}

# run_step <name> <command...>
run_step() {
    name="$1"
    shift
    started=$(date +%s)
    echo ""
    echo "---- [${platform}] ${name}: $*"
    if "$@"; then
        status=pass
        mark="✅"
        passes=$((passes + 1))
    else
        status=fail
        mark="❌"
        fails=$((fails + 1))
    fi
    finished=$(date +%s)
    seconds=$((finished - started))
    record "$name" "$status" "$seconds"
    echo "${mark} [${platform}] ${name} (${seconds}s)"
}

# skip_step <name> <reason>
skip_step() {
    name="$1"
    reason="$2"
    skips=$((skips + 1))
    record "$name" "skip" "0"
    echo "⏭ [${platform}] ${name} skipped: ${reason}"
}

run_step check cargo check -p ui_core --all-targets --all-features

if [ "$nightly" = 1 ]; then
    run_step clippy cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings
else
    skip_step clippy "nightly toolchain is not installed"
fi

run_step test cargo test -p ui_core --all-features

if [ "$nightly" = 1 ]; then
    # `-p ui_core` 是必须的：全 workspace 的 fmt 会碰到别的 crate。
    run_step fmt cargo +nightly fmt -p ui_core -- --check
else
    skip_step fmt "nightly toolchain is not installed"
fi

if [ "$bench" = 1 ]; then
    run_step bench cargo bench -p ui_core --features test-support --bench perf --no-run
fi

echo ""
echo "== verify.sh summary [${platform}]: ${passes} passed, ${fails} failed, ${skips} skipped"
if [ "$fails" -eq 0 ]; then
    echo "RESULT [${platform}]: PASS"
    exit 0
fi
echo "RESULT [${platform}]: FAIL"
exit 1
