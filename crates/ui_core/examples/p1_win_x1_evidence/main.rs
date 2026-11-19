//! 真机证据（X1 Windows）：`WindowOptions.owner / modal / always_on_top` 的原生语义。
//!
//! 运行（Windows only；其它平台是空壳，保证 `cargo test --all-targets` 三端可编译）：
//!
//! ```text
//! cargo run -p ui_core --example p1_win_x1_evidence
//! ```
//!
//! 验证内容（对应审查报告 §3.2 / 用户裁决 X1 的 Windows 原生部分）：
//! 1. **owner**：可见 owner + owned 窗口，`GetWindow(owned, GW_OWNER) == owner`（`CreateWindowExW`
//!    的 `hWndParent` 走 owner 语义、不设 `WS_CHILD`），且 owned 在 z 序里位于 owner 之上；
//! 2. **always_on_top**：可见置顶窗口 `GetWindowLongPtr(GWL_EXSTYLE)` 带 `WS_EX_TOPMOST`
//!    （普通窗口不带），且在 z 序里位于普通 owner 之上；
//! 3. **modal**：模态窗口打开期间系统禁用 owner（`IsWindowEnabled(owner) == false`），
//!    关闭模态窗口后恢复 `true`；
//! 4. **最小化跟随**：owner 最小化时 owned 隐藏（`IsWindowVisible == false`），恢复后重新可见。
//!
//! 所有检查从后台线程对**真实可见窗口**做只读/系统状态查询（不发输入、不抢焦点）；判定全为
//! true 才打印 `RESULT: PASS`，输出由调用方重定向到 `.review/p1-b-win-x1.log`。

/// 证据采集实现（`evidence.rs`；只在 Windows 上编译）。
#[cfg(windows)]
mod evidence;

#[cfg(not(windows))]
fn main() {
    eprintln!("p1_win_x1_evidence 只在 Windows 上有意义：本平台跳过");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    evidence::run()
}
