use crate::{
    math::LogicalRect,
    node::{id::NodeId, tree::NodeTree},
    paint::style::Clip,
    popup::{Anchor, PopupError},
};

pub(crate) fn resolve_anchor(tree: &NodeTree, anchor: Anchor) -> Result<LogicalRect, PopupError> {
    let missing = |node| PopupError::AnchorNodeMissing { node };
    match anchor {
        Anchor::WindowRect(rect) => Ok(rect),
        Anchor::Node { node } => tree.node_screen_rect(node).ok_or_else(|| missing(node)),
        Anchor::NodeRect { node, rect } => {
            let bounds = tree.node_screen_rect(node).ok_or_else(|| missing(node))?;
            let offset = tree.content_offset(node);
            Ok(LogicalRect::from_origin_size(
                bounds.origin + offset + rect.origin,
                rect.size,
            ))
        }
    }
}

fn viewport_rect(tree: &NodeTree) -> LogicalRect {
    tree.get(tree.root())
        .map_or(LogicalRect::default(), |root| root.layout().bounds)
}

fn clipped_visible_rect(tree: &NodeTree, node: NodeId) -> Option<LogicalRect> {
    if !tree.is_effectively_visible(node) {
        return None;
    }
    let mut visible = tree.node_screen_rect(node)?.intersect(viewport_rect(tree));
    let mut cursor = tree.get(node).ok()?.parent();
    while let Some(current) = cursor {
        let target = tree.get(current).ok()?;
        if target.clip() == Clip::Bounds {
            let clip = tree
                .node_screen_rect(current)?
                .inset(target.layout().border);
            visible = visible.intersect(clip);
        }
        cursor = target.parent();
    }
    Some(visible)
}

fn overlaps_inclusive(a: LogicalRect, b: LogicalRect) -> bool {
    a.is_convertible()
        && b.is_convertible()
        && a.origin.x <= b.max().x
        && b.origin.x <= a.max().x
        && a.origin.y <= b.max().y
        && b.origin.y <= a.max().y
}

pub(crate) fn anchor_visible(tree: &NodeTree, anchor: Anchor) -> bool {
    let Ok(rect) = resolve_anchor(tree, anchor) else {
        return false;
    };
    if !rect.is_convertible() {
        return false;
    }
    let viewport = viewport_rect(tree);
    if !overlaps_inclusive(rect, viewport) {
        return false;
    }
    match anchor {
        Anchor::WindowRect(_) => true,
        Anchor::Node { node } | Anchor::NodeRect { node, .. } => clipped_visible_rect(tree, node)
            .is_some_and(|visible| overlaps_inclusive(visible, rect)),
    }
}
