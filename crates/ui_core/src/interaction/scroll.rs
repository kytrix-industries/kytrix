//! 滚动：夹紧、视口、最近滚动祖先、滚轮目标（纯逻辑；状态在 `Node.interaction.scroll`）。
//!
//! 偏移是 `Transient` 语义（不登记依赖、不标脏位）；应用点是 `paint` 的累计偏移与 `hit_test` 的点换算
//! （`design/phase-10-interaction.md` §7）。

use crate::{
    interaction::{ScrollAxis, ScrollSlot},
    math::{Point, Rect, Size},
    node::{id::NodeId, tree::NodeTree},
};

/// 让 `point` 落在 `[0, max]` 内。
#[must_use]
pub(crate) fn clamp(value: f32, max: f32) -> f32 {
    if max <= 0.0 {
        return 0.0;
    }
    value.clamp(0.0, max)
}

/// 某轴的可用滚动范围（`content - viewport`，`>= 0`）。
#[must_use]
pub(crate) fn max_offset(slot: &ScrollSlot, viewport: Size) -> Point {
    let max_x = (slot.content.width - viewport.width).max(0.0);
    let max_y = (slot.content.height - viewport.height).max(0.0);
    match slot.axis {
        ScrollAxis::Vertical => Point::new(0.0, max_y),
        ScrollAxis::Horizontal => Point::new(max_x, 0.0),
    }
}

/// 夹紧后的偏移（只 clamp 该容器的轴；另一轴恒 0）。
#[must_use]
pub(crate) fn clamp_offset(slot: &ScrollSlot, viewport: Size) -> Point {
    let max = max_offset(slot, viewport);
    match slot.axis {
        ScrollAxis::Vertical => Point::new(0.0, clamp(slot.offset.y, max.y)),
        ScrollAxis::Horizontal => Point::new(clamp(slot.offset.x, max.x), 0.0),
    }
}

/// 节点自身的滚动视口（`bounds.inset(border)` = padding box，屏幕空间）。
#[must_use]
pub(crate) fn viewport(tree: &NodeTree, node: NodeId) -> Rect {
    tree.get(node).map_or_else(
        |_| Rect::default(),
        |target| target.layout().bounds.inset(target.layout().border),
    )
}

