//! 节点内容与旁表读写：文本 / a11y 载荷 / 绑定闭包 / 交互槽 / lane / clip。

use std::{rc::Rc, sync::Arc};

use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{BakedGeometryChunk, NodeKind, NodeTree};
#[cfg(test)]
use crate::paint::style::Style;
use crate::{
    error::EngineError,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    math::{PhysicalRect, Point, Rect, Size, Transform},
    node::{dependency::ReadCtx, dirty::DirtyFlags, id::NodeId, signal::SignalId},
    paint::{geometry::GeometryId, scene::LaneId, style::StylePatch},
    text::{ShapedLine, TextContent, shape::ShapedText},
};

// 消费者：`impl NodeTree::text_content` 的薄委托（外部调用点见该方法文档）。
// ── P05：文本（载荷 / 形状快照 / 选区） ───────────────────────────────────

/// 文本载荷（`None` = 不是文本节点）。消费者：`App::refresh_styles` / `App::refresh_text`。
pub(crate) fn text_content(tree: &NodeTree, node: NodeId) -> Option<&TextContent> {
    tree.arena.get(node).and_then(|target| target.kind.text())
}

// 消费者：`impl NodeTree::set_canvas` 的薄委托（外部调用点见该方法文档）。
/// 替换 `Canvas` 节点的画布内容（D-6：异步图片交付的落地入口）。
///
/// 只接受 `NodeKind::Canvas` 节点；替换后标该节点**子树** `PAINT`（lane 重建 ⇒ damage 来自
/// 重建后的图元，不会退化成整窗重绘）。不改变布局：Canvas 节点尺寸仍由布局决定。
/// `AtlasTile` 的生命周期由调用方持有的 `ResourceImage` 决定 —— 引擎不会在替换时保留/延长
/// 旧 tile，也不会自动 `unload` 旧资源。
///
/// **几何绑定表同步重建**：Canvas 里可以引用任意多个 `GeometryId`（`push_geometry` /
/// `push_path`），这些引用登记在 `geometry_bindings` 反向索引里，几何数据 range 更新时
/// Canvas 节点因此和数据更新后的自动标脏共用同一条路径。旧内容的引用在这里解绑、新内容的
/// 引用重新登记（同一个 id 同时在旧新内容里时先解后绑，语义不变）。扫描只在**替换**时发生
/// （异步图片交付，不是热路径）。
///
/// 返回 `false` = 节点不存在 / 不是 Canvas（**不**改动任何状态）。
pub(crate) fn set_canvas(
    tree: &mut NodeTree,
    node: NodeId,
    canvas: crate::paint::canvas::Canvas,
) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    let NodeKind::Canvas(existing) = &mut target.kind else {
        return false;
    };
    let fresh = Arc::new(canvas);
    let previous = std::mem::replace(existing, Arc::clone(&fresh));
    for geometry in canvas_geometry_ids(&previous) {
        unbind_geometry_node(tree, geometry, node);
    }
    for geometry in canvas_geometry_ids(&fresh) {
        bind_geometry_node(tree, geometry, node);
    }
    tree.mark_subtree_paint(node);
    true
}

