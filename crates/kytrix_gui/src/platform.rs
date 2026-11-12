//! 平台层（crate 私有；应用层不可见，I9）。
//!
//! Windows / macOS / Linux 三份实现按 `#[cfg(target_os)]` 编译期选定恰好一份（I14）；
//! 本文件只做模块声明与 crate 内出口，**不定义任何 item**（lint `definition_in_module_root`）。
//!
//! `event` 是 **Windows 专用**的 `WM_*` 解码层（`PlatformEvent` / `translate`）；macOS / Linux 的
//! 平台层直接把原生事件翻成公共 `Event` 后调 `App::dispatch`
//! （`design/phase-12-macos-linux.md` §3.2 / S-120）。
//!
//! 三份实现都必须提供：`run(app)`、`window::NativeWindow`、`dpi`、`ime`、`clipboard`、`a11y`。

pub(crate) mod dispatcher;

#[cfg(windows)]
pub(crate) mod event;
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux as native;
#[cfg(target_os = "macos")]
pub(crate) use macos as native;
#[cfg(windows)]
pub(crate) use windows as native;
