use crate::{
    interaction::ScrollSlot,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize},
    node::{id::NodeId, tree::NodeTree},
};

#[must_use]
pub(crate) fn clamp(value: LogicalPx, max: LogicalPx) -> LogicalPx {
    if max <= LogicalPx::ZERO {
        return LogicalPx::ZERO;
    }
    LogicalPx::new(value.value().clamp(0.0, max.value()))
}

#[must_use]
pub(crate) fn max_offset(slot: &ScrollSlot, viewport: LogicalSize) -> LogicalPoint {
    let max_x = if slot.axes.horizontal() {
        (slot.content.width - viewport.width).max(LogicalPx::ZERO)
    } else {
        LogicalPx::ZERO
    };
    let max_y = if slot.axes.vertical() {
        (slot.content.height - viewport.height).max(LogicalPx::ZERO)
    } else {
        LogicalPx::ZERO
    };
    LogicalPoint::new(max_x, max_y)
}

#[must_use]
pub(crate) fn clamp_offset(slot: &ScrollSlot, viewport: LogicalSize) -> LogicalPoint {
    let max = max_offset(slot, viewport);
    let x = if slot.axes.horizontal() {
        clamp(slot.offset.x, max.x)
    } else {
        LogicalPx::ZERO
    };
    let y = if slot.axes.vertical() {
        clamp(slot.offset.y, max.y)
    } else {
        LogicalPx::ZERO
    };
    LogicalPoint::new(x, y)
}

#[must_use]
pub(crate) fn viewport(tree: &NodeTree, node: NodeId) -> LogicalRect {
    tree.get(node).map_or_else(
        |_| LogicalRect::default(),
        |target| target.layout().bounds.inset(target.layout().border),
    )
}

pub(crate) fn scroll_ancestors(
    tree: &NodeTree,
    from: NodeId,
) -> impl Iterator<Item = (NodeId, ScrollSlot)> + '_ {
    let mut cursor = Some(from);
    std::iter::from_fn(move || {
        while let Some(node) = cursor {
            cursor = tree.get(node).ok().and_then(|target| target.parent());
            if let Some(slot) = tree.scroll_slot(node) {
                return Some((node, slot));
            }
        }
        None
    })
}

#[must_use]
pub(crate) fn scroll_ancestor(tree: &NodeTree, from: NodeId) -> Option<(NodeId, ScrollSlot)> {
    scroll_ancestors(tree, from).next()
}