/// `Canvas` 内容里引用的几何 id（`Primitive::Geometry` / `Primitive::Path`，去重保序）。
///
/// 消费者：`arena::insert_child` / [`set_canvas`] / `arena::remove` 维护反向绑定表
/// `geometry_bindings`，让「Canvas 里画的几何」和几何节点走同一条自动标脏路径。
/// **不在 paint 热路径调用**：Canvas 引用集合只在挂载 / 内容替换 / 卸载时扫描。
pub(crate) fn canvas_geometry_ids(
    canvas: &crate::paint::canvas::Canvas,
) -> SmallVec<[GeometryId; 2]> {
    let mut ids: SmallVec<[GeometryId; 2]> = SmallVec::new();
    for primitive in canvas.primitives() {
        let id = match primitive {
            crate::paint::primitive::Primitive::Geometry(draw) => draw.id,
            crate::paint::primitive::Primitive::Path(path) => path.geometry,
            _ => continue,
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

// 消费者：`impl NodeTree::set_text_content` 的薄委托（外部调用点见该方法文档）。
/// 写回文本载荷（`bind_text` 求值 + 静态 `text()` 的挂载路径）。
pub(crate) fn set_text_content(tree: &mut NodeTree, node: NodeId, content: TextContent) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.kind {
            NodeKind::Text(existing) => {
                **existing = content;
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
/// **O(1) 去重**（R12-13）：旧实现用 `Vec::contains`（O(n)），行情表整表 tick 时一帧 5000 次
/// 插入 ⇒ 约 1250 万次 `NodeId` 比较（5–15 ms 纯浪费）。现在用一个与队列同寿命的
/// `ahash::HashSet` 去重：插入 O(1)，`take` 时一次性清空（清空 = `clear()`，容量保留）。
pub(crate) fn mark_text_dirty(tree: &mut NodeTree, node: NodeId) {
    tree.text_dirty.push(node);
}

// 消费者：`impl NodeTree::take_text_dirty` 的薄委托（外部调用点见该方法文档）。
/// 把本帧待 shape 的文本节点追加进 `out` 并清空集合（**两侧容量都保留**）。
///
/// 消费者：`App::refresh_text`。返回的列表可能包含已被卸载的节点 —— 调用方按
/// 「查不到就跳过」处理（卸载时 `remove` 会尝试清理，但集合是旁路结构，不做强一致）。
pub(crate) fn take_text_dirty(tree: &mut NodeTree, out: &mut Vec<NodeId>) {
    tree.text_dirty.take_into(out);
}

// 消费者：`impl NodeTree::text_dirty_len` 的薄委托（外部调用点见该方法文档）。
/// 本帧待 shape 的文本节点数（诊断 / 测试）。
#[cfg(test)]
pub(crate) fn text_dirty_len(tree: &NodeTree) -> usize {
    tree.text_dirty.len()
}

// 消费者：`impl NodeTree::mark_text_layout_dirty` 的薄委托（外部调用点见该方法文档）。
/// 登记「该文本节点的 `ShapedLine` 需要重建」（R12-14 的第二阶段脏集合）。
pub(crate) fn mark_text_layout_dirty(tree: &mut NodeTree, node: NodeId) {
    tree.text_layout_dirty.push(node);
}

// 消费者：`impl NodeTree::take_text_layout_dirty` 的薄委托（外部调用点见该方法文档）。
/// 取走本帧需要重建 `ShapedLine` 的文本节点集合（消费后清空，容量保留）。
pub(crate) fn take_text_layout_dirty(tree: &mut NodeTree, out: &mut Vec<NodeId>) {
    tree.text_layout_dirty.take_into(out);
}

// 消费者：`impl NodeTree::text_layout_dirty_len` 的薄委托（外部调用点见该方法文档）。
/// 本帧待重排的文本节点数（诊断 / 测试）。
#[cfg(test)]
pub(crate) fn text_layout_dirty_len(tree: &NodeTree) -> usize {
    tree.text_layout_dirty.len()
}

// 消费者：`impl NodeTree::mark_canvas_nodes_paint` 的薄委托（外部调用点见该方法文档）。
/// 让所有 `Canvas` 节点的 lane 重建（v2.1 E1b：图集换代后的 canvas 编码期安全网入口；
/// E1d 起逐 tile LRU 淘汰的收尾同样调用它）。
///
/// Canvas 图元里的 `GlyphRun` 同样烘焙了 `AtlasTile`（应用经 `Canvas::push_shaped_text` 写入）。
/// 图集换代 / 逐 tile 淘汰后 lane 里缓存的 Canvas 值仍是旧 UV，而 `invalidate_text_lines` 只遍历
/// 文本节点 —— 不标 PAINT 的话 paint 会整桶复用它，编码期逐 tile 判活（`paint/pass.rs`）根本
/// 不会执行。标 PAINT 让 paint 重新 emit：失效 run 被丢弃（少字优于花字），节点计入诊断计数。
///
/// 返回被标记的节点数（0 = 场景里没有 Canvas 节点，冷路径不产生额外开销）。
pub(crate) fn mark_canvas_nodes_paint(tree: &mut NodeTree) -> usize {
    let mut nodes: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|node| {
        if tree
            .arena
            .get(node)
            .is_some_and(|target| matches!(target.kind(), NodeKind::Canvas(_)))
        {
            nodes.push(node);
        }
    });
    let marked = nodes.len();
    for node in nodes {
        tree.mark_paint(node);
    }
    marked
}

// 消费者：`impl NodeTree::invalidate_text_lines` 的薄委托（外部调用点见该方法文档）。
/// 让所有文本节点的 `ShapedLine` 失效并进入重排队列（图集逐 tile 淘汰 / 字体换代用）。
///
/// 与 [`invalidate_text_layout`] 的区别：这里**不动 measure 闭包、不置 `LAYOUT`** —— 断行宽度
/// 与字形宽度都没变，只需要重新光栅装箱。返回被失效的节点数。
pub(crate) fn invalidate_text_lines(tree: &mut NodeTree) -> usize {
    let mut nodes: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|node| {
        if tree
            .arena
            .get(node)
            .is_some_and(|target| target.kind.text().is_some())
        {
            nodes.push(node);
        }
    });
    let mut invalidated = 0;
    for node in nodes {
        let index = node.index() as usize;
        let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
            continue;
        };
        if slot.line.take().is_some() {
            invalidated += 1;
        }
        slot.laid_width_bits = 0;
        slot.laid_generation = 0;
        tree.mark_text_layout_dirty(node);
        tree.mark_paint(node);
    }
    invalidated
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
            // 新快照 ⇒ 旧的断行/布局依据全部作废（B4）：`layout_text` 的复用判据是
            // `line.is_some() && width_bits && generation`，任一不成立就重排。
            slot.line = None;
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
                    laid_width_bits: 0,
                    laid_generation: 0,
                });
            }
        }
    }
    // R12-14：只有「新快照」的节点需要重建 `ShapedLine`，因此只把它们入重排队列
    // （第二阶段不再每帧遍历整棵树）。
    tree.mark_text_layout_dirty(node);

    tree.invalidate_caches_upwards(node);
    if size_changed {
        tree.mark_layout(node)
    } else {
        tree.mark_paint(node)
    }
}

