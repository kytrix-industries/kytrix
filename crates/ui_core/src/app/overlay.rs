//! 浮层：创建 / 关闭 / 锚点重定位 / 提层 / 自下而上枚举。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::{App, overlay_style};
use crate::{
    error::EngineError,
    interaction,
    interaction::{
        Overlay, OverlayDismiss, OverlayOptions, OverlayPlacement, OverlaySlot,
        overlay as overlay_util,
    },
    math::{Rect, Size},
    node::{builder::NodeBuilder, id::NodeId},
    window::WindowId,
};

// ── P10：浮层 ──────────────────────────────────────────────────────────────

/// 打开一个浮层（root 的最后一个子节点 ⇒ `order` 末尾 ⇒ 命中优先）。
///
/// 记录打开瞬间的焦点节点作为 **opener**（`restore_focus` 时关闭后还给触发按钮，R02-12）；
/// `trap_focus` 置位时该浮层成为焦点作用域根（Tab 只在子树内循环、外部按下被消费）。
///
/// # Errors
/// `UnknownWindow`（窗口已关闭）/ `StaleNode`（树结构被破坏）。
/// 消费者：`App::create_overlay`。
pub(crate) fn create_overlay(
    app: &mut App,
    window: WindowId,
    options: OverlayOptions,
) -> Result<Overlay, EngineError> {
    let opener = app.focused(window);
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    let root = target.tree().root();
    // 新建时还不知道浮层尺寸：`Above` / `Left` 首帧先按零尺寸定位，布局之后由
    // resolve_overlay_placements 用实测尺寸纠正（P2-6）。
    let origin = interaction::resolve_placement(target.tree(), options.placement, Size::ZERO);
    let style = overlay_style(origin);
    let node = NodeBuilder::boxed()
        .style(style)
        .build(target.tree_mut(), root)?;
    if let Some(slot) = target.tree_mut().ensure_interaction(node) {
        slot.overlay = Some(OverlaySlot {
            dismiss: options.dismiss,
            placement: options.placement,
            trap_focus: options.trap_focus,
            opener,
            restore_focus: options.restore_focus,
        });
    }
    #[cfg(feature = "hud")]
    app.raise_debug_hud(window);
    app.request_frame();
    Ok(Overlay::new(window, node))
}

/// 关闭浮层（节点不存在 ⇒ `false`）。
/// 消费者：`App::close_overlay`。
pub(crate) fn close_overlay(app: &mut App, overlay: &Overlay) -> bool {
    app.remove_node(overlay.window(), overlay.node())
}

/// 帧首按**上一帧布局的实测尺寸**重新求值锚点定位（P2-6）。
///
/// `Above` / `Left` 需要知道浮层自己的宽/高；新建时按零尺寸定位会让浮层盖住锚点。
/// 这里是标准的 measure-then-place：**在布局之前**用上一帧的尺寸纠位，纠位写回后本帧
/// 布局就会用新位置；只做一次（收敛条件：位置差 < 0.5 px 早退）。
///
/// **不能放在布局之后**：布局之后写 inset 会被 `settle_frame` 的「本轮已布局」判定吞掉
/// `LAYOUT` 位，下一帧变 `Idle`，位置永远不生效（P1-2.2 的陷阱）。
/// 消费者：`App::resolve_overlay_placements`。
pub(crate) fn resolve_overlay_placements(app: &mut App, window: WindowId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    let root = target.tree().root();
    let children = target.tree().get(root).map_or_else(
        |_| smallvec::SmallVec::<[NodeId; 4]>::new(),
        |node| node.children().iter().copied().collect(),
    );
    let mut moved = false;
    for node in children {
        let Some(slot) = target.tree().overlay_slot(node) else {
            continue;
        };
        if !matches!(slot.placement, OverlayPlacement::Anchor { .. }) {
            continue;
        }
        let size = target
            .tree()
            .get(node)
            .map_or(Size::ZERO, |target| target.layout().bounds.size);
        let origin = interaction::resolve_placement(target.tree(), slot.placement, size);
        let current = target
            .tree()
            .get(node)
            .map_or(Rect::default(), |target| target.layout().bounds);
        if (current.origin.x - origin.x).abs() < 0.5 && (current.origin.y - origin.y).abs() < 0.5 {
            continue;
        }
        let _ = target.tree_mut().set_absolute_inset(node, origin);
        moved = true;
    }
    // 纠位写回会弄脏布局，必须再调度一帧让它生效：否则第一帧按「零尺寸」定位后，
    // 泵在没有后续输入时会直接阻塞，`Above` / `Left` 浮层永久盖住锚点（复审发现）。
    // 收敛条件在上面（< 0.5 px 早退），所以最多多出两帧。
    if moved {
        app.request_frame();
    }
}

/// 重新定位浮层（相对根的逻辑像素点）。
/// 消费者：`App::place_overlay`。
pub(crate) fn place_overlay(app: &mut App, overlay: &Overlay, placement: OverlayPlacement) -> bool {
    let window = overlay.window();
    let node = overlay.node();
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    let size = target
        .tree()
        .get(node)
        .map_or(Size::ZERO, |target| target.layout().bounds.size);
    let origin = interaction::resolve_placement(target.tree(), placement, size);
    if !target.tree_mut().set_absolute_inset(node, origin) {
        return false;
    }
    if let Some(slot) = target.tree_mut().ensure_interaction(node) {
        slot.overlay = slot.overlay.map(|overlay| OverlaySlot {
            placement,
            ..overlay
        });
    }
    app.request_frame();
    true
}

/// 把浮层提到最上层（重挂到 root 末尾）。
/// 消费者：`App::bring_overlay_to_front`。
pub(crate) fn bring_overlay_to_front(app: &mut App, overlay: &Overlay) -> bool {
    let window = overlay.window();
    let node = overlay.node();
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    let root = target.tree().root();
    if target.tree_mut().append_child(root, node).is_err() {
        return false;
    }
    #[cfg(feature = "hud")]
    app.raise_debug_hud(window);
    app.request_frame();
    true
}

/// 当前窗口的浮层（自下而上）。
/// 消费者：`App::overlays`。
pub(crate) fn overlays(app: &App, window: WindowId) -> Vec<(NodeId, OverlayDismiss)> {
    let Some(target) = app.windows.get(&window) else {
        return Vec::new();
    };
    let tree = target.tree();
    let Ok(root) = tree.get(tree.root()) else {
        return Vec::new();
    };
    root.children()
        .iter()
        .filter_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot.dismiss)))
        .collect()
}

/// `Dismiss` 动作：关掉最上层浮层（含 `Explicit`）。
/// 消费者：`App::dismiss_overlay`。
pub(crate) fn dismiss_overlay(app: &mut App, window: WindowId) -> bool {
    let node = app.windows.get(&window).and_then(|target| {
        let tree = target.tree();
        overlay_util::topmost(tree, tree.root()).map(|(node, _)| node)
    });
    match node {
        Some(node) => app.remove_node(window, node),
        None => false,
    }
}
