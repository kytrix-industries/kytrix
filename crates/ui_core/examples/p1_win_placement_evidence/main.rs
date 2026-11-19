//! 真机证据（B3 Windows）：`WindowOptions.position` / `min_size` / `max_size` 与
//! `Event::Moved` 的真实事件循环验证。
//!
//! 运行（Windows only；其它平台是空壳，保证 `cargo test --all-targets` 三端可编译）：
//!
//! ```text
//! cargo run -p ui_core --example p1_win_placement_evidence
//! ```
//!
//! 验证内容：
//! 1. `position` 建窗后，客户区左上角的**屏幕物理坐标** = 逻辑点 × 窗口所在显示器 scale；
//! 2. `WM_GETMINMAXINFO` 的 `ptMinTrackSize` / `ptMaxTrackSize` 与配置的 min/max 客户区
//!    尺寸一致（客户区 → 用 `AdjustWindowRectExForDpi` 换成窗口外框，含 DPI 换算）；
//! 3. 程序化 `SetWindowPos` 尝试越界尺寸：窗口被系统约束到 min/max 跟踪尺寸；
//! 4. 第二个窗口请求**所有显示器之外**的位置：平台 clamp 到某个显示器工作区（真实多屏
//!    宿主；单屏时同一证据仍成立，双屏边界另由 `placement` 纯函数单测覆盖）；
//! 5. 程序化 `SetWindowPos` 移动窗口 → 真实事件循环把 `WM_MOVE` 派发成
//!    `Event::Moved`（逻辑坐标 = 物理坐标 / 该消息的 scale）。
//!
//! 输出由调用方写入 `.review/p1-b-win-b3.log`（见报告）。

/// 证据采集实现（`evidence.rs`；只在 Windows 上编译）。
#[cfg(windows)]
mod evidence;

#[cfg(not(windows))]
fn main() {
    eprintln!("p1_win_placement_evidence 只在 Windows 上有意义：本平台跳过");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    evidence::run()
}