// 消费者：`impl NodeTree::set_text_line` 的薄委托（外部调用点见该方法文档）。
/// 挂上只读快照（P06 的 paint 输入）；`width`/`generation` 记下这次重排的依据。
///
/// **不再常驻第二份 parley `Layout`**（R12-6）：选区/光标几何在需要时按 `laid_width_bits`
/// 就地重排（`ShapedText` 内部一份 `Layout`，零 clone），常态下每个文本节点只留
/// `Rc<ShapedLine>`。
pub(crate) fn set_text_line(
    tree: &mut NodeTree,
    node: NodeId,
    line: Rc<ShapedLine>,
    width: f32,
    generation: u32,
) -> bool {
    let index = node.index() as usize;
    let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
        return false;
    };
    slot.line = Some(line);
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
    let shaped = slot.shaped();
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let width = expected_layout_width(tree, shaped, node);
    Ok(shaped.selection_rects(width, content.style.alignment, anchor, focus))
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
    let shaped = slot.shaped();
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let text = content.text.as_str();
    if shaped.text() != text
        || !shaped.same_shaping_inputs(&content.style)
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
    let expected_width = expected_layout_width(tree, shaped, node);
    if slot.laid_width_bits() != expected_width.to_bits() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static(
                "text layout width changed since the last text pass; wait for the text pass",
            ),
        });
    }

    let mut rect = shaped.caret_rect(expected_width, content.style.alignment, byte_index);
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

/// 断行宽度（逻辑像素）：taffy 算出的内容宽，未布局时退回 max-content 宽。
///
/// 与 `layout_text` 的取值完全一致 —— 选区/光标查询必须与该次重排用同一个宽度，
/// 否则 `caret_rect` 的宽度校验会误报「wait for the text pass」。
fn expected_layout_width(tree: &NodeTree, shaped: &ShapedText, node: NodeId) -> f32 {
    let width = tree.text_content_width(node);
    if width > 0.0 {
        width
    } else {
        shaped.max_content_size().width
    }
}

