use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{LayoutBox, NodeKind, NodeTree};
use crate::{
    error::EngineError,
    layout::{
        Axis, LayoutOverride, LayoutReport, MeasureFn,
        hit_test::{HitResult, hit_test as hit_test_layout},
    },
    math::{LogicalEdges, LogicalPoint, LogicalPx, LogicalRect, px},
    node::{
        dirty::{DirtyFlags, affects_parent, depends_on_children},
        id::NodeId,
    },
    paint::style::Style,
};

pub(crate) fn compute_layout(
    tree: &mut NodeTree,
    available: taffy::Size<Option<f32>>,
) -> Result<LayoutReport, EngineError> {
    crate::layout::taffy_tree::run(tree, available)
}

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

pub(crate) fn invalidate_measure(tree: &mut NodeTree, node: NodeId) -> Result<(), EngineError> {
    if tree.arena.get(node).is_none() {
        return Err(EngineError::StaleNode { node });
    }
    tree.invalidate_layout(node);
    Ok(())
}

pub(crate) fn set_style(
    tree: &mut NodeTree,
    node: NodeId,
    style: Style,
) -> Result<(), EngineError> {
    let Some(target) = tree.arena.get_mut(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if *target.style == style {
        return Ok(());
    }
    *target.style = style;
    tree.invalidate_layout(node);
    Ok(())
}

pub(crate) fn set_layout_override(
    tree: &mut NodeTree,
    node: NodeId,
    layout_override: Option<LayoutOverride>,
) -> Result<(), EngineError> {
    let Some(target) = tree.arena.get_mut(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.layout_override == layout_override {
        return Ok(());
    }
    target.layout_override = layout_override;
    tree.invalidate_layout(node);
    Ok(())
}

pub(crate) fn set_virtual_row(
    tree: &mut NodeTree,
    node: NodeId,
    top: LogicalPx,
    height: LogicalPx,
) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    target
        .style
        .position(taffy::style::Position::Absolute)
        .inset(
            taffy::style::LengthPercentageAuto::length(top.value()),
            taffy::style::LengthPercentageAuto::length(0.0),
            taffy::style::LengthPercentageAuto::auto(),
            taffy::style::LengthPercentageAuto::length(0.0),
        )
        .width(taffy::style::Dimension::auto())
        .height(taffy::style::Dimension::length(height.value()));
    tree.invalidate_layout(node)
}

pub(crate) fn set_absolute_inset(tree: &mut NodeTree, node: NodeId, origin: LogicalPoint) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    target
        .style
        .position(taffy::style::Position::Absolute)
        .inset(
            taffy::style::LengthPercentageAuto::length(origin.y.value()),
            taffy::style::LengthPercentageAuto::auto(),
            taffy::style::LengthPercentageAuto::auto(),
            taffy::style::LengthPercentageAuto::length(origin.x.value()),
        );
    tree.invalidate_layout(node)
}

pub(crate) fn taffy_style(tree: &NodeTree, node: NodeId) -> &taffy::Style<SmolStr> {
    match tree.arena.get(node) {
        Some(target) => target.style.inner(),
        None => {
            debug_assert!(false, "taffy queried a stale node's style");
            tree.arena
                .get(tree.root)
                .expect("根节点恒存活")
                .style
                .inner()
        }
    }
}

pub(crate) fn taffy_children(tree: &NodeTree, node: NodeId) -> &[NodeId] {
    tree.arena
        .get(node)
        .map_or(&[], |target| target.children.as_slice())
}

pub(crate) fn taffy_cache(tree: &mut NodeTree, node: NodeId) -> &mut taffy::Cache {
    tree.layout_cache.entry(node).or_default()
}

#[cfg(test)]
pub(crate) fn layout_cache(tree: &NodeTree, node: NodeId) -> Option<&taffy::Cache> {
    tree.layout_cache.get(&node)
}

pub(crate) fn set_local_layout(tree: &mut NodeTree, node: NodeId, layout: &taffy::Layout) {
    let epoch = tree.layout_epoch;
    let local = LogicalRect::new(
        px(layout.location.x),
        px(layout.location.y),
        px(layout.size.width),
        px(layout.size.height),
    );
    let border = LogicalEdges::new(
        px(layout.border.top),
        px(layout.border.right),
        px(layout.border.bottom),
        px(layout.border.left),
    );
    let content_width = px((layout.size.width
        - layout.border.left
        - layout.border.right
        - layout.padding.left
        - layout.padding.right)
        .max(0.0));
    let mut width_changed = false;
    if let Some(target) = tree.arena.get_mut(node) {
        width_changed = target.kind.text().is_some()
            && target.text_content_width.value().to_bits() != content_width.value().to_bits();
        target.local = local;
        target.local_border = border;
        target.text_content_width = content_width;
        target.content_offset = LogicalPoint::new(
            px(layout.border.left + layout.padding.left),
            px(layout.border.top + layout.padding.top),
        );
        target.laid_out_epoch = epoch;
        target.dirty.remove(DirtyFlags::LAYOUT);
    }
    tree.dirty_layout.remove(node);
    if width_changed {
        tree.mark_text_layout_dirty(node);
    }
}

