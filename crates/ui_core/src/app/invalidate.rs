//! 按节点 / 按窗口请求重绘（R01-16）。
//!
//! `Window::request_repaint` 的整窗语义（`full_damage_pending`）保留给「图集补发」这类确实
//! 需要整面的场景；面向应用的公开入口是这两个函数：
//!
//! - [`request_repaint`]（窗口级）：置整窗 damage，下一帧 `Plan::Full`；
//! - [`request_node_repaint`]（**节点级**，Chrome `InvalidateRect` / GPUI `cx.notify()` 的
//!   节点版本）：只把该节点的**子树**标 `PAINT | A11Y`（重建 lane），damage 只来自被重建的
//!   lane ⇒ 不再退化成整窗重绘。祖先链只做**有效性校验**（存活 + 可见），不重建祖先 lane：
//!   本引擎的 lane 是逐节点的（祖先 lane 不含子节点图元），重建祖先会把它自己的背景图元
//!   重新并进 damage，等于把"按节点收窄"又放大回去。
//!
//! 语义边界：这里只重放**绘制输出**（paint spec / 画布 / 几何 / 文本快照的当前值），不重算
//! `bind_style` / `bind_text` / 布局 —— 动态属性由信号写入（`App::invalidate`）或
//! `Transient` + 本入口驱动（文档见 `App::write_transient`）。

use super::App;
use crate::{error::EngineError, node::id::NodeId, window::WindowId};

/// 窗口级重绘：整窗 damage + 请求一帧。
/// 消费者：`App::request_repaint`。
pub(crate) fn request_repaint(app: &mut App, window: WindowId) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    target.request_repaint();
    app.request_frame();
    Ok(())
}

/// 节点级重绘：标记 `node` 的**子树** `PAINT | A11Y` 并请求一帧；damage 只来自被重建的
/// lane（不会退化成整窗）。`Ok(false)` = 子树不可见（`display:none` / `visible:false` 或祖先
/// 隐藏）⇒ 不产生帧请求（点不亮的像素不该出帧）。
/// 消费者：`App::request_node_repaint`。
pub(crate) fn request_node_repaint(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    let tree = target.tree_mut();
    if tree.get(node).is_err() {
        return Err(EngineError::StaleNode { node });
    }
    // 祖先链校验：任一祖先不可见 ⇒ 该子树不绘制，无需重建 lane（与命中/焦点同一可见性判据）。
    if !tree.is_effectively_visible(node) {
        return Ok(false);
    }
    tree.mark_subtree_paint(node);
    app.request_frame();
    Ok(true)
}
