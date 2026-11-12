//! Windows 平台后端（`design/phase-09-win32-demo.md` §3）。
//!
//! 本文件只做模块声明与 crate 内出口；实现按职责分文件（创建窗口 / 消息泵 / DPI / IME / 剪贴板）。

pub(crate) mod clipboard;
pub(crate) mod dpi;
pub(crate) mod event_loop;
pub(crate) mod ime;
pub(crate) mod window;

pub(crate) use event_loop::run;
