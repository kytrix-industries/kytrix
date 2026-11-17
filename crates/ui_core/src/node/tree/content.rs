//! 节点内容与旁表读写：文本 / a11y 载荷 / 绑定闭包 / 交互槽 / lane / clip。

use std::rc::Rc;

use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{NodeKind, NodeTree};
#[cfg(test)]
use crate::paint::style::Style;
use crate::{
    error::EngineError,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    math::{PhysicalRect, Point, Rect, Size},
    node::{dependency::ReadCtx, dirty::DirtyFlags, id::NodeId, signal::SignalId},
    paint::{scene::LaneId, style::StylePatch},
    text::{ShapedLine, TextContent},
};

// 消费者：`impl NodeTree::text_content` 的薄委托（外部调用点见该方法文档）。
// ── P05：文本（载荷 / 形状快照 / 选区） ───────────────────────────────────

/// 文本载荷（`None` = 不是文本节点）。消费者：`App::refresh_styles` / `App::refresh_text`。
pub(crate) fn text_content(tree: &NodeTree, node: NodeId) -> Option<&TextContent> {
    tree.arena.get(node).and_then(|target| target.kind.text())
}

// 消费者：`impl NodeTree::set_text_content` 的薄委托（外部调用点见该方法文档）。
/// 写回文本载荷（`bind_text` 求值 + 静态 `text()` 的挂载路径）。
pub(crate) fn set_text_content(tree: &mut NodeTree, node: NodeId, content: TextContent) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.kind {
            NodeKind::Text(existing) => {
                *existing = content;
                tree.mark_text_dirty(node);
                true
            }
            _ => false,
        },
        None => false,
    }
}

// 消费者：`impl NodeTree::mark_text_dirty` 的薄委托（外部调用点见该方法文档）。
/// 登记「该节点的文本需要重新 shape」（P1-2.3 的脏集合）。
///
/// 去重是线性扫描：典型每帧 0–32 条，比哈希表更快且迭代序确定。节点已不存在时静默忽略
/// （卸载路径会在 `remove` 里清理）。
pub(crate) fn mark_text_dirty(tree: &mut NodeTree, node: NodeId) {
    if !tree.text_dirty.contains(&node) {
        tree.text_dirty.push(node);
    }
}

// 消费者：`impl NodeTree::take_text_dirty` 的薄委托（外部调用点见该方法文档）。
/// 把本帧待 shape 的文本节点追加进 `out` 并清空集合（**两侧容量都保留**）。
///
/// 消费者：`App::refresh_text`。返回的列表可能包含已被卸载的节点 —— 调用方按
/// 「查不到就跳过」处理（卸载时 `remove` 会尝试清理，但集合是旁路结构，不做强一致）。
pub(crate) fn take_text_dirty(tree: &mut NodeTree, out: &mut Vec<NodeId>) {
    // `drain` 保住 `tree.text_dirty` 的容量，`out` 由调用方跨帧复用：
    // 旧实现 `mem::take` 每帧都让双方重新分配（复审 P3-2）。
    out.clear();
    out.append(&mut tree.text_dirty);
}

// 消费者：`impl NodeTree::text_dirty_len` 的薄委托（外部调用点见该方法文档）。
/// 本帧待 shape 的文本节点数（诊断 / 测试）。
#[cfg(test)]
pub(crate) fn text_dirty_len(tree: &NodeTree) -> usize {
    tree.text_dirty.len()
}