pub(crate) fn measure_of(tree: &NodeTree, node: NodeId) -> Option<&MeasureFn> {
    tree.arena
        .get(node)
        .and_then(|target| target.measure.as_ref())
}

pub(crate) fn layout_override_of(tree: &NodeTree, node: NodeId) -> Option<LayoutOverride> {
    tree.arena
        .get(node)
        .and_then(|target| target.layout_override)
}

pub(crate) fn is_out_of_flow(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena.get(node).is_some_and(|target| {
        target.style.inner().position.is_out_of_flow()
            && !matches!(target.style.inner().display, taffy::style::Display::None)
    })
}

pub(crate) fn begin_layout(tree: &mut NodeTree) {
    tree.layout_epoch = tree.layout_epoch.wrapping_add(1);
    tree.layout_stats = LayoutStats::default();
}

pub(crate) fn take_report(tree: &mut NodeTree) -> LayoutReport {
    std::mem::take(&mut tree.layout_stats).into_report()
}

pub(crate) fn stats_mut(tree: &mut NodeTree) -> &mut LayoutStats {
    &mut tree.layout_stats
}

pub(crate) fn note_invalid_measure(
    tree: &mut NodeTree,
    node: NodeId,
    axis: Axis,
    value: LogicalPx,
) {
    if tree.invalid_measure.is_none() {
        tree.invalid_measure = Some(EngineError::InvalidMeasure { node, axis, value });
    }
}

pub(crate) fn take_invalid_measure(tree: &mut NodeTree) -> Option<EngineError> {
    tree.invalid_measure.take()
}

pub(crate) fn absolutize(tree: &mut NodeTree) {
    let epoch = tree.layout_epoch;
    tree.place_subtree(tree.root, LogicalPoint::ZERO, epoch, true, false);
    expand_hoisted_ancestors(tree);
}

pub(crate) fn place_subtree(
    tree: &mut NodeTree,
    node: NodeId,
    reference: LogicalPoint,
    epoch: u32,
    force: bool,
    parent_hidden: bool,
) {
    let Some(target) = tree.arena.get(node) else {
        return;
    };
    let hidden =
        parent_hidden || matches!(target.style.inner().display, taffy::style::Display::None);
    let layout_leaf = target.layout_override.is_some() || target.kind.is_leaf();
    let children_hidden = hidden || layout_leaf;
    let local = if hidden {
        LogicalRect::default()
    } else {
        target
            .layout_override
            .map_or(target.local, |over| over.rect)
    };
    let abs = LogicalRect::from_origin_size(reference + local.origin, local.size);
    let old = target.layout;
    let children = target.children.clone();
    let changed = abs != old.bounds;
    let fresh = target.laid_out_epoch == epoch;
    if hidden {
        if let Some(target) = tree.arena.get_mut(node) {
            target.dirty.remove(DirtyFlags::LAYOUT);
        }
        tree.dirty_layout.remove(node);
    }
    if !force && !fresh && !changed {
        return;
    }

    if changed {
        tree.layout_stats.moved += 1;
        if matches!(
            tree.arena.get(node).map(|target| &target.kind),
            Some(NodeKind::Geometry(_))
        ) {
            tree.geometry_damage_escalation = true;
        }
        if let Some(target) = tree.arena.get_mut(node) {
            target.damage = target.damage.union(old.bounds).union(abs);
            target.layout = LayoutBox {
                bounds: abs,
                border: target.local_border,
            };
            target.dirty.remove(DirtyFlags::REUSABLE);
        }
        tree.moved_nodes.insert(node);
        tree.dirty_settle.insert(node);
        let _ = tree.mark_paint(node);
    } else if fresh && let Some(target) = tree.arena.get_mut(node) {
        target.layout.border = target.local_border;
    }

    let hoisted: SmallVec<[NodeId; 2]> = if tree.hoisted.is_empty() {
        SmallVec::new()
    } else {
        tree.hoisted.get(&node).cloned().unwrap_or_default()
    };
    for child in children.iter().copied() {
        if tree.is_out_of_flow(child)
            && tree.layout_override_of(child).is_none()
            && !children_hidden
        {
            continue;
        }
        tree.place_subtree(child, abs.origin, epoch, false, children_hidden);
    }
    for child in hoisted.iter().copied() {
        if tree.layout_override_of(child).is_some() {
            continue;
        }
        tree.place_subtree(child, abs.origin, epoch, false, layout_leaf);
    }

    let mut union = hit_envelope(tree, node, abs);
    for child in children.iter().copied() {
        union = union.union(subtree_bounds(tree, child));
    }
    for child in &hoisted {
        union = union.union(subtree_bounds(tree, *child));
    }
    store_subtree_bounds(tree, node, union);
}

