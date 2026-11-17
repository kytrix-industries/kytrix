//! 布局：taffy 缓存旁表、绝对坐标 post-pass、布局失效与帧结算。

use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{LayoutBox, NodeKind, NodeTree};
use crate::{
    error::EngineError,
    layout::{
        Axis, LayoutOverride, LayoutReport, MeasureFn,
        hit_test::{HitResult, hit_test as hit_test_layout},
    },
    math::{Edges, Point, Rect},
    node::{
        dirty::{DirtyFlags, affects_parent, depends_on_children},
        id::NodeId,
    },
    paint::style::Style,
};

// 消费者：`impl NodeTree::compute_layout` 的薄委托（外部调用点见该方法文档）。
/// 一次布局运行：`compute_root_layout` → 绝对坐标 post-pass → 错误面。
///
/// 消费者：`Window::compute_layout`（唯一调用点）。
///
/// # Errors
/// `EngineError::InvalidMeasure { node, axis, value }`：measure 闭包返回非有限值或负尺寸（首条优先）。
pub(crate) fn compute_layout(
    tree: &mut NodeTree,
    available: taffy::Size<Option<f32>>,
) -> Result<LayoutReport, EngineError> {
    crate::layout::taffy_tree::run(tree, available)
}

// 消费者：`impl NodeTree::invalidate_layout` 的薄委托（外部调用点见该方法文档）。
/// 标 `LAYOUT|PAINT|A11Y` + 冒泡 + 清 `node..root` 的 taffy 缓存（所有「布局输入变了」的公共尾段）。
///
/// `A11Y` 是 P11 追加的：布局输入变了 ⇒ 布局盒（进而 a11y bounds）可能变，必须重发。
///
/// 消费者：`set_style` / `invalidate_measure` / `set_layout_override` / `set_virtual_row` /
/// `set_absolute_inset` / `Window::{set_size, set_scale}`。
pub(crate) fn invalidate_layout(tree: &mut NodeTree, node: NodeId) -> bool {
    if !tree.touch(
        node,
        DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
    ) {
        return false;
    }
    tree.bubble_layout(node);
    tree.invalidate_caches_upwards(node);
    true
}

// 消费者：`impl NodeTree::invalidate_measure` 的薄委托（外部调用点见该方法文档）。
/// 「我的 measure 结果变了，但样式没变」：唯一的 measure 失效入口（P05 的字体栈/文本内容变化）。
///
/// 不做独立 measure 缓存：taffy `Cache` 的 key 已含 `(known_dimensions, available_space,
/// sizing_mode, run_mode, parent_size)`，唯一缺的是「闭包捕获的状态变了」这个信号 —— 就是本方法。
///
/// # Errors
/// `EngineError::StaleNode { node }`：句柄陈旧。
pub(crate) fn invalidate_measure(tree: &mut NodeTree, node: NodeId) -> Result<(), EngineError> {
    if tree.arena.get(node).is_none() {
        return Err(EngineError::StaleNode { node });
    }
    tree.invalidate_layout(node);
    Ok(())
}

// 消费者：`impl NodeTree::set_style` 的薄委托（外部调用点见该方法文档）。
// ── P04：布局与命中的公开入口 ──────────────────────────────────────────────

/// 结构性/静态样式变更的唯一公开入口：写值 + 标 `LAYOUT|PAINT` + 冒泡 + 清 `node..root` 的 taffy 缓存。
///
/// **动态属性请用 `bind_style`**（`StylePatch` 通道）：每帧调本方法会清掉整条祖先链的缓存，
/// 让增量布局退化为全量。
///
/// # Errors
/// `EngineError::StaleNode { node }`：句柄陈旧（从未存在 / 已卸载）。
pub(crate) fn set_style(
    tree: &mut NodeTree,
    node: NodeId,
    style: Style,
) -> Result<(), EngineError> {
    let Some(target) = tree.arena.get_mut(node) else {
        return Err(EngineError::StaleNode { node });
    };
    target.style = style;
    tree.invalidate_layout(node);
    Ok(())
}

