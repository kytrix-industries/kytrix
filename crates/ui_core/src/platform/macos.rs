//! macOS 平台层（`design/phase-12-macos-linux.md` §7）。
//!
//! 模块根只做声明（lint `definition_in_module_root`）；`run` 是 `Application::run` 的平台入口。

pub(crate) mod a11y;
/// `NSApplicationDelegate`：协商式退出（R09-16）。
pub(crate) mod app_delegate;
pub(crate) mod clipboard;
pub(crate) mod dpi;
pub(crate) mod event_loop;
pub(crate) mod ime;
/// `NSMenu` 主菜单（R09-1）：Cmd+Q / Cmd+W / Cmd+H / 编辑快捷键的承载对象。
pub(crate) mod menu;
pub(crate) mod view;
pub(crate) mod wake;
pub(crate) mod window;

pub(crate) use event_loop::run;