// 消费者：`impl NodeTree::text_byte_index_at` 的薄委托（外部调用点见该方法文档）。
/// 文本盒局部坐标（与 [`NodeTree::text_caret_rect`] 的输出同域）→ 最近合法字节边界（E5/§11.5）。
///
/// 只读：不登记依赖、不改任何节点/帧状态（I6）；断行与对齐在快照的 parley 布局上现场重跑
/// （与 `caret_rect` / `selection_rects` 同一条路径）。快速路径文本（单行 ASCII）不经过 parley，
/// 但返回同一套「半簇取整 + 垂直 clamp」语义。
///
/// # Errors
/// `StaleNode`：节点不存在 / 不是 `NodeKind::Text` / 尚未 shape 或 `ShapedLine` 尚未重建 /
/// 文本内容、样式或断行宽度在该次文本 pass 之后又变了（等下一帧再查）。
pub(crate) fn text_byte_index_at(
    tree: &NodeTree,
    node: NodeId,
    local_point: Point,
) -> Result<usize, EngineError> {
    let Some(target) = tree.arena.get(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.kind.text().is_none() {
        return Err(EngineError::StaleNode { node });
    }
    let Some(slot) = tree.text_slot(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let shaped = slot.shaped();
    // 「尚无 ShapedLine」与「快照不是当前内容」都不能作为命中目标（§11.5 的错误语义）：
    // 返回旧快照算出的索引会让应用把光标锚到错误的字符上。
    if tree.text_line(node).is_none()
        || shaped.text() != content.text.as_str()
        || !shaped.same_shaping_inputs(&content.style)
        || shaped.inline_boxes() != content.inline_boxes.as_ref()
    {
        return Err(EngineError::StaleNode { node });
    }
    let width = expected_layout_width(tree, shaped, node);
    if slot.laid_width_bits() != width.to_bits() {
        return Err(EngineError::StaleNode { node });
    }
    Ok(shaped.byte_index_at(width, content.style.alignment, local_point))
}

// 消费者：`impl NodeTree::invalidate_text_line` 的薄委托（外部调用点见该方法文档）。
/// 让**单个**文本节点的 ShapedLine 失效并进入重排队列（D-5）。
///
/// 与整树的 [invalidate_text_lines] 同语义（清 line + laid_* 失配 + mark_paint），但只动
/// 一个节点：绘制前硬校验的 stale 集合通常只有 0~几个节点，没必要整窗重排。
pub(crate) fn invalidate_text_line(tree: &mut NodeTree, node: NodeId) {
    let index = node.index() as usize;
    let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
        return;
    };
    slot.line = None;
    slot.laid_width_bits = 0;
    slot.laid_generation = 0;
    tree.mark_text_layout_dirty(node);
    tree.mark_paint(node);
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
/// 把所有文本节点标脏（字体栈 / 系统字体 / 图集换代时用）。
///
/// 两个阶段都要重做：`shape`（第一阶段）与 `layout`（第二阶段，重建 `ShapedLine`）。
/// 这是冷路径（换字体 / 系统字体变化），因此允许一次 DFS 收集；稳态帧不经过这里。
pub(crate) fn mark_all_text_dirty(tree: &mut NodeTree) {
    let mut nodes: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|node| {
        if tree
            .arena
            .get(node)
            .is_some_and(|target| target.kind.text().is_some())
        {
            nodes.push(node);
        }
    });
    for node in nodes {
        tree.mark_text_dirty(node);
        tree.mark_text_layout_dirty(node);
    }
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

// 消费者：`impl NodeTree::geometry_transform` 的薄委托（外部调用点见该方法文档）。
/// 几何节点的 per-draw 变换（R05-12）。未设置 / 非几何节点 ⇒ 单位矩阵。
pub(crate) fn geometry_transform(tree: &NodeTree, node: NodeId) -> Transform {
    tree.geometry_transforms
        .get(&node)
        .copied()
        .unwrap_or(Transform::IDENTITY)
}

// 消费者：`impl NodeTree::has_geometry_transforms` 的薄委托（外部调用点见该方法文档）。
/// 是否存在任何 per-draw 几何变换（O(1)；命中测试的"快路径"判定：通常一张窗口一个都没有）。
pub(crate) fn has_geometry_transforms(tree: &NodeTree) -> bool {
    !tree.geometry_transforms.is_empty()
}

// 消费者：`impl NodeTree::set_geometry_transform` 的薄委托（外部调用点见该方法文档）。
/// 写入几何节点的 per-draw 变换（单位矩阵 = 删除条目）。
///
/// 标 `PAINT`：变换烘在 lane 的 `Primitive::Geometry` 里，因此 lane 必须重建（顶点缓冲按
/// `(index, generation)` 缓存，不会被这次改动触碰）。
///
/// `extra_damage` 是调用方（`app::geometry`）算出的**两侧**可见范围并集（旧变换 ∪ 新变换下的
/// `GeometryDesc.bounds`，**内容坐标**）：`touch` 只能并入节点布局盒，而几何平移/缩放后旧像素
/// 可能落在盒外，必须由这里补上，否则局部刷新会留残影（AR2 P1-D4）。传空矩形 = 没有额外范围。
/// 走的是**同一条** damage 记账（节点的 `damage` 累加器），不是第二条 damage 路径。
///
/// # Errors
/// [`EngineError::StaleNode`]：节点不存在或不是几何节点。
pub(crate) fn set_geometry_transform(
    tree: &mut NodeTree,
    node: NodeId,
    transform: Transform,
    extra_damage: Rect,
) -> Result<(), EngineError> {
    match tree.arena.get(node).map(|target| target.kind()) {
        Some(NodeKind::Geometry(_)) => {}
        _ => return Err(EngineError::StaleNode { node }),
    }
    if transform == Transform::IDENTITY {
        tree.geometry_transforms.remove(&node);
    } else {
        tree.geometry_transforms.insert(node, transform);
    }
    tree.mark_paint(node);
    if let Some(target) = tree.arena.get_mut(node) {
        target.damage = target.damage.union(extra_damage);
    }
    Ok(())
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

/// 节点的**第一条**绘制桶句柄（消费者：paint 的单 lane 路径 / 测试）。
pub(crate) fn lane_of(tree: &NodeTree, node: NodeId) -> Option<LaneId> {
    tree.arena
        .get(node)
        .and_then(|target| target.lanes.first().copied())
}

/// lane 条数（几何节点 = 块数；消费者：paint 绘制序兜底 / 隐藏 / 卸载）。
pub(crate) fn lane_count(tree: &NodeTree, node: NodeId) -> usize {
    tree.arena.get(node).map_or(0, |target| target.lanes.len())
}

/// 第 `index` 条 lane（按块序；消费者：paint 绘制序兜底 / 隐藏 / 卸载）。
pub(crate) fn lane_at(tree: &NodeTree, node: NodeId, index: usize) -> Option<LaneId> {
    tree.arena
        .get(node)
        .and_then(|target| target.lanes.get(index).copied())
}

// 消费者：`impl NodeTree::set_lane` 的薄委托（外部调用点见该方法文档）。
/// 把 lane 集合改成**单条**（普通节点的唯一写法；几何节点用 `push_lane`）。
pub(crate) fn set_lane(tree: &mut NodeTree, node: NodeId, lane: LaneId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.clear();
        target.lanes.push(lane);
    }
}

// 消费者：`impl NodeTree::push_lane` 的薄委托（外部调用点见该方法文档）。
/// 追加一条 lane（几何节点的块 lane；调用方保证按块序追加）。
pub(crate) fn push_lane(tree: &mut NodeTree, node: NodeId, lane: LaneId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.push(lane);
    }
}

/// 截断 lane 集合到 `len`（多余 lane 由调用方先 `Scene::release_lane`；几何节点块数变化用）。
pub(crate) fn truncate_lanes(tree: &mut NodeTree, node: NodeId, len: usize) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.truncate(len);
    }
}

