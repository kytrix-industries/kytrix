//! Windows 平台后端。
//!
//! 本文件只做模块声明与 crate 内出口；实现按职责分文件（创建窗口 / 消息泵 / DPI / IME / 剪贴板）。

pub(crate) mod a11y;
pub(crate) mod clipboard;
pub(crate) mod composition;
pub(crate) mod dpi;
pub(crate) mod dpi_awareness;
pub(crate) mod event_loop;
pub(crate) mod frame_clock;
pub(crate) mod ime;
pub(crate) mod window;

pub(crate) use event_loop::run;