// 消费者：`impl NodeTree::set_text_shaped` 的薄委托（外部调用点见该方法文档）。
/// 冻结形状快照并重建 measure 闭包。
///
/// **无条件**清 `node..root` 的 taffy 缓存（B1）：taffy 的 cache 键不含「闭包捕获的状态」，
/// snapshot 变化后旧 measure 结果必须作废；`size_changed` 只决定**是否额外**置 `LAYOUT`。
pub(crate) fn set_text_shaped(
    tree: &mut NodeTree,
    node: NodeId,
    shaped: Rc<crate::text::shape::ShapedText>,
    key: crate::text::cache::ShapeKey,
    scale_epoch: u32,
    measured: Size,
    size_changed: bool,
) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    let rc = Rc::clone(&shaped);
    target.measure = Some(Box::new(move |known, available| {
        rc.measure(known, available)
    }));
    target.text_measured_size = measured;

    let index = node.index() as usize;
    if tree.text_slots.len() <= index {
        tree.text_slots.resize_with(index + 1, || None);
    }
    match tree.text_slots.get_mut(index).and_then(Option::as_mut) {
        Some(slot) => {
            slot.shaped = shaped;
            slot.key = key;
            slot.scale_epoch = scale_epoch;
            // 新快照 ⇒ 旧的断行/选区/布局依据全部作废（B4）：`layout_text` 的复用判据是
            // `line.is_some() && width_bits && generation`，任一不成立就重排。
            slot.line = None;
            slot.broken = None;
            slot.laid_width_bits = 0;
            slot.laid_generation = 0;
        }
        None => {
            if let Some(slot) = tree.text_slots.get_mut(index) {
                *slot = Some(TextSlot {
                    shaped,
                    key,
                    scale_epoch,
                    line: None,
                    broken: None,
                    laid_width_bits: 0,
                    laid_generation: 0,
                });
            }
        }
    }

    tree.invalidate_caches_upwards(node);
    if size_changed {
        tree.mark_layout(node)
    } else {
        tree.mark_paint(node)
    }
}

// 消费者：`impl NodeTree::set_text_line` 的薄委托（外部调用点见该方法文档）。
/// 挂上只读快照（P06 的 paint 输入）；`width`/`generation` 记下这次重排的依据。
pub(crate) fn set_text_line(
    tree: &mut NodeTree,
    node: NodeId,
    line: Rc<ShapedLine>,
    broken: parley::Layout<crate::paint::color::ColorBrush>,
    width: f32,
    generation: u32,
) -> bool {
    let index = node.index() as usize;
    let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
        return false;
    };
    slot.line = Some(line);
    slot.broken = Some(Box::new(broken));
    slot.laid_width_bits = width.to_bits();
    slot.laid_generation = generation;
    // 快照变了 ⇒ 文本 lane 里的 UV/尺寸必须重建。`set_text_line` 的调用方
    // （`layout_text`）在布局阶段跑，此处的 `PAINT` 会让本帧 paint 重新编码该节点，
    // 而不是复用 `REUSABLE` 的旧图元（否则图集中途换代时会用新纹理画旧 UV，
    // 复审 P2-9）。`mark_paint` 会清 `REUSABLE`，这正是需要的。
    tree.mark_paint(node);
    true
}

// 消费者：`impl NodeTree::text_content_width` 的薄委托（外部调用点见该方法文档）。
/// 最近一次布局的**内容宽**（逻辑像素；文本节点的断行宽度）。
pub(crate) fn text_content_width(tree: &NodeTree, node: NodeId) -> f32 {
    tree.arena
        .get(node)
        .map_or(0.0, |target| target.text_content_width)
}

// 消费者：`impl NodeTree::text_line` 的薄委托（外部调用点见该方法文档）。
/// 只读快照（P06）。
pub(crate) fn text_line(tree: &NodeTree, node: NodeId) -> Option<&Rc<ShapedLine>> {
    tree.text_slots
        .get(node.index() as usize)
        .and_then(Option::as_ref)
        .and_then(TextSlot::line)
}

// 消费者：`impl NodeTree::text_measured_size` 的薄委托（外部调用点见该方法文档）。
/// 最近一次 shape 的内容尺寸（逻辑像素）。
pub(crate) fn text_measured_size(tree: &NodeTree, node: NodeId) -> Option<Size> {
    tree.arena.get(node).map(|target| target.text_measured_size)
}