/// 该几何节点的块 lane 烘焙状态（与 `lanes` 一一对应；`None` = 还没画过）。
pub(crate) fn baked_geometry_chunks(
    tree: &NodeTree,
    node: NodeId,
) -> Option<&SmallVec<[BakedGeometryChunk; 2]>> {
    tree.geometry_chunks.get(&node)
}

/// 重置块 lane 烘焙状态的长度（首帧 / 块数变化；`0` = 删除条目）。
pub(crate) fn resize_baked_geometry_chunks(tree: &mut NodeTree, node: NodeId, len: usize) {
    if len == 0 {
        tree.geometry_chunks.remove(&node);
        return;
    }
    let entry = tree
        .geometry_chunks
        .entry(node)
        .or_insert_with(|| SmallVec::with_capacity(len));
    entry.resize(
        len,
        BakedGeometryChunk {
            revision: 0,
            bounds: Rect::default(),
        },
    );
}

/// 写一条块 lane 的烘焙状态（`chunk` 必须 < 已重置的长度）。
pub(crate) fn set_baked_geometry_chunk(
    tree: &mut NodeTree,
    node: NodeId,
    chunk: usize,
    baked: BakedGeometryChunk,
) {
    if let Some(entry) = tree.geometry_chunks.get_mut(&node)
        && let Some(slot) = entry.get_mut(chunk)
    {
        *slot = baked;
    }
}

// 消费者：`impl NodeTree::mark_geometry_updated` 的薄委托（外部调用点见该方法文档）。
/// 把绑定到 `geometry` 的节点标 `PAINT`，返回标脏的节点数。
///
/// 两类绑定节点的语义不同：
/// - **几何节点**（`NodeKind::Geometry`）：只登记脏集合，**不清 `REUSABLE`、不并节点盒
///   damage**。块 lane 的复用判据是「快照块 revision 是否变化」，paint 会对每个块按旧∪新
///   AABB 记 damage；清 `REUSABLE` 会把全部块 lane 拖去重建（K 线追加只该重建尾块）。
/// - **Canvas 节点**（`NodeKind::Canvas`）：Canvas 内容按引用重新编码（图元里是
///   `Primitive::Geometry`，没有块级 revision 判据），因此必须清 `REUSABLE` 并让 lane
///   重建；damage 由 `emit_content` 按 Canvas 内容范围记（整块 Canvas 重画，不按几何块切分）。
pub(crate) fn mark_geometry_updated(tree: &mut NodeTree, geometry: GeometryId) -> usize {
    let Some(nodes) = tree.geometry_bindings.get(&geometry) else {
        return 0;
    };
    // 绑定表通常是 1 个节点（几何 id 每图表一个）；克隆 SmallVec 避免「读绑定表 + 改脏集合」
    // 的借用冲突（`insert` 只碰 `dirty_paint`/`dirty_settle` 两个不同字段，但函数级 `&mut self`
    // 会整树借用）。
    let nodes: SmallVec<[NodeId; 2]> = nodes.iter().copied().collect();
    let mut touched = 0;
    for node in nodes {
        let Some(target) = tree.arena.get(node) else {
            continue;
        };
        let is_canvas = matches!(target.kind(), NodeKind::Canvas(_));
        if is_canvas {
            // Canvas 的图元没有块级 revision（引用的是整条几何），只能整节点重建。
            tree.mark_paint(node);
        } else {
            tree.dirty_paint.insert(node);
            tree.dirty_settle.insert(node);
        }
        touched += 1;
    }
    touched
}

