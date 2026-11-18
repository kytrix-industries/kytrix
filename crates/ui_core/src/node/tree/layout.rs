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
    *target.style = style;
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
///
/// **按需分配**：只有真正参与过布局的节点才有条目（`taffy::Cache` 约 0.5 KB，按 arena 槽位
/// 预分配会让每个节点白付这个体积 —— 20 万节点 ≈ 100 MB，R03-3）。
pub(crate) fn taffy_cache(tree: &mut NodeTree, node: NodeId) -> &mut taffy::Cache {
    tree.layout_cache.entry(node).or_default()
}

// 消费者：`impl NodeTree::layout_cache` 的薄委托（仅测试）。
/// 节点的 taffy 缓存（`None` = 从未参与布局 / 已失效并释放）。
#[cfg(test)]
pub(crate) fn layout_cache(tree: &NodeTree, node: NodeId) -> Option<&taffy::Cache> {
    tree.layout_cache.get(&node)
}

// 消费者：`impl NodeTree::set_local_layout` 的薄委托（外部调用点见该方法文档）。
/// `LayoutPartialTree::set_unrounded_layout` 的落点：写**父相对**几何 + 消费 `LAYOUT`。
///
/// **`LAYOUT` 的唯一清除点**（脏位表）。taffy 每轮布局对"缓存未命中"的节点写回一次，
/// 因此"本轮真的被重算过"与"`LAYOUT` 位被这次布局消费"是同一件事。布局之后（
/// `rewindow_virtuals` / `clamp_scroll_offsets`）新产生的 `LAYOUT` 不会被吞掉：帧尾
/// `settle_frame` 不再清它。
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
    let mut width_changed = false;
    if let Some(target) = tree.arena.get_mut(node) {
        width_changed = target.kind.text().is_some()
            && target.text_content_width.to_bits() != content_width.to_bits();
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
    tree.dirty_layout.remove(node);
    // R12-14：容器宽度变化 ⇒ 该文本节点的断行依据变了，必须进第二阶段脏集合。
    // 这里**不动** `LAYOUT`/`PAINT` 位（由 `layout_text` 的重排自己标），只登记「要重排」。
    if width_changed {
        tree.mark_text_layout_dirty(node);
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

// 消费者：`impl NodeTree::set_device_scale` 的薄委托（外部调用点见该方法文档）。
/// 设置布局提交前的设备像素吸附比例（`None`/非法值 = 关闭）。
///
/// 变化会标根 `LAYOUT`（下一轮布局把**全部**边界按新比例重贴一次）。DPI 变化路径已经
/// 整窗失效，这里只是保证独立调用也正确。
pub(crate) fn set_device_scale(tree: &mut NodeTree, scale: Option<f32>) {
    let scale = scale.filter(|scale| scale.is_finite() && *scale > 0.0);
    if tree.device_scale == scale {
        return;
    }
    tree.device_scale = scale;
    // 吸附开关/比例变了：未圆整原点旁表与已提交的吸附盒都要重建（标根 LAYOUT 走全量）。
    tree.raw_origins.clear();
    let root = tree.root;
    tree.invalidate_layout(root);
}

// 消费者：`impl NodeTree::absolutize` 的薄委托（外部调用点见该方法文档）。
/// 绝对坐标 post-pass：把 `local`（父相对）换算成 `layout.bounds`（窗口绝对），
/// 同时**就地**维护每节点的子树包围盒（不再全树重算，B9）。
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
    // B9：出流盒由**包含块**放置，可能晚于它的树父被访问 ⇒ 树祖先的包络需要按"只增"修正
    // （见 `expand_hoisted_ancestors` 的复杂度与正确性说明）。普通子树在 `place_subtree`
    // 的后序里已经精确重算，这里只处理跨界的那批。
    expand_hoisted_ancestors(tree);
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
    // B7：**提交给绘制的边界**做设备像素对齐（只吸附边界，见 `Rect::snap_to_device`）。
    //
    // 递归引用系 `reference` 用的是**未圆整**的绝对坐标：吸附只发生在写 `layout.bounds` 的
    // 这一处，所以 N 层嵌套不会把每层的舍入误差累加（每层边界都是"逻辑边界 × scale"的
    // 一次舍入，而不是"父的已舍入值 + 子偏移"的链式舍入）。
    let committed = match tree.device_scale {
        Some(scale) => abs.snap_to_device(scale),
        None => abs,
    };
    let old = target.layout;
    let children = target.children.clone();
    let changed = committed != old.bounds;
    let fresh = target.laid_out_epoch == epoch;
    // 吸附开启时：父盒"吸附后没变"不蕴含子树没变（未圆整位移可能让后代跨过半像素边界），
    // 因此早期退出判定还要看**未圆整原点**是否变化（该旁表只在 `device_scale.is_some()` 时维护）。
    let raw_changed = tree.device_scale.is_some() && raw_origin(tree, node) != Some(abs.origin);
    if !force && !fresh && !changed && !raw_changed {
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
            // damage 的旧一侧在这里精确并入（`prev_layout` 字段因此可以删掉，省 32 B/节点）。
            target.damage = target.damage.union(old.bounds).union(committed);
            target.layout = LayoutBox {
                bounds: committed,
                border: target.local_border,
            };
            // P06 F-60：lane 内的图元持有**绝对** bounds；布局移动后即使没有 `mark_paint`，
            // 旧 lane 也已过期（新位置的像素没人写）。
            target.dirty.remove(DirtyFlags::REUSABLE);
        }
        // 反向集合：a11y 要重发（`mark_moved_a11y` 的唯一输入）+ 帧尾要结算 damage/REUSABLE。
        tree.moved_nodes.insert(node);
        tree.dirty_settle.insert(node);
    } else if fresh {
        // 尺寸/位置没变但 border 宽可能变了（样式改 border）。
        if let Some(target) = tree.arena.get_mut(node) {
            target.layout.border = target.local_border;
        }
    }

    // 出流子节点由**包含块**的 hoisted 列表驱动（B2）；没有任何出流盒时省掉每节点的表查找。
    let hoisted: SmallVec<[NodeId; 2]> = if tree.hoisted.is_empty() {
        SmallVec::new()
    } else {
        tree.hoisted.get(&node).cloned().unwrap_or_default()
    };
    for child in children.iter().copied() {
        // 出流节点（无 override）由它的**包含块**的 hoisted 列表驱动；但 override 节点按
        // **树父**的 border-box origin 放置（B1：taffy 会把 absolute 节点挂到最近的定位祖先，
        // 而引擎 `LayoutOverride` 的契约是「相对父 border box」），隐藏子树里的出流后代也必须
        // 在这里零化（N1）。
        if tree.is_out_of_flow(child) && tree.layout_override_of(child).is_none() && !hidden {
            continue;
        }
        tree.place_subtree(child, abs.origin, epoch, false, hidden);
    }
    for child in hoisted.iter().copied() {
        if tree.layout_override_of(child).is_some() {
            // override 节点已由树父按 border-box origin 放置（B1），不得再用包含块的 local 算一次。
            continue;
        }
        tree.place_subtree(child, abs.origin, epoch, false, false);
    }

    // B9：本节点的子树包围盒在**同一次后序遍历**里精确重算（不再有第二遍全树扫描）。
    //
    // 子节点在下面这个循环之前已经处理完（DFS 后序），因此读到的 `subtree_bounds` 是本轮的；
    // 未被访问的子节点缓存仍然有效（跳过条件是「没布局且绝对盒没变」⇒ 其子树包络不变）。
    // 成本 = O(本节点直接子节点数)，与"命中测试要剪枝的那些子树"同阶，**不是** O(全树)。
    let mut union = committed;
    for child in children.iter().copied() {
        union = union.union(subtree_bounds(tree, child));
    }
    for child in &hoisted {
        union = union.union(subtree_bounds(tree, *child));
    }
    store_subtree_bounds(tree, node, union);
    store_raw_origin(tree, node, abs.origin);
}