// 消费者：`impl NodeTree::text_slot` 的薄委托（外部调用点见该方法文档）。
/// 文本快照（`shaped` 供 measure/选区）。
pub(crate) fn text_slot(tree: &NodeTree, node: NodeId) -> Option<&TextSlot> {
    tree.text_slots
        .get(node.index() as usize)
        .and_then(Option::as_ref)
}

// 消费者：`impl NodeTree::text_selection_rects` 的薄委托（外部调用点见该方法文档）。
/// `(anchor, focus)` 字节偏移 → 文本盒局部的选区矩形（P10 的入口）。
///
/// # Errors
/// `StaleNode`：句柄陈旧；`TextShape`：不是文本节点或尚未 shape。
pub(crate) fn text_selection_rects(
    tree: &NodeTree,
    node: NodeId,
    anchor: usize,
    focus: usize,
) -> Result<Vec<Rect>, EngineError> {
    let Some(target) = tree.arena.get(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.kind.text().is_none() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("node is not a text node"),
        });
    }
    let Some(slot) = tree.text_slot(node) else {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("text has not been shaped yet"),
        });
    };
    let Some(broken) = slot.broken() else {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("text has no final layout yet"),
        });
    };
    Ok(crate::text::inline_flow::selection_rects(
        broken,
        anchor,
        focus,
        slot.shaped().scale(),
    ))
}

// 消费者：`impl NodeTree::text_caret_rect` 的薄委托（外部调用点见该方法文档）。
/// `byte_index` 处的文本插入点矩形（文本内容盒局部逻辑像素）。
///
/// 与 [`NodeTree::text_selection_rects`] 共用 `TextSlot`；空文本没有 parley 行 metrics 时，
/// 高度回退到 `TextStyle.size × line_height`，宽度回退到 1 逻辑像素。
///
/// # Errors
/// `StaleNode`：句柄陈旧；`TextShape`：不是文本节点、尚未 shape、或 `byte_index` 越界/不在 char boundary。
pub(crate) fn text_caret_rect(
    tree: &NodeTree,
    node: NodeId,
    byte_index: usize,
) -> Result<Rect, EngineError> {
    let Some(target) = tree.arena.get(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.kind.text().is_none() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("node is not a text node"),
        });
    }
    let Some(slot) = tree.text_slot(node) else {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("text has not been shaped yet"),
        });
    };
    let Some(broken) = slot.broken() else {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("text has no final layout yet"),
        });
    };
    let shaped = slot.shaped();
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let text = content.text.as_str();
    if shaped.text() != text
        || shaped.style() != &content.style
        || shaped.inline_boxes() != content.inline_boxes.as_ref()
    {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static(
                "text content/style changed since the last shape; wait for the text pass",
            ),
        });
    }
    if byte_index > text.len() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("byte index is past the end of the text"),
        });
    }
    if !text.is_char_boundary(byte_index) {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("byte index is not a char boundary"),
        });
    }
    let expected_width = {
        let width = tree.text_content_width(node);
        if width > 0.0 {
            width
        } else {
            shaped.widths().max / shaped.scale()
        }
    };
    if slot.laid_width_bits() != expected_width.to_bits() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static(
                "text layout width changed since the last text pass; wait for the text pass",
            ),
        });
    }

    let mut rect = crate::text::inline_flow::caret_rect(broken, byte_index, shaped.scale());
    if rect.size.width <= 0.0 {
        rect.size.width = 1.0;
    }
    if rect.size.height <= 0.0 {
        let style_size = content.style.size.max(1.0);
        let line_height = content.style.line_height.unwrap_or(1.0).max(0.0);
        rect.size.height = (style_size * line_height).max(1.0);
    }
    Ok(rect)
}

// 消费者：`impl NodeTree::invalidate_text_layout` 的薄委托（外部调用点见该方法文档）。
/// 文本节点的布局失效（= `invalidate_measure` + 补 `PAINT`，F-56）。
pub(crate) fn invalidate_text_layout(tree: &mut NodeTree, node: NodeId) -> bool {
    if tree.invalidate_measure(node).is_err() {
        return false;
    }
    tree.mark_paint(node)
}