/// 注册「几何 id → 引用它的节点」绑定（消费者：`arena::insert_child` 的几何 / Canvas 分支）。
pub(crate) fn bind_geometry_node(tree: &mut NodeTree, geometry: GeometryId, node: NodeId) {
    let entry = tree.geometry_bindings.entry(geometry).or_default();
    if !entry.contains(&node) {
        entry.push(node);
    }
}

/// 移除一个节点的全部反向绑定（消费者：`arena::remove` / Canvas 内容替换）。
pub(crate) fn unbind_geometry_node(tree: &mut NodeTree, geometry: GeometryId, node: NodeId) {
    if let Some(nodes) = tree.geometry_bindings.get_mut(&geometry) {
        nodes.retain(|existing| *existing != node);
        if nodes.is_empty() {
            tree.geometry_bindings.remove(&geometry);
        }
    }
}

/// 上次烘焙 lane 图元时扣掉的偏移（烘焙坐标 + 它 = 布局坐标；消费者：paint 的旧内容 damage）。
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

// 消费者：`impl NodeTree::paint_state` 的薄委托（外部调用点见该方法文档）。
/// 一次 arena 查找算出 paint 遍历需要的全部只读状态（见 [`super::PaintNodeState`]）。
///
/// `screen_offset` 是**祖先滚动偏移的累计值**（屏幕 = 内容 − offset；`clip_bounds` 用它）；
/// `bake_offset` 是图元真正烘焙用的偏移（**不含本节点所属滚动槽自己的偏移**，滚动因此不
/// 重建 lane）；`clip_key` 是本帧当前的祖先 clip 上下文。
pub(crate) fn paint_state(
    tree: &NodeTree,
    node: NodeId,
    screen_offset: Point,
    bake_offset: Point,
    clip_key: (u8, Option<PhysicalRect>),
) -> Option<super::PaintNodeState> {
    let target = tree.arena.get(node)?;
    let hidden = matches!(target.style.inner.display, taffy::style::Display::None)
        || target.style_patch().visible == Some(false);
    // 自绘节点（几何 / Canvas）不支持合成端滚动偏移：它们的图元必须烘焙成屏幕坐标。
    let screen_baked = matches!(
        target.kind(),
        super::NodeKind::Geometry(_) | super::NodeKind::Canvas(_)
    );
    let effective_bake = if screen_baked {
        screen_offset
    } else {
        bake_offset
    };
    let reusable = target.dirty.contains(DirtyFlags::REUSABLE)
        && tree.clip_key(node) == Some(clip_key)
        && target.lane_offset == effective_bake;
    let clip_bounds = (target.style.clip == crate::paint::style::Clip::Bounds).then(|| {
        target
            .layout
            .bounds
            .translate(Point::new(-screen_offset.x, -screen_offset.y))
            .inset(target.layout.border)
    });
    let scroll = target
        .interaction
        .as_deref()
        .and_then(|slot| slot.scroll)
        .map(|slot| slot.offset);
    let geometry = match target.kind() {
        NodeKind::Geometry(id) => Some(*id),
        _ => None,
    };
    Some(super::PaintNodeState {
        hidden,
        reusable,
        lane: target.lanes.first().copied(),
        geometry,
        children: target.children.len(),
        clip_bounds,
        scroll,
        screen_baked,
    })
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

// 消费者：`impl NodeTree::force_repaint_all` 的薄委托（外部调用点见该方法文档）。
/// 全树强制重绘（消费者：`Window::force_full_repaint` 的 oracle 全量路径 / 测试基建）。
///
/// 清 `REUSABLE` 并把**每个节点**登记进 `dirty_paint`：paint pass 的复用判据因此全部失配，
/// 逐节点重建（`clear_paint` 会把它们移出集合）。只清位不入集合会让
/// `needs_node_repaint`（O(1) 门禁）看不到工作 ⇒ 帧被判空闲（旧实现靠全树扫描才发现）。
#[cfg(feature = "test-support")]
pub(crate) fn force_repaint_all(tree: &mut NodeTree) {
    // A1：oracle 的「全量」路径必须真的走**全量遍历**（而不是把每个节点都塞进脏闭包让增量
    // 路径“看起来一样”）：绘制序 / 滚动槽编号都要按全量语义重建。
    tree.force_full_paint();
    let mut ids: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|id| ids.push(id));
    for id in ids {
        let _ = tree.mark_paint(id);
        if let Some(target) = tree.arena.get_mut(id) {
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

// 消费者：`impl NodeTree::set_deps` 的薄委托（外部调用点见该方法文档）。
/// 写回本次求值的依赖。
///
/// **复用容量**（R03-15）：旧实现整体赋值 `target.deps = deps`，每个「依赖数 > 内联容量」的
/// 节点每帧都会 free + alloc 一次；这里 `clear` + `extend_from_slice`，第二轮起零分配。
pub(crate) fn set_deps(tree: &mut NodeTree, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.deps {
            Some(existing) => {
                existing.clear();
                existing.extend_from_slice(&deps);
            }
            None => target.deps = Some(Box::new(deps)),
        },
        None => return false,
    }
    true
}

// 消费者：`impl NodeTree::set_style_patch` 的薄委托（外部调用点见该方法文档）。
/// 写回动态属性补丁（`None` 语义 = 空补丁，见 `EMPTY_STYLE_PATCH`）。
pub(crate) fn set_style_patch(tree: &mut NodeTree, node: NodeId, patch: StylePatch) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.style_patch {
            Some(existing) => **existing = patch,
            None => target.style_patch = Some(Box::new(patch)),
        },
        None => return false,
    }
    true
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
        .is_some_and(|target| target.style_patch().interactive != Some(false))
}

