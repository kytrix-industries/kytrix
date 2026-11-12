//! 平台层（crate 私有；应用层不可见，I9）。
//!
//! Windows 实现是 `platform/windows/**`（`design/phase-09-win32-demo.md` §3）：本文件只做
//! 模块声明与编译期后端选择（I14），**不定义任何 item**（lint `definition_in_module_root`）。
//! macOS / Linux 由 P11 / P17 追加同形目录。

pub(crate) mod dispatcher;

#[cfg(windows)]
pub(crate) mod event;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(windows)]
pub(crate) use windows as native;