// 消费者：`impl NodeTree::mark_all_text_dirty` 的薄委托（外部调用点见该方法文档）。
/// 把所有文本节点标脏（字体栈 / DPI / 图集换代时用）。
pub(crate) fn mark_all_text_dirty(tree: &mut NodeTree) {
    let nodes: Vec<NodeId> = tree
        .order
        .iter()
        .copied()
        .filter(|node| {
            tree.arena
                .get(*node)
                .is_some_and(|t| t.kind.text().is_some())
        })
        .collect();
    for node in nodes {
        tree.mark_text_dirty(node);
    }
}

// 消费者：`impl NodeTree::mark_all_text_paint` 的薄委托（外部调用点见该方法文档）。
/// 把所有文本节点标 `PAINT`（**不动** `LAYOUT`），P1-2.6。
///
/// 用途：字形图集是**进程级**共享的，任一窗口的 DPI 变化或容量逃逸都会让
/// `AtlasAllocator::generation` 递增、页内所有 tile 重排。此时每个窗口文本节点的
/// `ShapedLine` 里存的是旧 `AtlasTile`（旧 UV），而 paint 的 lane 复用只认
/// `REUSABLE`/`clip_key` —— 不标 `PAINT` 就会继续画旧 UV，表现为错字/花字。
///
/// 只标 `PAINT` 不标 `LAYOUT`：图集换代不改变字形宽度/行数，不需要重排（重排是纯损失）。
/// 返回被标记的节点数（诊断 / 测试）。
pub(crate) fn mark_all_text_paint(tree: &mut NodeTree) -> usize {
    let nodes: SmallVec<[NodeId; 32]> = tree
        .order
        .iter()
        .copied()
        .filter(|node| {
            tree.arena
                .get(*node)
                .is_some_and(|t| t.kind.text().is_some())
        })
        .collect();
    let mut marked = 0;
    for node in nodes {
        if tree.mark_paint(node) {
            marked += 1;
        }
    }
    marked
}