// 消费者：`impl NodeTree::is_effectively_interactive` 的薄委托（外部调用点见该方法文档）。
/// 该节点**及其所有祖先**都未被 `interactive: Some(false)` 禁用。
///
/// 这是「可交互」的**唯一**继承规则（命中 / 焦点 / Action 三处共用）：
/// - 命中测试沿途把 `disabled` 传下去，禁用子树整体不命中（`layout::hit_test`）；
/// - 焦点遍历用本方法判定候选，禁用节点不会被 Tab 到（`interaction::focus`）；
/// - a11y 的 `disabled` 属性在同一节点上取同一判据（`a11y::mapping::is_disabled`），
///   祖先禁用由 AT 的父节点 `disabled` 语义继承。
///
/// 与 CSS `pointer-events` 的差别（有意）：**没有**子节点重新启用（`pointer-events:auto`）
/// 的逃生口 —— 内核要把「祖先禁用 ⇒ 子树不命中」做成确定性规则，而不是两套可组合取值。
pub(crate) fn is_effectively_interactive(tree: &NodeTree, node: NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        // 唯一本地判据：`is_hittable`（命中 / 焦点 / a11y 三处共用，R04-9 的一致化）。
        if !is_hittable(tree, current) {
            return false;
        }
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
    true
}

// 消费者：`impl NodeTree::note_hit_test_skip` 的薄委托（外部调用点见该方法文档）。
/// 记一次「命中因不可逆变换 / 非有限几何而跳过」（C11/C13 诊断计数）。
pub(crate) fn note_hit_test_skip(tree: &NodeTree) {
    tree.hit_test_skips
        .set(tree.hit_test_skips.get().saturating_add(1));
}