// 消费者：`impl NodeTree::set_layout_override` 的薄委托（外部调用点见该方法文档）。
/// 更新布局覆盖矩形（P10 的拖拽/分栏手柄）：写值 + 标 `LAYOUT|PAINT` + 清 `node..root` 缓存。
///
/// # Errors
/// `EngineError::StaleNode { node }`：句柄陈旧。
pub(crate) fn set_layout_override(
    tree: &mut NodeTree,
    node: NodeId,
    layout_override: Option<LayoutOverride>,
) -> Result<(), EngineError> {
    let Some(target) = tree.arena.get_mut(node) else {
        return Err(EngineError::StaleNode { node });
    };
    target.layout_override = layout_override;
    tree.invalidate_layout(node);
    Ok(())
}

// 消费者：`impl NodeTree::set_virtual_row` 的薄委托（外部调用点见该方法文档）。
/// 写一行虚拟列表的绝对定位（`left: 0 / right: 0 / top: i*extent / height: extent`）。
///
/// 只碰 taffy 样式里「行位置」的四个字段（应用的行子树不受影响），随后走 `invalidate_layout`
/// （`LAYOUT|PAINT` + 冒泡 + 清缓存）。
pub(crate) fn set_virtual_row(tree: &mut NodeTree, node: NodeId, top: f32, height: f32) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    target.style.inner.position = taffy::style::Position::Absolute;
    target.style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(0.0),
        right: taffy::style::LengthPercentageAuto::length(0.0),
        top: taffy::style::LengthPercentageAuto::length(top),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    target.style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::auto(),
        height: taffy::style::Dimension::length(height),
    };
    tree.invalidate_layout(node)
}

// 消费者：`impl NodeTree::set_absolute_inset` 的薄委托（外部调用点见该方法文档）。
/// 写绝对定位节点的左上角（浮层定位；只碰 `inset.left/top`）。
pub(crate) fn set_absolute_inset(tree: &mut NodeTree, node: NodeId, origin: Point) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    target.style.inner.position = taffy::style::Position::Absolute;
    target.style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(origin.x),
        right: taffy::style::LengthPercentageAuto::auto(),
        top: taffy::style::LengthPercentageAuto::length(origin.y),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    tree.invalidate_layout(node)
}

// 消费者：`impl NodeTree::taffy_style` 的薄委托（外部调用点见该方法文档）。
// ── P04：taffy 低级 trait 的访问器（消费者：`layout::taffy_tree` / `layout::hit_test`） ──

/// 节点的 taffy 样式；陈旧 id 回落到根的样式（`debug_assert`）。
pub(crate) fn taffy_style(tree: &NodeTree, node: NodeId) -> &taffy::Style<SmolStr> {
    match tree.arena.get(node) {
        Some(target) => &target.style.inner,
        None => {
            debug_assert!(false, "taffy queried a stale node's style");
            &tree.arena.get(tree.root).expect("根节点恒存活").style.inner
        }
    }
}

// 消费者：`impl NodeTree::taffy_children` 的薄委托（外部调用点见该方法文档）。
/// 子节点切片；陈旧 id 回落为空切片。
pub(crate) fn taffy_children(tree: &NodeTree, node: NodeId) -> &[NodeId] {
    tree.arena
        .get(node)
        .map_or(&[], |target| target.children.as_slice())
}

// 消费者：`impl NodeTree::taffy_cache` 的薄委托（外部调用点见该方法文档）。
/// 旁表里的 taffy 缓存（首次访问即建）。
pub(crate) fn taffy_cache(tree: &mut NodeTree, node: NodeId) -> &mut taffy::Cache {
    let index = node.index() as usize;
    if tree.layout_cache.len() <= index {
        tree.layout_cache.resize_with(index + 1, || None);
    }
    tree.layout_cache[index].get_or_insert_with(taffy::Cache::new)
}

