//! Windows 平台后端。
//!
//! 本文件只做模块声明与 crate 内出口；实现按职责分文件（创建窗口 / 消息泵 / DPI / IME / 剪贴板）。

pub(crate) mod a11y;
/// E6 frameless： resize 命中 / 最大化客户区补偿 / DWM 保留（纯逻辑 + 少量 DWM 调用）。
pub(crate) mod chrome;
pub(crate) mod clipboard;
pub(crate) mod composition;
pub(crate) mod dark_mode;
pub(crate) mod dpi;
pub(crate) mod dpi_awareness;
pub(crate) mod event_loop;
/// E6 窗口图标：PNG / ICO → `HICON`（`WM_SETICON`）。
pub(crate) mod icon;
pub(crate) mod ime;
pub(crate) mod placement;
pub(crate) mod version;
pub(crate) mod window;

pub(crate) use event_loop::run;
pub(crate) use window::unsupported_window_control;