// 消费者：`impl NodeTree::hit_test_skips` 的薄委托（外部调用点见该方法文档）。
/// 累计的命中跳过次数（单调递增；诊断 / 测试断言用）。
pub(crate) fn hit_test_skips(tree: &NodeTree) -> u64 {
    tree.hit_test_skips.get()
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
        .is_some_and(|target| target.style_patch().interactive == Some(true))
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
            *target.style = style;
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
/// 文本脏队列：`Vec`（保序、保容量）+ `ahash::HashSet`（O(1) 去重；R12-13）。
///
/// `take_into` 一次性取出全部条目并清空去重集合（容量保留）—— 同一节点在同一帧的同一阶段
/// 只入队一次，消费之后可以再次入队（例如第一阶段 shape 完 → 第二阶段重排）。
///
/// 为什么不是「按节点位的位图」：`mark_text_dirty` 可能在该节点还没有 `TextSlot` 时被调用
/// （第一次 `set_text_content`），位图需要改 `Node` 布局（`Node` 有 1128 B 的硬预算）。
/// 集合的峰值大小 = 单帧脏文本数，行情表整表 tick 时也就几千个 `NodeId`（8 B/条）。
pub(crate) struct TextDirtyQueue {
    nodes: Vec<NodeId>,
    seen: ahash::HashSet<NodeId>,
}

impl TextDirtyQueue {
    /// 空队列。
    pub(crate) fn new() -> Self {
        Self {
            nodes: Vec::new(),
            seen: ahash::HashSet::default(),
        }
    }

    /// 入队（去重）。
    pub(crate) fn push(&mut self, node: NodeId) {
        if self.seen.insert(node) {
            self.nodes.push(node);
        }
    }

    /// 取走全部条目（`out` 与队列自身的容量都保留）。
    pub(crate) fn take_into(&mut self, out: &mut Vec<NodeId>) {
        self.seen.clear();
        out.clear();
        out.append(&mut self.nodes);
    }

    /// 待处理条目数。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for TextDirtyQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// 一个文本节点的快照（P05）：`shaped` 供 measure/选区，`line` 供 P06 的 paint。
///
/// `key`/`scale_epoch`/`laid_*` 是「本帧是否需要重做」的判据（B4）：三者全等 ⇒ 整节点跳过。
///
/// **不再持有第二份 parley `Layout`**（R12-6）：选区/光标几何由 `ShapedText` 在同一份
/// `Layout` 上就地重排（零 clone），每个文本节点的常驻成本只剩 `Rc<ShapedLine>`。
pub(crate) struct TextSlot {
    shaped: Rc<crate::text::shape::ShapedText>,
    key: crate::text::cache::ShapeKey,
    scale_epoch: u32,
    line: Option<Rc<ShapedLine>>,
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
}

#[cfg(test)]
mod tests {
    use super::TextDirtyQueue;
    use crate::{
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        text::{TextContent, TextStyle},
    };

    fn text_tree(count: usize) -> (NodeTree, Vec<NodeId>) {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut nodes = Vec::with_capacity(count);
        for index in 0..count {
            let node = NodeBuilder::text(format!("t{index}"), TextStyle::new(12.0))
                .build(&mut tree, root)
                .expect("text node");
            nodes.push(node);
        }
        (tree, nodes)
    }

    /// R12-13：`mark_text_dirty` 去重是 **O(1)**（同一节点重复标脏只入队一次）。
    ///
    /// 旧实现是 `Vec::contains`（O(n)）：行情表整表 tick 时一帧 5000 次插入 ⇒ 约 1250 万次比较。
    /// 这里用「大规模重复标记 + 长度断言」把语义钉死：正确性由集合去重保证，成本由 `HashSet`
    /// 保证（1000 次标记 10 个节点 ⇒ 队列恒为 10）。
    #[test]
    fn text_dirty_queue_dedupes_in_constant_time() {
        let (tree, nodes) = text_tree(10);
        let mut queue = TextDirtyQueue::new();
        for _ in 0..100 {
            for node in &nodes {
                queue.push(*node);
            }
        }
        assert_eq!(queue.len(), nodes.len(), "去重后恒为节点数");
        let mut out = Vec::new();
        queue.take_into(&mut out);
        assert_eq!(out.len(), nodes.len());
        assert_eq!(queue.len(), 0, "take 之后队列为空");
        // 消费之后同一节点可以再次入队（第二阶段 / 下一帧）。
        queue.push(nodes[0]);
        assert_eq!(queue.len(), 1);
        let _ = tree;
    }

    /// R12-14：`set_text_content` 只进 shape 阶段；`ShapedLine` 的重排由 `refresh_text`
    /// （`set_text_shaped`）与内容宽变化（`set_local_layout`）入队。
    #[test]
    fn text_content_and_layout_queues_are_separate() {
        let (mut tree, nodes) = text_tree(2);
        // 构造时静态文本已入 shape 队列；layout 队列要等 shape 出快照才入队。
        assert_eq!(tree.text_dirty_len(), 2);
        assert_eq!(tree.text_layout_dirty_len(), 0);

        let node = nodes[0];
        let content = TextContent::new("changed", TextStyle::new(12.0));
        assert!(tree.set_text_content(node, content));
        assert_eq!(
            tree.text_dirty_len(),
            2,
            "同一节点重复标脏被 O(1) 去重（R12-13）"
        );

        let mut shaped = Vec::new();
        tree.take_text_dirty(&mut shaped);
        assert_eq!(tree.text_dirty_len(), 0);
        let mut layout = Vec::new();
        tree.take_text_layout_dirty(&mut layout);
        assert!(layout.is_empty(), "还没有快照 ⇒ 没有待重排的节点");
        assert_eq!(tree.invalidate_text_lines(), 0, "还没有 line 可失效");
    }
}