/// 从 `from`（含自身）向上找第一个挂了滚动槽的节点。
#[must_use]
pub(crate) fn scroll_ancestor(tree: &NodeTree, from: NodeId) -> Option<(NodeId, ScrollSlot)> {
    let mut cursor = Some(from);
    while let Some(node) = cursor {
        if let Some(slot) = tree.scroll_slot(node) {
            return Some((node, slot));
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    None
}

/// 某个节点的**祖先**滚动偏移之和（不含自身；屏幕 = 内容 − 该值）。
///
/// 用途：`damage` / `resolve_placement` 需要把**内容坐标**换算成屏幕坐标，而容器自身的 `offset`
/// 只作用于它的子树，不作用于它自己的 bounds。
#[must_use]
pub(crate) fn ancestor_offset(tree: &NodeTree, node: NodeId) -> Point {
    let mut total = Point::ZERO;
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        if let Some(slot) = tree.scroll_slot(current) {
            total = Point::new(total.x + slot.offset.x, total.y + slot.offset.y);
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    total
}

/// 把内容坐标的矩形换算到屏幕坐标（减去祖先滚动偏移）。
#[must_use]
pub(crate) fn to_screen(tree: &NodeTree, node: NodeId, rect: Rect) -> Rect {
    let offset = ancestor_offset(tree, node);
    rect.translate(Point::new(-offset.x, -offset.y))
}

/// 滚轮事件 → 目标偏移（`drives` 已按 `Shift` 规则选出，§5.1）。
#[must_use]
pub(crate) fn wheel_target(slot: &ScrollSlot, drives: f32) -> Point {
    match slot.axis {
        ScrollAxis::Vertical => Point::new(0.0, slot.offset.y - drives * slot.line_height),
        ScrollAxis::Horizontal => Point::new(slot.offset.x - drives * slot.line_height, 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ancestor_offset, clamp_offset, max_offset, scroll_ancestor, to_screen, viewport,
        wheel_target,
    };
    use crate::{
        interaction::{ScrollAxis, ScrollConfig, ScrollSlot},
        math::{Point, Rect, Size},
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::style::{Clip, Style},
    };

    fn slot(axis: ScrollAxis, content: Size) -> ScrollSlot {
        ScrollSlot {
            offset: Point::ZERO,
            content,
            axis,
            line_height: 48.0,
        }
    }

    fn clipped_box(
        tree: &mut NodeTree,
        parent: crate::node::id::NodeId,
    ) -> crate::node::id::NodeId {
        let style = Style {
            clip: Clip::Bounds,
            ..Style::default()
        };
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("mount")
    }

    #[test]
    fn clamps_both_ends() {
        let content = Size::new(100.0, 500.0);
        let viewport = Size::new(100.0, 200.0);
        let mut slot = slot(ScrollAxis::Vertical, content);

        slot.offset = Point::new(0.0, -50.0);
        assert_eq!(clamp_offset(&slot, viewport), Point::ZERO);
        slot.offset = Point::new(0.0, 10_000.0);
        assert_eq!(clamp_offset(&slot, viewport).y, 300.0);
        assert_eq!(max_offset(&slot, viewport), Point::new(0.0, 300.0));
    }

    #[test]
    fn zero_range_never_scrolls() {
        let slot = slot(ScrollAxis::Vertical, Size::new(100.0, 200.0));
        let viewport = Size::new(100.0, 200.0);

        assert_eq!(max_offset(&slot, viewport), Point::ZERO);
        assert_eq!(clamp_offset(&slot, viewport), Point::ZERO);
    }

    #[test]
    fn horizontal_containers_ignore_the_vertical_axis() {
        let mut slot = slot(ScrollAxis::Horizontal, Size::new(500.0, 100.0));
        slot.offset = Point::new(10_000.0, 999.0);

        assert_eq!(
            clamp_offset(&slot, Size::new(100.0, 100.0)),
            Point::new(400.0, 0.0)
        );
    }

    #[test]
    fn ancestor_offset_excludes_the_node_itself() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = clipped_box(&mut tree, root);
        let inner = clipped_box(&mut tree, outer);
        tree.ensure_interaction(outer).expect("exists").scroll = Some(ScrollSlot {
            offset: Point::new(0.0, 30.0),
            content: Size::new(100.0, 400.0),
            axis: ScrollAxis::Vertical,
            line_height: 48.0,
        });
        tree.ensure_interaction(inner).expect("exists").scroll = Some(ScrollSlot {
            offset: Point::new(0.0, 10.0),
            content: Size::new(100.0, 400.0),
            axis: ScrollAxis::Vertical,
            line_height: 48.0,
        });

        assert_eq!(
            ancestor_offset(&tree, outer),
            Point::ZERO,
            "自身 offset 不算"
        );
        assert_eq!(ancestor_offset(&tree, inner), Point::new(0.0, 30.0));
        let rect = Rect::new(0.0, 100.0, 50.0, 20.0);
        assert_eq!(to_screen(&tree, inner, rect).origin, Point::new(0.0, 70.0));
    }

    #[test]
    fn wheel_target_uses_the_line_height() {
        let slot = slot(ScrollAxis::Vertical, Size::new(100.0, 500.0));

        // 滚轮向上（drives > 0）= 内容下移 = offset 减小。
        assert_eq!(wheel_target(&slot, 1.0), Point::new(0.0, -48.0));
        assert_eq!(wheel_target(&slot, -2.0), Point::new(0.0, 96.0));
    }

    #[test]
    fn viewport_is_the_padding_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = clipped_box(&mut tree, root);
        let bounds = crate::math::Rect::new(10.0, 20.0, 100.0, 80.0);
        let border = crate::math::Edges::uniform(2.0);
        tree.set_layout_for_test(
            node,
            crate::node::tree::LayoutBox { bounds, border },
            crate::node::tree::LayoutBox::default(),
        );

        assert_eq!(
            viewport(&tree, node),
            crate::math::Rect::new(12.0, 22.0, 96.0, 76.0)
        );
    }

    #[test]
    fn nearest_scroll_ancestor_wins() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = clipped_box(&mut tree, root);
        let inner = clipped_box(&mut tree, outer);
        let leaf = NodeBuilder::boxed().build(&mut tree, inner).expect("mount");
        let config = ScrollConfig::default();
        for node in [outer, inner] {
            tree.ensure_interaction(node).expect("exists").scroll = Some(ScrollSlot {
                offset: Point::ZERO,
                content: Size::new(100.0, 400.0),
                axis: config.axis,
                line_height: config.line_height,
            });
        }

        assert_eq!(
            scroll_ancestor(&tree, leaf).map(|(node, _)| node),
            Some(inner)
        );
        assert_eq!(scroll_ancestor(&tree, root), None);
    }
}