// 消费者：`impl NodeTree::take_bind_text` 的薄委托（外部调用点见该方法文档）。
/// 取走 `bind_text` 闭包（求值期间脱离节点，避免借用冲突）。
pub(crate) fn take_bind_text(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>> {
    tree.arena.get_mut(node)?.text_bind.take()
}

// 消费者：`impl NodeTree::put_bind_text` 的薄委托（外部调用点见该方法文档）。
/// 放回 `bind_text` 闭包。
pub(crate) fn put_bind_text(
    tree: &mut NodeTree,
    node: NodeId,
    bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.text_bind = Some(bind_text);
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::take_geometry_damage_escalation` 的薄委托（外部调用点见该方法文档）。
/// 取走「几何 damage 需要升级为全帧」的标记（`Window::paint_frame` 每帧读一次）。
pub(crate) fn take_geometry_damage_escalation(tree: &mut NodeTree) -> bool {
    std::mem::take(&mut tree.geometry_damage_escalation)
}

// 消费者：`impl NodeTree::content_offset` 的薄委托（外部调用点见该方法文档）。
/// 内容盒相对 border box 左上角的偏移（消费者：`paint/pass.rs` 的文本绝对坐标）。
pub(crate) fn content_offset(tree: &NodeTree, node: NodeId) -> Point {
    tree.arena
        .get(node)
        .map_or(Point::ZERO, |target| target.content_offset)
}

// 消费者：`impl NodeTree::lane_of` 的薄委托（外部调用点见该方法文档）。
// ── P06：lane / clip 上下文 / 帧结算 ──────────────────────────────────────

/// 节点的绘制桶句柄（`P06` 每节点最多一条；消费者：`paint/pass.rs`）。
pub(crate) fn lane_of(tree: &NodeTree, node: NodeId) -> Option<LaneId> {
    tree.arena
        .get(node)
        .and_then(|target| target.lanes.first().copied())
}

// 消费者：`impl NodeTree::set_lane` 的薄委托（外部调用点见该方法文档）。
/// 记录节点的绘制桶（首个 lane；消费者：`paint/pass.rs`）。
pub(crate) fn set_lane(tree: &mut NodeTree, node: NodeId, lane: LaneId) {
    if let Some(target) = tree.arena.get_mut(node) {
        if target.lanes.is_empty() {
            target.lanes.push(lane);
        } else {
            target.lanes[0] = lane;
        }
    }
}

// 消费者：`impl NodeTree::lane_offset` 的薄委托（外部调用点见该方法文档）。
/// 该节点 lane 上次生成时的累计滚动偏移（paint 的复用判据）。
pub(crate) fn lane_offset(tree: &NodeTree, node: NodeId) -> Point {
    tree.arena
        .get(node)
        .map_or(Point::ZERO, |target| target.lane_offset)
}

// 消费者：`impl NodeTree::set_lane_offset` 的薄委托（外部调用点见该方法文档）。
/// 写 lane 的偏移戳（paint 重建 lane 时调用）。
pub(crate) fn set_lane_offset(tree: &mut NodeTree, node: NodeId, offset: Point) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lane_offset = offset;
    }
}

// 消费者：`impl NodeTree::clip_key` 的薄委托（外部调用点见该方法文档）。
/// 最近一次重建 lane 时的 clip 上下文 `(depth, covering)`（P06 §5.6 的 B1 守卫）。
pub(crate) fn clip_key(tree: &NodeTree, node: NodeId) -> Option<(u8, Option<PhysicalRect>)> {
    tree.clip_keys.get(node.index() as usize).copied().flatten()
}

// 消费者：`impl NodeTree::set_clip_key` 的薄委托（外部调用点见该方法文档）。
/// 写回本帧的 clip 上下文（消费者：`paint/pass.rs`）。
pub(crate) fn set_clip_key(tree: &mut NodeTree, node: NodeId, key: (u8, Option<PhysicalRect>)) {
    let index = node.index() as usize;
    if tree.clip_keys.len() <= index {
        tree.clip_keys.resize_with(index + 1, || None);
    }
    tree.clip_keys[index] = Some(key);
}

// 消费者：`impl NodeTree::clear_clip_key` 的薄委托（外部调用点见该方法文档）。
/// 清空 clip 上下文（隐藏子树用：再次显示时必然失配 ⇒ 重建，避免复用空 lane）。
pub(crate) fn clear_clip_key(tree: &mut NodeTree, node: NodeId) {
    let index = node.index() as usize;
    if let Some(slot) = tree.clip_keys.get_mut(index) {
        *slot = None;
    }
}

// 消费者：`impl NodeTree::set_reusable` 的薄委托（外部调用点见该方法文档）。
/// 标记节点可整桶复用（lane 内容与本帧的树/样式一致）。消费者：`paint/pass.rs`。
pub(crate) fn set_reusable(tree: &mut NodeTree, node: NodeId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.dirty.insert(DirtyFlags::REUSABLE);
    }
}

// 消费者：`impl NodeTree::clear_all_reusable` 的薄委托（外部调用点见该方法文档）。
/// 全树清 `REUSABLE`（消费者：`Window::force_full_repaint` 的 oracle 全量路径）。
#[cfg(feature = "test-support")]
pub(crate) fn clear_all_reusable(tree: &mut NodeTree) {
    for id in &tree.order {
        if let Some(target) = tree.arena.get_mut(*id) {
            target.dirty.remove(DirtyFlags::REUSABLE);
        }
    }
}

// 消费者：`impl NodeTree::clear_lane` 的薄委托（外部调用点见该方法文档）。
/// 丢弃节点的 lane 句柄（不放回 `Scene`；重建路径在"无图元"时先 `Scene::release_lane` 再调它）。
pub(crate) fn clear_lane(tree: &mut NodeTree, node: NodeId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.clear();
    }
}

// 消费者：`impl NodeTree::drain_removed_lanes` 的薄委托（外部调用点见该方法文档）。
/// 取走卸载节点的待释放 lane（消费者：`Window::paint_frame`；**借用式**清空，容量保留）。
pub(crate) fn drain_removed_lanes(tree: &mut NodeTree) -> std::vec::Drain<'_, LaneId> {
    tree.removed_lanes.drain(..)
}

// 消费者：`impl NodeTree::a11y_of` 的薄委托（外部调用点见该方法文档）。
// ── P11：a11y 侧表与 A11Y 脏位的消费 ──────────────────────────────────────

/// a11y 载荷（`None` = 该节点没有声明任何 a11y 属性）。
pub(crate) fn a11y_of(tree: &NodeTree, node: NodeId) -> Option<&crate::a11y::tree::NodeA11y> {
    tree.a11y_slots
        .get(node.index() as usize)
        .and_then(|slot| slot.as_deref())
}

// 消费者：`impl NodeTree::take_bind_a11y` 的薄委托（外部调用点见该方法文档）。
/// 取走 `bind_a11y` 闭包（求值期间脱离节点，避免借用冲突）。
pub(crate) fn take_bind_a11y(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>> {
    tree.a11y_slots
        .get_mut(node.index() as usize)?
        .as_mut()?
        .bind
        .take()
}

// 消费者：`impl NodeTree::put_bind_a11y` 的薄委托（外部调用点见该方法文档）。
/// 放回 `bind_a11y` 闭包（求值后；panic 展开时闭包丢失 ⇒ 该节点退化为静态 a11y）。
pub(crate) fn put_bind_a11y(
    tree: &mut NodeTree,
    node: NodeId,
    bind: Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>,
) -> bool {
    match tree
        .a11y_slots
        .get_mut(node.index() as usize)
        .and_then(|slot| slot.as_mut())
    {
        Some(slot) => {
            slot.bind = Some(bind);
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::set_a11y_patch` 的薄委托（外部调用点见该方法文档）。
/// 写回 `bind_a11y` 的上次求值结果（槽位不存在时创建）。
pub(crate) fn set_a11y_patch(
    tree: &mut NodeTree,
    node: NodeId,
    patch: crate::a11y::A11yPatch,
) -> bool {
    if tree.arena.get(node).is_none() {
        return false;
    }
    let index = node.index() as usize;
    if tree.a11y_slots.len() <= index {
        tree.a11y_slots.resize_with(index + 1, || None);
    }
    let slot = &mut tree.a11y_slots[index];
    let slot = slot.get_or_insert_with(|| Box::new(crate::a11y::tree::NodeA11y::new(None, None)));
    slot.patch = patch;
    true
}

// 消费者：`impl NodeTree::has_a11y_bind` 的薄委托（外部调用点见该方法文档）。
/// 该节点是否有 a11y 动态绑定（`bind_a11y`）。
pub(crate) fn has_a11y_bind(tree: &NodeTree, node: NodeId) -> bool {
    tree.a11y_slots
        .get(node.index() as usize)
        .and_then(|slot| slot.as_ref())
        .is_some_and(|slot| slot.bind.is_some())
}

// 消费者：`impl NodeTree::take_bind_style` 的薄委托（外部调用点见该方法文档）。
/// 取走动态属性闭包（求值期；消费者：`App::refresh_styles`）。
pub(crate) fn take_bind_style(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>> {
    tree.arena.get_mut(node)?.bind_style.take()
}

// 消费者：`impl NodeTree::put_bind_style` 的薄委托（外部调用点见该方法文档）。
/// 放回动态属性闭包（求值后；panic 展开时闭包丢失 → 该节点退化为静态节点）。
pub(crate) fn put_bind_style(
    tree: &mut NodeTree,
    node: NodeId,
    bind_style: Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>,
) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.bind_style = Some(bind_style);
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::deps` 的薄委托（外部调用点见该方法文档）。
/// 上次求值的依赖（`replace_deps` 的 `old`；关窗退订）。
pub(crate) fn deps(tree: &NodeTree, node: NodeId) -> Option<&[SignalId]> {
    tree.arena.get(node).map(|target| target.deps.as_slice())
}

// 消费者：`impl NodeTree::set_deps` 的薄委托（外部调用点见该方法文档）。
/// 写回本次求值的依赖。
pub(crate) fn set_deps(tree: &mut NodeTree, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.deps = deps;
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::set_style_patch` 的薄委托（外部调用点见该方法文档）。
/// 写回动态属性补丁。
pub(crate) fn set_style_patch(tree: &mut NodeTree, node: NodeId, patch: StylePatch) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.style_patch = patch;
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::has_visual_bind` 的薄委托（外部调用点见该方法文档）。
/// 该节点是否有**视觉**动态绑定（`bind_style` / `bind_text`）。
pub(crate) fn has_visual_bind(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena
        .get(node)
        .is_some_and(|target| target.bind_style.is_some() || target.text_bind.is_some())
}

// 消费者：`impl NodeTree::interaction` 的薄委托（外部调用点见该方法文档）。
// ── P10：交互槽的读写（消费者：`interaction/**`、`app.rs::dispatch_input`、`paint/pass.rs`） ──

/// 交互槽（`None` = 非交互节点）。
pub(crate) fn interaction(tree: &NodeTree, node: NodeId) -> Option<&NodeInteraction> {
    tree.arena
        .get(node)
        .and_then(|target| target.interaction.as_deref())
}

// 消费者：`impl NodeTree::ensure_interaction` 的薄委托（外部调用点见该方法文档）。
/// 确保交互槽存在（滚动 / 虚拟列表 / 浮层在挂载后挂槽的唯一入口）。
pub(crate) fn ensure_interaction(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<&mut NodeInteraction> {
    tree.arena
        .get_mut(node)
        .map(|target| {
            target
                .interaction
                .get_or_insert_with(|| Box::new(NodeInteraction::new()))
        })
        .map(Box::as_mut)
}

// 消费者：`impl NodeTree::is_hittable` 的薄委托（外部调用点见该方法文档）。
/// 该节点是否可成为命中目标（`interactive == Some(false)` ⇒ 否；子节点照旧）。
pub(crate) fn is_hittable(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena
        .get(node)
        .is_some_and(|target| target.style_patch.interactive != Some(false))
}

// 消费者：`impl NodeTree::focus_ring` 的薄委托（外部调用点见该方法文档）。
/// 焦点环（`None` = 不画）。
pub(crate) fn focus_ring(tree: &NodeTree, node: NodeId) -> Option<crate::paint::style::FocusRing> {
    tree.interaction(node).and_then(|slot| slot.focus_ring)
}

// 消费者：`impl NodeTree::is_explicitly_interactive` 的薄委托（外部调用点见该方法文档）。
/// 该节点是否被显式标为可交互（`interactive == Some(true)`）。
pub(crate) fn is_explicitly_interactive(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena
        .get(node)
        .is_some_and(|target| target.style_patch.interactive == Some(true))
}

// 消费者：`impl NodeTree::scroll_slot` 的薄委托（外部调用点见该方法文档）。
/// 滚动槽（`None` = 不是滚动容器）。
pub(crate) fn scroll_slot(tree: &NodeTree, node: NodeId) -> Option<ScrollSlot> {
    tree.interaction(node).and_then(|slot| slot.scroll)
}

// 消费者：`impl NodeTree::scroll_slot_mut` 的薄委托（外部调用点见该方法文档）。
/// 滚动槽的可写引用。
pub(crate) fn scroll_slot_mut(tree: &mut NodeTree, node: NodeId) -> Option<&mut ScrollSlot> {
    tree.ensure_interaction(node)
        .and_then(|slot| slot.scroll.as_mut())
}

// 消费者：`impl NodeTree::overlay_slot` 的薄委托（外部调用点见该方法文档）。
/// 浮层槽（`None` = 不是浮层）。
pub(crate) fn overlay_slot(tree: &NodeTree, node: NodeId) -> Option<OverlaySlot> {
    tree.interaction(node).and_then(|slot| slot.overlay)
}

// 消费者：`impl NodeTree::virtual_slot` 的薄委托（外部调用点见该方法文档）。
/// 虚拟列表槽。
pub(crate) fn virtual_slot(tree: &NodeTree, node: NodeId) -> Option<&VirtualSlot> {
    tree.interaction(node)
        .and_then(|slot| slot.virtual_list.as_ref())
}

// 消费者：`impl NodeTree::virtual_slot_mut` 的薄委托（外部调用点见该方法文档）。
/// 虚拟列表槽的可写引用。
pub(crate) fn virtual_slot_mut(tree: &mut NodeTree, node: NodeId) -> Option<&mut VirtualSlot> {
    tree.ensure_interaction(node)
        .and_then(|slot| slot.virtual_list.as_mut())
}

// 消费者：`impl NodeTree::set_style_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：直接替换静态样式。
#[cfg(test)]
pub(crate) fn set_style_for_test(tree: &mut NodeTree, node: NodeId, style: Style) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.style = style;
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::set_style_patch_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：直接写样式补丁（paint pass 的 visible/opacity/background 用例需要）。
#[cfg(test)]
pub(crate) fn set_style_patch_for_test(
    tree: &mut NodeTree,
    node: NodeId,
    patch: StylePatch,
) -> bool {
    tree.set_style_patch(node, patch)
}

// 消费者：`impl NodeTree::edit_style_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：就地编辑静态样式。
#[cfg(test)]
pub(crate) fn edit_style_for_test(
    tree: &mut NodeTree,
    node: NodeId,
    edit: impl FnOnce(&mut Style),
) {
    if let Some(target) = tree.arena.get_mut(node) {
        edit(&mut target.style);
    }
}

// 消费者：本模块的文本方法（`set_text_shaped` / `set_text_line` / `text_*`）。
/// 一个文本节点的快照（P05）：`shaped` 供 measure/选区，`line` 供 P06 的 paint。
///
/// `key`/`scale_epoch`/`laid_*` 是「本帧是否需要重做」的判据（B4）：三者全等 ⇒ 整节点跳过。
pub(crate) struct TextSlot {
    shaped: Rc<crate::text::shape::ShapedText>,
    key: crate::text::cache::ShapeKey,
    scale_epoch: u32,
    line: Option<Rc<ShapedLine>>,
    /// 最终（按内容宽断行 + 对齐）的 parley 布局：选区映射的唯一真相（B1）。
    broken: Option<Box<parley::Layout<crate::paint::color::ColorBrush>>>,
    laid_width_bits: u32,
    laid_generation: u32,
}

impl TextSlot {
    pub(crate) fn shaped(&self) -> &Rc<crate::text::shape::ShapedText> {
        &self.shaped
    }

    pub(crate) fn key(&self) -> crate::text::cache::ShapeKey {
        self.key
    }

    pub(crate) fn scale_epoch(&self) -> u32 {
        self.scale_epoch
    }

    pub(crate) fn line(&self) -> Option<&Rc<ShapedLine>> {
        self.line.as_ref()
    }

    pub(crate) fn laid_width_bits(&self) -> u32 {
        self.laid_width_bits
    }

    pub(crate) fn laid_generation(&self) -> u32 {
        self.laid_generation
    }

    pub(crate) fn broken(&self) -> Option<&parley::Layout<crate::paint::color::ColorBrush>> {
        self.broken.as_deref()
    }
}
