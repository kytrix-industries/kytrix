//! 窗口：批注尺寸 + 自绘标题栏的最小尺寸约束（`design/phase-12-macos-linux.md` §8.3）。
//!
//! Wayland 没有「窗口对象」：状态在 `WaylandState`（surface/toplevel）里，本模块只放
//! 与协议无关的窗口常量与尺寸换算，避免它散落到事件翻译层。

use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

use crate::{math::Scale, platform::linux::dpi};

/// 最小客户区（逻辑像素；与 Win32 / macOS 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// 把最小尺寸按当前 `scale` 交给 `xdg_toplevel`（缩放变化后必须重发）。
pub(crate) fn apply_min_size(toplevel: &XdgToplevel, scale: Scale) {
    let (width, height) = dpi::physical_size(
        crate::math::Size::new(MIN_SIZE_LOGICAL.0, MIN_SIZE_LOGICAL.1),
        scale,
    );
    toplevel.set_min_size(width as i32, height as i32);
}