// 消费者：`impl NodeTree::set_local_layout` 的薄委托（外部调用点见该方法文档）。
/// `LayoutPartialTree::set_unrounded_layout` 的落点：写**父相对**几何 + 消费 `LAYOUT`。
pub(crate) fn set_local_layout(tree: &mut NodeTree, node: NodeId, layout: &taffy::Layout) {
    let epoch = tree.layout_epoch;
    let local = Rect::new(
        layout.location.x,
        layout.location.y,
        layout.size.width,
        layout.size.height,
    );
    let border = Edges::new(
        layout.border.top,
        layout.border.right,
        layout.border.bottom,
        layout.border.left,
    );
    let content_width = (layout.size.width
        - layout.border.left
        - layout.border.right
        - layout.padding.left
        - layout.padding.right)
        .max(0.0);
    if let Some(target) = tree.arena.get_mut(node) {
        target.local = local;
        target.local_border = border;
        target.text_content_width = content_width;
        target.content_offset = Point::new(
            layout.border.left + layout.padding.left,
            layout.border.top + layout.padding.top,
        );
        target.laid_out_epoch = epoch;
        // `LAYOUT` 位的唯一消费点：被 taffy 布局过 ⇒ 不再需要重排。
        target.dirty.remove(DirtyFlags::LAYOUT);
    }
}

// 消费者：`impl NodeTree::measure_of` 的薄委托（外部调用点见该方法文档）。
/// 节点的自定义测量闭包。
pub(crate) fn measure_of(tree: &NodeTree, node: NodeId) -> Option<&MeasureFn> {
    tree.arena
        .get(node)
        .and_then(|target| target.measure.as_ref())
}

// 消费者：`impl NodeTree::layout_override_of` 的薄委托（外部调用点见该方法文档）。
/// 节点的布局覆盖。
pub(crate) fn layout_override_of(tree: &NodeTree, node: NodeId) -> Option<LayoutOverride> {
    tree.arena
        .get(node)
        .and_then(|target| target.layout_override)
}

// 消费者：`impl NodeTree::is_out_of_flow` 的薄委托（外部调用点见该方法文档）。
/// 节点的布局是否「出流」（与 taffy `RoundTree::is_out_of_flow` 的判定逐字一致）。
pub(crate) fn is_out_of_flow(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena.get(node).is_some_and(|target| {
        target.style.inner.position.is_out_of_flow()
            && !matches!(target.style.inner.display, taffy::style::Display::None)
    })
}

// 消费者：`impl NodeTree::begin_layout` 的薄委托（外部调用点见该方法文档）。
/// 开一轮布局：epoch +1、统计清零。
pub(crate) fn begin_layout(tree: &mut NodeTree) {
    tree.layout_epoch = tree.layout_epoch.wrapping_add(1);
    tree.layout_stats = LayoutStats::default();
}

// 消费者：`impl NodeTree::take_report` 的薄委托（外部调用点见该方法文档）。
/// 取走本轮统计（消费者：`layout::taffy_tree::run`）。
pub(crate) fn take_report(tree: &mut NodeTree) -> LayoutReport {
    std::mem::take(&mut tree.layout_stats).into_report()
}

// 消费者：`impl NodeTree::stats_mut` 的薄委托（外部调用点见该方法文档）。
/// 本轮统计的可写视图（消费者：`impl CacheTree for NodeTree`、`dispatch_child_layout`）。
pub(crate) fn stats_mut(tree: &mut NodeTree) -> &mut LayoutStats {
    &mut tree.layout_stats
}

// 消费者：`impl NodeTree::note_invalid_measure` 的薄委托（外部调用点见该方法文档）。
/// 记录一条 measure 违规（**首条优先**，保证诊断确定）。
pub(crate) fn note_invalid_measure(tree: &mut NodeTree, node: NodeId, axis: Axis, value: f32) {
    if tree.invalid_measure.is_none() {
        tree.invalid_measure = Some(EngineError::InvalidMeasure { node, axis, value });
    }
}

// 消费者：`impl NodeTree::take_invalid_measure` 的薄委托（外部调用点见该方法文档）。
pub(crate) fn take_invalid_measure(tree: &mut NodeTree) -> Option<EngineError> {
    tree.invalid_measure.take()
}

