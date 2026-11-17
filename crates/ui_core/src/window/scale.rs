//! DPI 比例与代际。

use super::{Window, surface::covering_surface};
use crate::math::Scale;

// 消费者：`impl Window::scale` 的薄委托（外部调用点见该方法文档）。
/// 当前生效的 DPI 比例。
pub(crate) fn scale(window: &Window) -> Scale {
    window.scale
}

// 消费者：`impl Window::set_scale` 的薄委托（外部调用点见该方法文档）。
/// **不调整 OS 窗口 / 渲染目标**：只改比例与布局失效。平台层随后用 `set_surface` 写入真实
/// 渲染目标；平台已写过真值时本方法保留它，应用代码不要在活窗口上用本方法改客户区
/// （F-186 / F-189）。只有平台尚未写入过目标（headless / 快照 / 首帧前）时，`surface` 才
/// 跟随 `size × scale` 的 covering。
///
/// DPI 变化（P12 的事件处理调用它）：更新比例并标根 `LAYOUT | PAINT`。
///
/// **逻辑布局盒与 scale 无关**（I11：布局全用逻辑像素，物理化在 paint/P06），这里只保守地失效。
///
/// **不**置 `App` 的唤醒请求：DPI 事件本身来自平台，平台总是会调度一帧（P12）。
pub(crate) fn set_scale(window: &mut Window, scale: Scale) {
    window.scale = scale;
    window.scale_epoch = window.scale_epoch.wrapping_add(1);
    window.full_damage_pending = true;
    // B7：布局提交前的设备像素吸附只在**非整数 DPI** 下开启（见 `NodeTree::set_device_scale`）。
    // scale == 1 时逻辑像素就是设备像素，保持引擎"布局不圆整"的既有契约。
    window
        .tree
        .set_device_scale((scale.factor() != 1.0).then_some(scale.factor()));
    let root = window.tree.root();
    window.tree.invalidate_layout(root);
    // **所有文本必须重新 shape**：`font_size` 与 `ppem` 在 shape 时按当时的 `scale`
    // 烘焙进 parley `Run` 与字形位图。只标 LAYOUT 时 `layout_text` 会"重排"，但用的仍是
    // 旧 ppem 的 `ShapedText` → 旧位图以新比例放大，文字发虚、字面不一致，直到该文本内容
    // 再次变化。旧实现每帧全量扫描时靠 `slot.scale_epoch != epoch` 覆盖了这条路径，
    // 改成脏集合驱动后必须显式标脏（P1 复审发现）。
    window.tree.mark_all_text_dirty();
    // a11y 的 bounds 全是物理像素 ⇒ 比例变了必须整棵重发（P11 §5.5）。
    window.a11y.note_bounds_changed();
    // 平台真值一旦写入就不再被 covering 覆盖（F-186）。
    if !window.surface_from_platform {
        window.surface = covering_surface(window.size(), window.scale);
    }
}

// 消费者：`impl Window::scale_epoch` 的薄委托（外部调用点见该方法文档）。
/// DPI 代际（`set_scale` 递增；`App::refresh_text` 的图集换代判据）。
pub(crate) fn scale_epoch(window: &Window) -> u32 {
    window.scale_epoch
}