fn store_subtree_bounds(tree: &mut NodeTree, node: NodeId, bounds: LogicalRect) {
    let index = node.index() as usize;
    if tree.subtree_bounds.len() <= index {
        tree.subtree_bounds
            .resize_with(index + 1, LogicalRect::default);
    }
    tree.subtree_bounds[index] = bounds;
    tree.layout_stats.bounds_updated += 1;
}

fn expand_hoisted_ancestors(tree: &mut NodeTree) {
    if tree.moved_nodes.is_empty() {
        return;
    }
    let mut moved = std::mem::take(&mut tree.dirty_scratch);
    tree.moved_nodes.snapshot_into(&mut moved);

    let mut ancestors: Vec<(u32, NodeId)> = Vec::new();
    let mut seen: ahash::HashSet<NodeId> = ahash::HashSet::default();
    for &moved_node in &moved {
        if !tree.is_out_of_flow(moved_node) || tree.layout_override_of(moved_node).is_some() {
            continue;
        }
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
                break;
            }
            ancestors.push((depth, current));
            cursor = tree.arena.get(current).and_then(|target| target.parent);
        }
    }

    ancestors.sort_unstable_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    for &(_, current) in &ancestors {
        let Some(target) = tree.arena.get(current) else {
            continue;
        };
        let base = target.layout.bounds;
        let children = target.children.clone();
        let mut union = base;
        for child in children {
            union = union.union(subtree_bounds(tree, child));
        }
        let union = hit_envelope(tree, current, union);
        store_subtree_bounds(tree, current, union);
    }
    tree.recycle(moved);
}

pub(crate) fn recompute_bounds_upwards(tree: &mut NodeTree, from: NodeId) {
    let mut cursor = Some(from);
    while let Some(current) = cursor {
        let Some(target) = tree.arena.get(current) else {
            return;
        };
        let base = target.layout.bounds;
        let children = target.children.clone();
        let mut union = base;
        for child in children {
            union = union.union(subtree_bounds(tree, child));
        }
        let union = hit_envelope(tree, current, union);
        let cached = subtree_bounds(tree, current);
        if union == cached {
            return;
        }
        store_subtree_bounds(tree, current, union);
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
}

fn hit_envelope(tree: &NodeTree, node: NodeId, bounds: LogicalRect) -> LogicalRect {
    let Some(target) = tree.arena.get(node) else {
        return bounds;
    };
    let Some(local) = target
        .hit_shape()
        .and_then(crate::layout::hit_test::HitShape::bounds)
    else {
        return bounds;
    };
    let origin = target.layout.bounds.origin;
    bounds.union(LogicalRect::from_origin_size(
        origin + local.origin,
        local.size,
    ))
}

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
            .map_or(LogicalRect::default(), |target| target.layout.bounds);
        let children = tree.arena.get(node).map(|target| target.children());
        for child in children.unwrap_or_default() {
            let index = child.index() as usize;
            if let Some(bounds) = tree.subtree_bounds.get(index) {
                union = union.union(*bounds);
            }
        }
        let union = hit_envelope(tree, node, union);
        let index = node.index() as usize;
        if tree.subtree_bounds.len() <= index {
            tree.subtree_bounds
                .resize_with(index + 1, LogicalRect::default);
        }
        tree.subtree_bounds[index] = union;
    }
}

pub(crate) fn subtree_bounds(tree: &NodeTree, node: NodeId) -> LogicalRect {
    tree.subtree_bounds
        .get(node.index() as usize)
        .copied()
        .unwrap_or_default()
}

pub(crate) fn invalidate_caches_upwards(tree: &mut NodeTree, node: NodeId) {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if let Some(cache) = tree.layout_cache.get_mut(&current) {
            cache.clear();
        }
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
}

