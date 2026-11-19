//! 真机证据（X2 Windows）：自绘标题栏**拖动区**（`Window::set_drag_regions`）的端到端验证。
//!
//! 运行（Windows only；其它平台是空壳，保证 `cargo test --all-targets` 三端可编译）：
//!
//! ```text
//! cargo run -p ui_core --example p1_win_drag_evidence
//! ```
//!
//! 验证内容：
//! 1. `WM_NCHITTEST` 语义：拖动区内返回 `HTCAPTION`、区域外是 `HTCLIENT`（单元测试已在
//!    隐藏窗口上覆盖，这里在**可见窗口**上再确认一次）；
//! 2. 真实鼠标拖动：`SendInput` 在拖动区对应屏幕位置按下并移动 ≥2 s，窗口位置必须变化
//!    （系统 move 模态循环真的被 `HTCAPTION` 触发）；
//! 3. 拖动期间/之后应用收到 `Event::Moved`（窗口真实移动的回流）。
//!
//! 输出由调用方写入 `.review/p1-b-win-x2.log`（见报告）。

/// 证据采集实现（`evidence.rs`；只在 Windows 上编译）。
#[cfg(windows)]
mod evidence;

#[cfg(not(windows))]
fn main() {
    eprintln!("p1_win_drag_evidence 只在 Windows 上有意义：本平台跳过");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    evidence::run()
}