// 消费者：`impl NodeTree::absolutize` 的薄委托（外部调用点见该方法文档）。
/// 绝对坐标 post-pass：把 `local`（父相对）换算成 `layout.bounds`（窗口绝对）。
///
/// 遍历规则（正确性的核心，见 `design/phase-04-layout.md` §7）：
/// - 从根开始，根的 `local` 相对 ICB 原点 ⇒ 绝对 = `local`；
/// - 流内子节点：`abs = 父 abs.origin + child.local.origin`；
/// - 出流子节点：**不在**父的 children 循环里处理（跳过），而由它**包含块**的 hoisted 列表驱动，
///   参照系仍是包含块的 border box origin（taffy `OofPositioningArea.offset` 的定义）；
/// - 跳过整棵子树的条件：本轮没被布局（`laid_out_epoch != epoch`）且 `abs` 与旧值逐位相同。
pub(crate) fn absolutize(tree: &mut NodeTree) {
    let epoch = tree.layout_epoch;
    tree.place_subtree(tree.root, Point::ZERO, epoch, true, false);
    tree.recompute_subtree_bounds();
}

// 消费者：`impl NodeTree::place_subtree` 的薄委托（外部调用点见该方法文档）。
pub(crate) fn place_subtree(
    tree: &mut NodeTree,
    node: NodeId,
    reference: Point,
    epoch: u32,
    force: bool,
    parent_hidden: bool,
) {
    let Some(target) = tree.arena.get(node) else {
        return;
    };
    // 隐藏子树（自身或祖先 `display: none`）一律零化：taffy 的 hidden pass 已把 local 写成零，
    // 这里再兜住 `LayoutOverride`（覆盖节点不得留非零幽灵盒）。
    let hidden = parent_hidden || matches!(target.style.inner.display, taffy::style::Display::None);
    // `LayoutOverride` 直接覆盖 taffy 给的父相对盒（位置与尺寸都取应用给的值）；
    // 隐藏时归零（否则出流/隐藏的覆盖节点会在 P06 读到幽灵盒）。
    let local = if hidden {
        Rect::default()
    } else {
        target
            .layout_override
            .map_or(target.local, |over| over.rect)
    };
    let abs = Rect::from_origin_size(reference + local.origin, local.size);
    let old = target.layout;
    let children = target.children.clone();
    let changed = abs != old.bounds;
    let fresh = target.laid_out_epoch == epoch;
    if !force && !fresh && !changed {
        return;
    }

    if changed {
        tree.layout_stats.moved += 1;
        // 几何内容的可见范围可能超出布局盒：布局移动拿不到 `GeometryDesc`，
        // 保守升级为全帧 damage（复审 A3）。
        if matches!(
            tree.arena.get(node).map(|target| &target.kind),
            Some(NodeKind::Geometry(_))
        ) {
            tree.geometry_damage_escalation = true;
        }
        if let Some(target) = tree.arena.get_mut(node) {
            target.damage = target.damage.union(old.bounds).union(abs);
            target.prev_layout = old;
            target.layout = LayoutBox {
                bounds: abs,
                border: target.local_border,
            };
            // P06 F-60：lane 内的图元持有**绝对** bounds；布局移动后即使没有 `mark_paint`，
            // 旧 lane 也已过期（新位置的像素没人写）。
            target.dirty.remove(DirtyFlags::REUSABLE);
        }
    } else if fresh {
        // 尺寸/位置没变但 border 宽可能变了（样式改 border）。
        if let Some(target) = tree.arena.get_mut(node) {
            target.layout.border = target.local_border;
        }
    }

    let hoisted: SmallVec<[NodeId; 2]> = tree.hoisted.get(&node).cloned().unwrap_or_default();
    for child in children {
        // 出流节点（无 override）由它的**包含块**的 hoisted 列表驱动；但 override 节点按
        // **树父**的 border-box origin 放置（B1：taffy 会把 absolute 节点挂到最近的定位祖先，
        // 而引擎 `LayoutOverride` 的契约是「相对父 border box」），隐藏子树里的出流后代也必须
        // 在这里零化（N1）。
        if tree.is_out_of_flow(child) && tree.layout_override_of(child).is_none() && !hidden {
            continue;
        }
        tree.place_subtree(child, abs.origin, epoch, false, hidden);
    }
    for child in hoisted {
        if tree.layout_override_of(child).is_some() {
            // override 节点已由树父按 border-box origin 放置（B1），不得再用包含块的 local 算一次。
            continue;
        }
        tree.place_subtree(child, abs.origin, epoch, false, false);
    }
}

