//! 窗口：最小尺寸与 CSD 装饰度量（`design/phase-12-macos-linux.md` §8.3）。
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
//! **阴影边距**（R10-10）：引擎目前不画投影，[`SHADOW_MARGIN`] 是 0，于是 window geometry
//! 就是整个 surface，`set_min_size(320, 200)` 正好约束客户区。将来加投影时只需把边距改成非 0：
//! `xdg_surface.set_window_geometry` 会据此收缩（阴影落在几何之外），`set_min_size` 会据此
//! 把边距补回去（否则窗口能被缩到内容不可用）。两处都走
//! [`crate::platform::wayland::toplevel`] 的纯函数，语义不会分叉。
//!
//! 没有 viewporter 时走整数回退（buffer = `round(逻辑 × N)`、`wl_surface.set_buffer_scale(N)`，
//! 见 `surface.rs`）：Wayland 的逻辑尺寸已在平台层取整，buffer 严格是 N 的整数倍（否则
//! `wl_surface.attach` 报 `invalid_size`），surface 的逻辑尺寸因此等于 `buffer / N`，
//! 本方法写入的 320×200 仍是期望的最小客户区。

use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

use crate::platform::wayland::toplevel;

/// 最小客户区（逻辑像素；与 Win32 / macOS 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// CSD 阴影 / 边框在 window geometry **之外**占的逻辑像素（每边）。
///
/// 0 = 不自绘阴影（当前实现）：window geometry 等于整个 surface。
pub(crate) const SHADOW_MARGIN: f32 = 0.0;

/// 把最小尺寸交给 `xdg_toplevel`：window geometry 的逻辑像素（含阴影边距）。
///
/// 每个窗口只在建窗时（`WaylandState::add_window` 的首个 commit 之前）调用一次：DPI 变化后
/// 重发会把最小客户区按 `scale` 放大（V-122 ⑦）。
pub(crate) fn apply_min_size(toplevel: &XdgToplevel) {
    let (width, height) = toplevel::min_size_with_shadow(MIN_SIZE_LOGICAL, SHADOW_MARGIN);
    toplevel.set_min_size(width, height);
}