/// 上次布局的未圆整绝对原点（`None` = 从未记录；吸附关闭时恒为 `None`）。
fn raw_origin(tree: &NodeTree, node: NodeId) -> Option<Point> {
    tree.raw_origins.get(node.index() as usize).copied()
}

/// 记录未圆整绝对原点（只在吸附开启时分配/写入，整数 DPI 下零成本）。
fn store_raw_origin(tree: &mut NodeTree, node: NodeId, origin: Point) {
    if tree.device_scale.is_none() {
        return;
    }
    let index = node.index() as usize;
    if tree.raw_origins.len() <= index {
        tree.raw_origins.resize(index + 1, Point::ZERO);
    }
    tree.raw_origins[index] = origin;
}

/// 写一个节点的子树包围盒（按 arena 槽位扩容；`layout_stats.bounds_updated` 计数）。
fn store_subtree_bounds(tree: &mut NodeTree, node: NodeId, bounds: Rect) {
    let index = node.index() as usize;
    if tree.subtree_bounds.len() <= index {
        tree.subtree_bounds.resize_with(index + 1, Rect::default);
    }
    tree.subtree_bounds[index] = bounds;
    tree.layout_stats.bounds_updated += 1;
}

/// 出流盒的**树祖先包络修正**（B9 的跨界情形）。
///
/// 普通子节点在 `place_subtree` 的后序里先于父被处理，父的包络因此是精确的。出流盒是例外：
/// 它由**包含块**（可能不是树父）放置，而包含块可能在它的树父**之后**才被访问 —— 于是那一段
/// 树祖先的包络可能低估（漏掉刚移动的出流后代）。
///
/// 修正方式：对「本轮移动过的出流盒」沿**树父链**收集祖先，去重后**按深度自底向上精确重算一次**
/// 包络（`自身 ∪ 直接子节点的包络`）。子节点的包络此时已全部是本轮的（DFS 后序 + 本次修正的
/// 自底向上），因此结果是精确值而不是"只增近似"。
///
/// **复杂度（关键，wave-9 性能回归的修复点）**：
/// - 祖先集合去重 ⇒ 每个被波及的祖先只重算一次，成本 `O(Σ 被修正祖先的直接子节点数)`；
/// - 收集阶段 `O(移动出流盒数 × 祖先深度)`，与子树规模无关；
/// - **`LayoutOverride` 节点不参与**：它们由树父在本轮 DFS 里就地放置（B1 的"相对父 border box"
///   契约），父的包络必然在它们之后计算，天然精确。把它们当作"跨界出流"处理会让
///   「500 个 override 兄弟」退化成 `O(兄弟数 × 兄弟数)` 的二次开销（实测 1.1 ms → 0）。
fn expand_hoisted_ancestors(tree: &mut NodeTree) {
    if tree.moved_nodes.is_empty() {
        return;
    }
    // 先快照（不能同时持有集合借用与 `&mut tree`）；复用跨帧缓冲。
    let mut moved = std::mem::take(&mut tree.dirty_scratch);
    tree.moved_nodes.snapshot_into(&mut moved);

    // 1) 收集受影响的树祖先：去重 + **真深度**（根 = 0），随后深者先算。
    //
    // 深度必须是「到根的层数」而不是「到移动节点走了几步」：不同移动节点的链会把同一个
    // 祖先登记成不同的相对深度，用它排序会让深祖先排在浅祖先之后（wave-9 首版实现的缺陷）。
    let mut ancestors: Vec<(u32, NodeId)> = Vec::new();
    let mut seen: ahash::HashSet<NodeId> = ahash::HashSet::default();
    for &moved_node in &moved {
        // override 节点由树父就地放置 ⇒ 包络已精确，不需要跨界修正。
        if !tree.is_out_of_flow(moved_node) || tree.layout_override_of(moved_node).is_some() {
            continue;
        }
        // 移动节点到根的层数（一次 O(depth) 行走），祖先的真深度由它沿链递减得到。
        let mut depth = 0;
        let mut cursor = Some(moved_node);
        while let Some(current) = cursor {
            cursor = tree.arena.get(current).and_then(|target| target.parent);
            if cursor.is_some() {
                depth += 1;
            }
        }
        let mut cursor = tree.arena.get(moved_node).and_then(|target| target.parent);
        while let Some(current) = cursor {
            depth -= 1;
            if !seen.insert(current) {
                // 第一次插入时已经把它**上面**的所有祖先一并登记过，可以停。
                break;
            }
            ancestors.push((depth, current));
            cursor = tree.arena.get(current).and_then(|target| target.parent);
        }
    }

    // 2) 自底向上精确重算（父读到的子包络是本轮的）。
    ancestors.sort_unstable_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    for &(_, current) in &ancestors {
        let Some(target) = tree.arena.get(current) else {
            continue;
        };
        let mut union = target.layout.bounds;
        let children = target.children.clone();
        for child in children {
            union = union.union(subtree_bounds(tree, child));
        }
        store_subtree_bounds(tree, current, union);
    }
    tree.recycle(moved);
}