// 消费者：`impl NodeTree::recompute_subtree_bounds` 的薄委托（外部调用点见该方法文档）。
/// 自底向上重算每节点的子树包围盒（绝对坐标并集）。
///
/// 迭代式后序遍历（显式栈）：树的深度由应用决定，递归可能爆栈。
pub(crate) fn recompute_subtree_bounds(tree: &mut NodeTree) {
    let root = tree.root;
    let mut stack: Vec<(NodeId, bool)> = vec![(root, false)];
    while let Some((node, visited)) = stack.pop() {
        let Some(target) = tree.arena.get(node) else {
            continue;
        };
        if !visited {
            stack.push((node, true));
            for child in target.children().iter().rev() {
                stack.push((*child, false));
            }
            continue;
        }
        let mut union = tree
            .arena
            .get(node)
            .map_or(Rect::default(), |target| target.layout.bounds);
        let children = tree.arena.get(node).map(|target| target.children());
        for child in children.unwrap_or_default() {
            let index = child.index() as usize;
            if let Some(bounds) = tree.subtree_bounds.get(index) {
                union = union.union(*bounds);
            }
        }
        let index = node.index() as usize;
        if tree.subtree_bounds.len() <= index {
            tree.subtree_bounds.resize_with(index + 1, Rect::default);
        }
        tree.subtree_bounds[index] = union;
    }
}

// 消费者：`impl NodeTree::subtree_bounds` 的薄委托（外部调用点见该方法文档）。
/// 子树包围盒（绝对坐标；自身 + 后代布局盒的并集）。未布局时返回 `Rect::default()`。
///
/// 命中测试用它做**唯一**安全的子树剪枝（P2-2）：节点自己的布局盒不能代表子树，
/// 出流 / 溢出后代可以落在父盒之外。
pub(crate) fn subtree_bounds(tree: &NodeTree, node: NodeId) -> Rect {
    tree.subtree_bounds
        .get(node.index() as usize)
        .copied()
        .unwrap_or_default()
}

// 消费者：`impl NodeTree::invalidate_caches_upwards` 的薄委托（外部调用点见该方法文档）。
/// 清 `node` 与**全部祖先**的 taffy 缓存（**不能早停**：任一祖先 cache 命中都会让 taffy 不再下钻）。
pub(crate) fn invalidate_caches_upwards(tree: &mut NodeTree, node: NodeId) {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        let index = current.index() as usize;
        if let Some(cache) = tree.layout_cache.get_mut(index).and_then(Option::as_mut) {
            cache.clear();
        }
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
}

// 消费者：`impl NodeTree::settle_frame` 的薄委托（外部调用点见该方法文档）。
/// 帧结算（**唯一**的复位点，P06 §9.4 / F-59）：damage 归零、`prev_layout = layout`、
/// 置 `REUSABLE`、清本帧已消费的 `LAYOUT | PAINT`。
///
/// **只在 `Window::commit_frame` 里、paint 已消费 lane 与 damage 之后调用**：drain 阶段的
/// `touch` 发生在帧前，帧初归零会吞掉已累积的 damage。帧内不得 `mark_*`（`&mut self` 已把
/// 写操作挡在帧外）。
///
/// `LAYOUT` 只对本轮**真正参与过布局**的节点清除（`laid_out_epoch == layout_epoch`）。
/// 这一条是 P1-2.2 的修复：虚拟列表在布局**之后**补挂可见行，那些行本帧从未进 taffy；
/// 无条件清 `LAYOUT` 会让它们的脏位凭空消失，于是永久停在 0×0（帧尾不再重排、下一帧
/// `needs_frame` 也看不到脏位）。保留脏位让下一帧的 `layout_if_dirty` 把它们排上。
pub(crate) fn settle_frame(tree: &mut NodeTree) {
    let epoch = tree.layout_epoch;
    for id in &tree.order {
        if let Some(target) = tree.arena.get_mut(*id) {
            target.damage = Rect::default();
            target.prev_layout = target.layout;
            target.dirty.insert(DirtyFlags::REUSABLE);
            // 本轮布局过的节点：`LAYOUT` 已由 `set_layout` 消费（那里就 remove 了），
            // 这里只需清 `PAINT`。没布局过的节点保留 `LAYOUT`，等下一帧重排。
            target.dirty.remove(DirtyFlags::PAINT);
            if target.laid_out_epoch != epoch {
                continue;
            }
            target.dirty.remove(DirtyFlags::LAYOUT);
        }
    }
}

