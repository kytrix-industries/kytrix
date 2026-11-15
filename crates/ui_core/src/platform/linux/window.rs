//! 窗口：最小尺寸约束（`design/phase-12-macos-linux.md` §8.3）。
//!
//! Wayland 没有「窗口对象」：状态在 `WaylandState`（surface/toplevel）里，本模块只放
//! 与协议无关的窗口常量与最小尺寸设置，避免它散落到事件翻译层。
//!
//! 最小尺寸是 **window geometry 的逻辑像素**，与 `scale` 无关：xdg-shell 的
//! `xdg_toplevel.set_min_size` 参数是 window geometry 坐标，不是 buffer 像素。本仓用
//! `wp_viewport.set_destination(ceil(logical))` 让 surface 尺寸等于逻辑尺寸（S-144），所以
//! 320×200 直接就是逻辑值；因此它只需在建窗时（`wl_surface` 首个 commit 之前）设置一次
//! （V-122 ⑦）。
//!
//! 没有 viewporter 的退化路径（V-122 ④ / P2-5）不在本方法保证范围内：那时 surface-local
//! 坐标与逻辑尺寸不再一一对应，本方法写入的 320×200 不保证是期望的最小客户区。

use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

/// 最小客户区（逻辑像素；与 Win32 / macOS 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// 把最小尺寸交给 `xdg_toplevel`：window geometry 的逻辑像素，与 `scale` 无关。
///
/// 只在建窗时（`WaylandState::new` 的首个 commit 之前）调用一次：DPI 变化后重发会把最小客户区
/// 按 `scale` 放大（V-122 ⑦）。
pub(crate) fn apply_min_size(toplevel: &XdgToplevel) {
    toplevel.set_min_size(MIN_SIZE_LOGICAL.0 as i32, MIN_SIZE_LOGICAL.1 as i32);
}