/// 结构变更（卸载子树）后的**局部收紧**：沿树父链精确重算包络。
///
/// `place_subtree` 只在被访问的节点上重算包络；卸载整棵子树时被删节点不再被访问，父的包络
/// 会保留旧（偏大）值。偏大只影响剪枝效率、不影响正确性，但长期回收行会让包络越滚越大，
/// 因此结构变更走这条精确路径：
///
/// - 成本 = `O(Σ 被删祖先的直接子节点数)`，与节点总数无关；不变则提前停止；
/// - 只在**卸载**（冷路径）调用，布局热路径不付这笔钱。
pub(crate) fn recompute_bounds_upwards(tree: &mut NodeTree, from: NodeId) {
    let mut cursor = Some(from);
    while let Some(current) = cursor {
        let Some(target) = tree.arena.get(current) else {
            return;
        };
        let mut union = target.layout.bounds;
        let children = target.children.clone();
        for child in children {
            union = union.union(subtree_bounds(tree, child));
        }
        let cached = subtree_bounds(tree, current);
        if union == cached {
            return;
        }
        store_subtree_bounds(tree, current, union);
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
}

// 消费者：`impl NodeTree::recompute_subtree_bounds` 的薄委托（外部调用点见该方法文档）。/// 全树重算子树包围盒（**诊断/测试逃生口**：正常布局路径由 `place_subtree` 增量维护）。
///
/// 生产路径不再调用它（R04-1：每轮全树 post-pass 是 O(N)）；保留为
/// `#[cfg(test)]` 不可见的一致性校验入口（`subtree_bounds` 的增量维护必须与它逐位一致）。
#[cfg(test)]
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
        if let Some(cache) = tree.layout_cache.get_mut(&current) {
            cache.clear();
        }
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
}