// 消费者：`impl NodeTree::bubble_layout` 的薄委托（外部调用点见该方法文档）。
/// `LAYOUT` 的向上冒泡（§9.2 的逐字伪代码）。
pub(crate) fn bubble_layout(tree: &mut NodeTree, from: NodeId) {
    let mut child = from;
    while let Some((parent, affects)) = tree
        .arena
        .get(child)
        .map(|target| (target.parent, affects_parent(target)))
    {
        if !affects {
            break;
        }
        let Some(parent) = parent else {
            break;
        };
        let already = tree
            .arena
            .get(parent)
            .is_some_and(|target| target.dirty.contains(DirtyFlags::LAYOUT));
        if already && tree.arena.get(parent).is_some_and(depends_on_children) {
            break;
        }
        if !tree.touch_layout_only(parent) {
            break;
        }
        if !tree.arena.get(parent).is_some_and(depends_on_children) {
            break;
        }
        child = parent;
    }
}

// 消费者：`impl NodeTree::hit_test` 的薄委托（外部调用点见该方法文档）。
/// 命中测试（逆绘制序 + clip 链 + `hit_shape`）。
///
/// 消费者：P10 的事件分发、P10 的 `Overlay`、P11 的 a11y hit test（P04 的用例也用它）。
pub(crate) fn hit_test(tree: &NodeTree, point: Point) -> Option<HitResult> {
    hit_test_layout(tree, point)
}

// 消费者：`impl NodeTree::set_layout_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：直接写布局盒（P04 的 `set_layout` 之前的唯一写点，damage 用例需要）。
#[cfg(test)]
pub(crate) fn set_layout_for_test(
    tree: &mut NodeTree,
    node: NodeId,
    layout: LayoutBox,
    prev_layout: LayoutBox,
) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.layout = layout;
            target.prev_layout = prev_layout;
            true
        }
        None => false,
    }
}

// 消费者：`layout::taffy_tree`（经 `NodeTree::stats_mut` / `take_report`）。
/// 一次布局运行的计数器。
///
/// 字段私有 + `note_*`/`into_report` 方法：`field_scoped_visibility_modifiers` 禁止字段带 scoped 修饰，
/// 而可见性必须能被 `layout::taffy_tree` 使用，因此只能经方法暴露。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LayoutStats {
    cache_hits: usize,
    cache_misses: usize,
    laid_out: usize,
    moved: usize,
}

impl LayoutStats {
    pub(crate) fn note_cache_hit(&mut self) {
        self.cache_hits += 1;
    }

    pub(crate) fn note_cache_miss(&mut self) {
        self.cache_misses += 1;
    }

    /// 只有「完整布局且 taffy cache 未命中」的节点才计入 `laid_out`（= 真正被重算的节点）。
    pub(crate) fn note_laid_out(&mut self) {
        self.laid_out += 1;
    }

    pub(crate) fn into_report(self) -> LayoutReport {
        LayoutReport {
            laid_out: self.laid_out,
            moved: self.moved,
            cache_hits: self.cache_hits,
            cache_misses: self.cache_misses,
        }
    }
}