#[must_use]
pub(crate) fn ancestor_offset(tree: &NodeTree, node: NodeId) -> LogicalPoint {
    let mut total = LogicalPoint::ZERO;
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        if let Some(slot) = tree.scroll_slot(current) {
            total = LogicalPoint::new(total.x + slot.offset.x, total.y + slot.offset.y);
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    total
}

#[must_use]
pub(crate) fn to_screen(tree: &NodeTree, node: NodeId, rect: LogicalRect) -> LogicalRect {
    let offset = ancestor_offset(tree, node);
    rect.translate(LogicalPoint::new(-offset.x, -offset.y))
}

#[cfg(test)]
mod tests {
    use super::{ancestor_offset, clamp_offset, max_offset, scroll_ancestor, to_screen, viewport};
    use crate::{
        interaction::{ScrollAxes, ScrollConfig, ScrollSlot},
        math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::style::{Clip, Style},
    };

    fn slot(axes: ScrollAxes, content: LogicalSize) -> ScrollSlot {
        ScrollSlot {
            offset: LogicalPoint::ZERO,
            content,
            axes,
            line_height: px(48.0),
        }
    }

    fn clipped_box(
        tree: &mut NodeTree,
        parent: crate::node::id::NodeId,
    ) -> crate::node::id::NodeId {
        let mut style = Style::default();
        style.clip = Clip::Bounds;
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("mount")
    }

    #[test]
    fn clamps_both_ends() {
        let content = LogicalSize::new(px(100.0), px(500.0));
        let viewport = LogicalSize::new(px(100.0), px(200.0));
        let mut slot = slot(ScrollAxes::Vertical, content);

        slot.offset = LogicalPoint::new(px(0.0), px(-50.0));
        assert_eq!(clamp_offset(&slot, viewport), LogicalPoint::ZERO);
        slot.offset = LogicalPoint::new(px(0.0), px(10_000.0));
        assert_eq!(clamp_offset(&slot, viewport).y, px(300.0));
        assert_eq!(
            max_offset(&slot, viewport),
            LogicalPoint::new(px(0.0), px(300.0))
        );
    }

    #[test]
    fn zero_range_never_scrolls() {
        let slot = slot(ScrollAxes::Vertical, LogicalSize::new(px(100.0), px(200.0)));
        let viewport = LogicalSize::new(px(100.0), px(200.0));

        assert_eq!(max_offset(&slot, viewport), LogicalPoint::ZERO);
        assert_eq!(clamp_offset(&slot, viewport), LogicalPoint::ZERO);
    }

    #[test]
    fn horizontal_containers_ignore_the_vertical_axis() {
        let mut slot = slot(
            ScrollAxes::Horizontal,
            LogicalSize::new(px(500.0), px(100.0)),
        );
        slot.offset = LogicalPoint::new(px(10_000.0), px(999.0));

        assert_eq!(
            clamp_offset(&slot, LogicalSize::new(px(100.0), px(100.0))),
            LogicalPoint::new(px(400.0), px(0.0))
        );
        assert_eq!(
            max_offset(&slot, LogicalSize::new(px(100.0), px(100.0))),
            LogicalPoint::new(px(400.0), px(0.0))
        );
    }

    #[test]
    fn dual_axis_containers_clamp_each_axis_independently() {
        let mut slot = slot(ScrollAxes::Both, LogicalSize::new(px(500.0), px(500.0)));
        let viewport = LogicalSize::new(px(100.0), px(200.0));
        assert_eq!(
            max_offset(&slot, viewport),
            LogicalPoint::new(px(400.0), px(300.0))
        );

        slot.offset = LogicalPoint::new(px(10_000.0), px(120.0));
        assert_eq!(
            clamp_offset(&slot, viewport),
            LogicalPoint::new(px(400.0), px(120.0)),
            "一轴到边界不影响另一轴"
        );
        slot.offset = LogicalPoint::new(px(-50.0), px(-50.0));
        assert_eq!(clamp_offset(&slot, viewport), LogicalPoint::ZERO);
    }

    #[test]
    fn disabled_axis_stays_zero_even_with_overflowing_content() {
        let mut slot = slot(
            ScrollAxes::Vertical,
            LogicalSize::new(px(5_000.0), px(5_000.0)),
        );
        slot.offset = LogicalPoint::new(px(321.0), px(222.0));
        let viewport = LogicalSize::new(px(100.0), px(200.0));

        assert_eq!(max_offset(&slot, viewport).x, LogicalPx::ZERO);
        assert_eq!(clamp_offset(&slot, viewport).x, LogicalPx::ZERO);
        assert_eq!(
            clamp_offset(&slot, viewport),
            LogicalPoint::new(px(0.0), px(222.0)),
            "未启用轴恒 0、启用轴照常夹紧"
        );
    }

    #[test]
    fn ancestor_offset_excludes_the_node_itself() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = clipped_box(&mut tree, root);
        let inner = clipped_box(&mut tree, outer);
        tree.ensure_interaction(outer).expect("exists").scroll = Some(ScrollSlot {
            offset: LogicalPoint::new(px(0.0), px(30.0)),
            content: LogicalSize::new(px(100.0), px(400.0)),
            axes: ScrollAxes::Vertical,
            line_height: px(48.0),
        });
        tree.ensure_interaction(inner).expect("exists").scroll = Some(ScrollSlot {
            offset: LogicalPoint::new(px(0.0), px(10.0)),
            content: LogicalSize::new(px(100.0), px(400.0)),
            axes: ScrollAxes::Vertical,
            line_height: px(48.0),
        });

        assert_eq!(
            ancestor_offset(&tree, outer),
            LogicalPoint::ZERO,
            "自身 offset 不算"
        );
        assert_eq!(
            ancestor_offset(&tree, inner),
            LogicalPoint::new(px(0.0), px(30.0))
        );
        let rect = LogicalRect::new(px(0.0), px(100.0), px(50.0), px(20.0));
        assert_eq!(
            to_screen(&tree, inner, rect).origin,
            LogicalPoint::new(px(0.0), px(70.0))
        );
    }

    #[test]
    fn viewport_is_the_padding_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = clipped_box(&mut tree, root);
        let bounds = crate::math::LogicalRect::new(px(10.0), px(20.0), px(100.0), px(80.0));
        let border = crate::math::LogicalEdges::uniform(px(2.0));
        tree.set_layout_for_test(node, crate::node::tree::LayoutBox { bounds, border });

        assert_eq!(
            viewport(&tree, node),
            crate::math::LogicalRect::new(px(12.0), px(22.0), px(96.0), px(76.0))
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
                offset: LogicalPoint::ZERO,
                content: LogicalSize::new(px(100.0), px(400.0)),
                axes: config.axes,
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