// 消费者：`impl NodeTree::settle_frame` 的薄委托（外部调用点见该方法文档）。
/// 帧结算（**唯一**的 damage / `REUSABLE` 复位点，P06 §9.4 / F-59）。
///
/// 只遍历**本帧动过的节点**（`dirty_settle`）：damage 归零、置 `REUSABLE`。
/// 旧实现遍历整个全序数组（每帧 O(N)），是 R03-1 的 #12。
///
/// **不**在这里清 `LAYOUT` / `PAINT`（R03-9）：`LAYOUT` 的唯一清除点是 `set_local_layout`，
/// `PAINT` 的唯一清除点是 paint pass。帧尾无脑清 LAYOUT 会吞掉「布局之后新产生的布局请求」
/// （`rewindow_virtuals` / `clamp_scroll_offsets`），正是 R03-9 现象 B。
///
/// **只在 `Window::commit_frame` 里、paint 已消费 lane 与 damage 之后调用**。
pub(crate) fn settle_frame(tree: &mut NodeTree) {
    let ids = tree.take_settle();
    for id in &ids {
        if let Some(target) = tree.arena.get_mut(*id) {
            target.damage = Rect::default();
            target.dirty.insert(DirtyFlags::REUSABLE);
        }
    }
    tree.recycle(ids);
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
pub(crate) fn set_layout_for_test(tree: &mut NodeTree, node: NodeId, layout: LayoutBox) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.layout = layout;
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
    /// 本轮写过子树包围盒的节点数（B9 增量性证据；`store_subtree_bounds` 计数）。
    bounds_updated: usize,
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
            bounds_updated: self.bounds_updated,
        }
    }
}