pub(crate) fn settle_frame(tree: &mut NodeTree) {
    let ids = tree.take_settle();
    for id in &ids {
        if let Some(target) = tree.arena.get_mut(*id) {
            target.damage = LogicalRect::default();
            target.dirty.insert(DirtyFlags::REUSABLE);
        }
    }
    tree.recycle(ids);
}

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

pub(crate) fn hit_test(tree: &NodeTree, point: LogicalPoint) -> Option<HitResult> {
    hit_test_layout(tree, point)
}

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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LayoutStats {
    cache_hits: usize,
    cache_misses: usize,
    laid_out: usize,
    moved: usize,
    bounds_updated: usize,
}

impl LayoutStats {
    pub(crate) fn note_cache_hit(&mut self) {
        self.cache_hits += 1;
    }

    pub(crate) fn note_cache_miss(&mut self) {
        self.cache_misses += 1;
    }

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

#[cfg(test)]
mod tests {
    use crate::{
        layout::LayoutOverride,
        math::{LogicalRect, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::{style::Style, style_builder::StyleBuilder},
    };

    fn fixed(width: f32, height: f32) -> Style {
        StyleBuilder::new().size(px(width), px(height)).build()
    }

    fn add(tree: &mut NodeTree, parent: NodeId, style: Style) -> NodeId {
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("挂载成功")
    }

    fn layout(tree: &mut NodeTree) {
        tree.compute_layout(taffy::geometry::Size {
            width: Some(200.0),
            height: Some(200.0),
        })
        .expect("布局成功");
        assert!(!tree.has_layout_work(), "布局后脏集合必须清空");
    }

    fn bounds(tree: &NodeTree, node: NodeId) -> LogicalRect {
        tree.get(node).expect("存在").layout().bounds
    }

    #[test]
    fn children_of_a_layout_override_node_are_zeroed() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let child = add(&mut tree, parent, fixed(20.0, 20.0));
        layout(&mut tree);
        assert!(
            !bounds(&tree, child).is_empty(),
            "前置：子节点先正常参与过布局"
        );

        tree.set_layout_override(
            parent,
            Some(LayoutOverride::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(50.0),
                px(50.0),
            ))),
        )
        .expect("节点存活");
        layout(&mut tree);

        assert_eq!(
            bounds(&tree, parent),
            LogicalRect::new(px(0.0), px(0.0), px(50.0), px(50.0))
        );
        assert!(
            bounds(&tree, child).is_empty(),
            "布局叶子的子节点必须零盒（不得冻结在上一轮的 local）：{:?}",
            bounds(&tree, child)
        );
    }

    #[test]
    fn a_descendant_of_a_layout_leaf_never_leaves_a_pending_layout_request() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let child = add(&mut tree, parent, fixed(20.0, 20.0));
        layout(&mut tree);

        tree.set_layout_override(
            parent,
            Some(LayoutOverride::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(50.0),
                px(50.0),
            ))),
        )
        .expect("节点存活");
        layout(&mut tree);

        tree.set_style(child, fixed(30.0, 30.0)).expect("节点存活");
        assert!(tree.has_layout_work(), "前置：确实有布局请求");
        layout(&mut tree);
        assert!(
            !tree.has_layout_work(),
            "布局叶子的子节点等不到 taffy 写回，LAYOUT 必须在这里消费（否则每帧空转）"
        );
    }

    #[test]
    fn writing_an_equal_style_is_a_no_op() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let style = fixed(40.0, 20.0);
        let node = add(&mut tree, root, style.clone());
        layout(&mut tree);

        tree.set_style(node, style).expect("节点存活");

        assert!(
            !tree.has_layout_work(),
            "等值写入不得请求布局（旧实现会清掉 node..root 的 taffy 缓存）"
        );
    }

    #[test]
    fn writing_an_equal_layout_override_is_a_no_op() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = add(&mut tree, root, fixed(40.0, 20.0));
        layout(&mut tree);

        tree.set_layout_override(node, None).expect("节点存活");
        assert!(!tree.has_layout_work(), "等值写入不得请求布局");

        let over = LayoutOverride::new(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)));
        tree.set_layout_override(node, Some(over))
            .expect("节点存活");
        assert!(tree.has_layout_work(), "真正的变更必须请求布局");
        layout(&mut tree);

        tree.set_layout_override(node, Some(over))
            .expect("节点存活");
        assert!(
            !tree.has_layout_work(),
            "同一份覆盖矩形再写一次不得请求布局"
        );
    }
}
