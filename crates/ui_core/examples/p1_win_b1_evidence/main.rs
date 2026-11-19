//! 真机证据（B1 Windows）：系统 move 模态循环（拖动标题栏）期间的**持续出帧**。
//!
//! 运行（Windows only；其它平台是空壳，保证 `cargo test --all-targets` 三端可编译）：
//!
//! ```text
//! cargo run -p ui_core --example p1_win_b1_evidence
//! cargo run -p ui_core --example p1_win_b1_evidence -- --edge
//! ```
//!
//! 验证内容（对应报告 §1 / 用户裁决 D1(a)）：
//! 1. 真实窗口 + 真 GPU 运行事件循环；后台线程用**真实输入**按住系统标题栏拖动 **3.2 s**
//!    （`SetCursorPos` 到标题栏中点 → `SendInput(LEFTDOWN)` → 连续
//!    `SendInput(MOUSEEVENTF_MOVE)` ≥3 s → `SendInput(LEFTUP)`，末步单程拉开 160 px）；
//!    期间每 ~50 ms 采样提交帧数 / `Window::frame_index()` / `GUI_INMOVESIZE`；
//! 2. 断言窗口**真的被系统拖动**：`GetWindowRect` 位移 > 100 px 且尺寸不变 —— 这是「泵被
//!    系统模态循环占住、出帧只能来自模态 tick」的前提（程序化 `WM_NCLBUTTONDOWN` 不会让
//!    窗口移动，旧实现因此判 FAIL）；
//! 3. 拖动期间帧数 / 时长（fps）、最大帧间隔、进程 CPU 占用、`on_frame` 钩子推进；
//! 4. 松手后再记录 **500 ms**：帧数不得显著高于拖动态势（无「堆积爆发」）；
//! 5. `--edge`：拖动过程中程序化关闭**最后一个**窗口 —— 不 panic、事件循环正常退出。
//!
//! 变异对照（禁用模态 tick 分支后重编译再跑本程序，日志
//! `.review/p1-b-win-b1-mutation.log`）：同样的拖动会真的移动窗口，但拖动期间帧数 ≈ 0、
//! 松手后集中爆发 —— 本程序必须打印 `RESULT: FAIL`。输出由调用方重定向到 `.review/`。

/// 证据采集实现（`evidence.rs`；只在 Windows 上编译）。
#[cfg(windows)]
mod evidence;

#[cfg(not(windows))]
fn main() {
    eprintln!("p1_win_b1_evidence 只在 Windows 上有意义：本平台跳过");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    evidence::run()
}
