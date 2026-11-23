use std::sync::Arc;

use crate::{
    math::{Point, Rect, Size, Transform},
    node::{id::NodeId, tree::NodeTree},
    paint::style::Clip,
};

const MAX_CLIP_DEPTH: usize = crate::paint::scene::MAX_CLIP_DEPTH as usize;

#[derive(Clone)]
pub enum HitShape {
    Rect(Rect),
    RoundedRect { rect: Rect, corner_radius: [f32; 4] },
    Circle { center: Point, radius: f32 },
    Ellipse { center: Point, radius: Size },
    Path { points: Arc<[Point]>, bounds: Rect },
    Custom(Arc<dyn Fn(Point) -> bool>),
}

impl HitShape {
    pub fn custom(predicate: impl Fn(Point) -> bool + 'static) -> Self {
        Self::Custom(Arc::new(predicate))
    }

    #[must_use]
    pub fn path(points: impl Into<Arc<[Point]>>) -> Self {
        let points: Arc<[Point]> = points.into();
        let bounds = if points.len() >= 3 && points.iter().all(|point| point.is_finite()) {
            let mut min = Point::new(f32::INFINITY, f32::INFINITY);
            let mut max = Point::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
            for point in points.iter() {
                min.x = min.x.min(point.x);
                min.y = min.y.min(point.y);
                max.x = max.x.max(point.x);
                max.y = max.y.max(point.y);
            }
            Rect::new(min.x, min.y, max.x - min.x, max.y - min.y)
        } else {
            Rect::default()
        };
        Self::Path { points, bounds }
    }

    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        match self {
            Self::Rect(rect) => Some(*rect),
            Self::RoundedRect { rect, .. } => Some(*rect),
            Self::Circle { center, radius } => {
                let radius = *radius;
                Some(Rect::new(
                    center.x - radius,
                    center.y - radius,
                    radius * 2.0,
                    radius * 2.0,
                ))
            }
            Self::Ellipse { center, radius } => Some(Rect::new(
                center.x - radius.width,
                center.y - radius.height,
                radius.width * 2.0,
                radius.height * 2.0,
            )),
            Self::Path { bounds, .. } => Some(*bounds),
            Self::Custom(_) => None,
        }
    }

    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        if !point.is_finite() {
            return false;
        }
        match self {
            Self::Rect(rect) => rect.contains(point),
            Self::RoundedRect {
                rect,
                corner_radius,
            } => rounded_rect_contains(*rect, *corner_radius, point),
            Self::Circle { center, radius } => {
                let radius = *radius;
                if !radius.is_finite() || radius <= 0.0 || !center.is_finite() {
                    return false;
                }
                let dx = point.x - center.x;
                let dy = point.y - center.y;
                dx * dx + dy * dy <= radius * radius
            }
            Self::Ellipse { center, radius } => {
                let (rx, ry) = (radius.width, radius.height);
                if !rx.is_finite() || !ry.is_finite() || rx <= 0.0 || ry <= 0.0 {
                    return false;
                }
                let nx = (point.x - center.x) / rx;
                let ny = (point.y - center.y) / ry;
                nx * nx + ny * ny <= 1.0
            }
            Self::Path { points, bounds } => {
                if points.len() < 3 || bounds.is_empty() || !bounds.contains(point) {
                    return false;
                }
                let mut inside = false;
                for index in 0..points.len() {
                    let a = points[index];
                    let b = points[(index + 1) % points.len()];
                    if (a.y > point.y) != (b.y > point.y) {
                        let t = (point.y - a.y) / (b.y - a.y);
                        if point.x < a.x + t * (b.x - a.x) {
                            inside = !inside;
                        }
                    }
                }
                inside
            }
            Self::Custom(predicate) => predicate(point),
        }
    }
}

fn rounded_rect_contains(rect: Rect, corner_radius: [f32; 4], point: Point) -> bool {
    if !rect.contains(point) {
        return false;
    }
    let (width, height) = (rect.size.width, rect.size.height);
    let radii = crate::paint::instance::normalize_corner_radius(corner_radius, width, height);
    let local = Point::new(point.x - rect.origin.x, point.y - rect.origin.y);
    let corners = [
        (0.0, 0.0, radii[0]),
        (width, 0.0, radii[1]),
        (width, height, radii[2]),
        (0.0, height, radii[3]),
    ];
    for (cx, cy, radius) in corners {
        if radius <= 0.0 {
            continue;
        }
        let in_corner_box = (if cx == 0.0 {
            local.x < radius
        } else {
            local.x > width - radius
        }) && (if cy == 0.0 {
            local.y < radius
        } else {
            local.y > height - radius
        });
        if !in_corner_box {
            continue;
        }
        let center_x = if cx == 0.0 { radius } else { width - radius };
        let center_y = if cy == 0.0 { radius } else { height - radius };
        let dx = local.x - center_x;
        let dy = local.y - center_y;
        if dx * dx + dy * dy > radius * radius {
            return false;
        }
    }
    true
}

impl std::fmt::Debug for HitShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rect(rect) => f.debug_tuple("Rect").field(rect).finish(),
            Self::RoundedRect {
                rect,
                corner_radius,
            } => f
                .debug_struct("RoundedRect")
                .field("rect", rect)
                .field("corner_radius", corner_radius)
                .finish(),
            Self::Circle { center, radius } => f
                .debug_struct("Circle")
                .field("center", center)
                .field("radius", radius)
                .finish(),
            Self::Ellipse { center, radius } => f
                .debug_struct("Ellipse")
                .field("center", center)
                .field("radius", radius)
                .finish(),
            Self::Path { points, bounds } => f
                .debug_struct("Path")
                .field("points", &points.len())
                .field("bounds", bounds)
                .finish(),
            Self::Custom(_) => f.write_str("Custom(<predicate>)"),
        }
    }
}

impl PartialEq for HitShape {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Rect(a), Self::Rect(b)) => a == b,
            (
                Self::RoundedRect {
                    rect: ar,
                    corner_radius: ac,
                },
                Self::RoundedRect {
                    rect: br,
                    corner_radius: bc,
                },
            ) => ar == br && ac == bc,
            (
                Self::Circle {
                    center: ac,
                    radius: ar,
                },
                Self::Circle {
                    center: bc,
                    radius: br,
                },
            ) => ac == bc && ar == br,
            (
                Self::Ellipse {
                    center: ac,
                    radius: ar,
                },
                Self::Ellipse {
                    center: bc,
                    radius: br,
                },
            ) => ac == bc && ar == br,
            (
                Self::Path {
                    points: ap,
                    bounds: ab,
                },
                Self::Path {
                    points: bp,
                    bounds: bb,
                },
            ) => ab == bb && ap == bp,
            (Self::Custom(a), Self::Custom(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipChain {
    len: u8,
    rects: [Rect; MAX_CLIP_DEPTH],
}

impl Default for ClipChain {
    fn default() -> Self {
        Self {
            len: 0,
            rects: [Rect::default(); MAX_CLIP_DEPTH],
        }
    }
}

impl ClipChain {
    #[must_use]
    pub fn as_slice(&self) -> &[Rect] {
        &self.rects[..usize::from(self.len)]
    }

    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, rect: Rect) {
        if usize::from(self.len) >= MAX_CLIP_DEPTH {
            let last = &mut self.rects[usize::from(self.len) - 1];
            *last = last.intersect(rect);
            return;
        }
        self.rects[usize::from(self.len)] = rect;
        self.len += 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitResult {
    pub node: NodeId,
    pub local_point: Point,
    pub clip_chain: ClipChain,
}

fn negate(offset: Point) -> Point {
    Point::new(-offset.x, -offset.y)
}

fn transformed_box(bounds: Rect, transform: Transform) -> Rect {
    let local = Rect::new(0.0, 0.0, bounds.size.width, bounds.size.height);
    let visual = transform.transform_rect(local);
    Rect::from_origin_size(bounds.origin + visual.origin, visual.size)
}

pub(crate) fn hit_test(tree: &NodeTree, point: Point) -> Option<HitResult> {
    if !point.is_finite() {
        tree.note_hit_test_skip();
        return None;
    }
    let root = tree.root();
    let root_bounds = tree.get(root).ok()?.layout().bounds;
    if !root_bounds.is_finite() || !root_bounds.contains(point) {
        return None;
    }
    let transforms = tree.has_geometry_transforms();
    pick(
        tree,
        root,
        point,
        Point::ZERO,
        ClipChain::default(),
        false,
        transforms,
    )
}

fn pick(
    tree: &NodeTree,
    node: NodeId,
    point: Point,
    offset: Point,
    mut chain: ClipChain,
    inherited_disabled: bool,
    transforms: bool,
) -> Option<HitResult> {
    let slot = tree.get(node).ok()?;
    let bounds = slot.layout().bounds.translate(negate(offset));
    let ancestors = chain;

    if !tree.is_visible_locally(node) {
        return None;
    }

    let disabled = inherited_disabled || !tree.is_hittable(node);

    let mut clipped = false;
    if slot.clip() == Clip::Bounds {
        let clip_rect = bounds.inset(slot.layout().border);
        if !clip_rect.is_finite() {
            tree.note_hit_test_skip();
            return None;
        }
        if clip_rect.contains(point) {
            chain.push(clip_rect);
        } else {
            clipped = true;
        }
    }

    if !clipped {
        let child_offset = offset
            + tree
                .scroll_slot(node)
                .map_or(Point::ZERO, |slot| slot.offset);
        if !child_offset.is_finite() {
            tree.note_hit_test_skip();
            return None;
        }
        for child in slot.children().iter().rev() {
            let Ok(child_slot) = tree.get(*child) else {
                continue;
            };
            let mut subtree = tree.subtree_bounds(*child).translate(negate(child_offset));
            if transforms && child_slot.kind().is_geometry() {
                let transform = tree.geometry_transform(*child);
                if transform != Transform::IDENTITY {
                    let child_bounds = child_slot.layout().bounds.translate(negate(child_offset));
                    if child_bounds.is_finite() {
                        subtree = subtree.union(transformed_box(child_bounds, transform));
                    }
                }
            }
            if !subtree.contains(point) {
                continue;
            }
            if let Some(hit) = pick(
                tree,
                *child,
                point,
                child_offset,
                chain,
                disabled,
                transforms,
            ) {
                return Some(hit);
            }
        }
    }

    if disabled {
        return None;
    }
    let local_point = Point::new(point.x - bounds.origin.x, point.y - bounds.origin.y);
    if !bounds.is_finite() {
        tree.note_hit_test_skip();
        return None;
    }
    if transforms && slot.kind().is_geometry() {
        let transform = tree.geometry_transform(node);
        if transform != Transform::IDENTITY {
            return hit_transformed(
                tree,
                node,
                slot,
                point,
                bounds,
                transform,
                local_point,
                ancestors,
            );
        }
    }
    let shape = slot.hit_shape();
    let region = match shape.and_then(HitShape::bounds) {
        Some(shape_bounds) => bounds.union(Rect::from_origin_size(
            bounds.origin + shape_bounds.origin,
            shape_bounds.size,
        )),
        None => bounds,
    };
    if !region.contains(point) {
        return None;
    }
    if let Some(shape) = shape
        && !shape.contains(local_point)
    {
        return None;
    }
    Some(HitResult {
        node,
        local_point,
        clip_chain: ancestors,
    })
}

#[inline(never)]
fn hit_transformed(
    tree: &NodeTree,
    node: NodeId,
    slot: &crate::node::tree::Node,
    point: Point,
    bounds: Rect,
    transform: Transform,
    local_point: Point,
    ancestors: ClipChain,
) -> Option<HitResult> {
    let Some(inverse) = transform.inverse() else {
        tree.note_hit_test_skip();
        return None;
    };
    let local = inverse.apply(local_point);
    let local_box = Rect::new(0.0, 0.0, bounds.size.width, bounds.size.height);
    let shape = slot.hit_shape();
    let local_region = match shape.and_then(HitShape::bounds) {
        Some(shape_bounds) => local_box.union(shape_bounds),
        None => local_box,
    };
    let world_region =
        Rect::from_origin_size(bounds.origin + local_region.origin, local_region.size);
    if !local.is_finite() || !transformed_box(world_region, transform).contains(point) {
        return None;
    }
    if !local_region.contains(local) {
        return None;
    }
    if let Some(shape) = shape
        && !shape.contains(local)
    {
        return None;
    }
    Some(HitResult {
        node,
        local_point: local,
        clip_chain: ancestors,
    })
}
#[cfg(test)]
mod tests {
    use crate::{
        layout::hit_test::{ClipChain, HitShape, hit_test},
        math::{Point, Rect},
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        paint::{
            canvas::Canvas,
            style::{Clip, Style, StylePatch},
            style_builder::StyleBuilder,
        },
    };

    fn fixed(width: f32, height: f32) -> Style {
        StyleBuilder::new().size(width, height).build()
    }

    fn absolute(size: f32) -> Style {
        StyleBuilder::new()
            .absolute()
            .left(0.0)
            .top(0.0)
            .size(size, size)
            .build()
    }

    fn add(tree: &mut NodeTree, parent: NodeId, style: Style) -> NodeId {
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("挂载成功")
    }

    fn layout(tree: &mut NodeTree) {
        let _ = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(200.0),
                height: Some(200.0),
            })
            .expect("布局成功");
    }

    fn hit_node(tree: &NodeTree, x: f32, y: f32) -> Option<NodeId> {
        hit_test(tree, Point::new(x, y)).map(|hit| hit.node)
    }

    #[test]
    fn deepest_last_child_wins() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = add(&mut tree, root, fixed(100.0, 100.0));
        let lower = add(&mut tree, container, absolute(100.0));
        let upper = add(&mut tree, container, absolute(100.0));
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(50.0, 50.0)).expect("命中");
        assert_eq!(hit.node, upper, "后创建的子节点绘制在上层");
        assert_ne!(hit.node, lower);
        assert_eq!(hit.local_point, Point::new(50.0, 50.0));
        assert!(hit.clip_chain.is_empty());
    }

    #[test]
    fn child_beats_parent_background() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let child = add(&mut tree, parent, fixed(20.0, 20.0));
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(child), "子内容压过父背景");
        assert_eq!(hit_node(&tree, 50.0, 50.0), Some(parent), "父背景可命中");
    }

    #[test]
    fn clip_prunes_subtree_and_fills_chain() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut scroller_style = fixed(40.0, 40.0);
        scroller_style.clip = Clip::Bounds;
        scroller_style.border(2.0);
        let scroller = add(&mut tree, root, scroller_style);
        let overflowing = add(&mut tree, scroller, fixed(100.0, 100.0));
        layout(&mut tree);

        let inside = hit_test(&tree, Point::new(10.0, 10.0)).expect("命中");
        assert_eq!(inside.node, overflowing);
        assert_eq!(inside.clip_chain.len(), 1, "裁剪祖先进链");
        assert_eq!(
            inside.clip_chain.as_slice()[0],
            Rect::new(2.0, 2.0, 36.0, 36.0),
            "padding box = border box 内缩 border"
        );

        let border_band = hit_test(&tree, Point::new(1.0, 1.0)).expect("命中父");
        assert_eq!(border_band.node, scroller, "被裁剪的子节点不可命中");
        assert!(
            border_band.clip_chain.is_empty(),
            "目标自身的 clip 不进 clip_chain"
        );

        assert_eq!(hit_node(&tree, 60.0, 60.0), Some(root));
    }

    #[test]
    fn clip_folding_does_not_leak_into_sibling_subtrees() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut parent = root;
        for _ in 0..=super::MAX_CLIP_DEPTH {
            let mut style = fixed(100.0, 100.0);
            style.clip = Clip::Bounds;
            parent = add(&mut tree, parent, style);
        }
        let mut narrow = fixed(10.0, 10.0);
        narrow.clip = Clip::Bounds;
        let left = add(&mut tree, parent, narrow);
        let right = add(&mut tree, parent, fixed(80.0, 80.0));
        layout(&mut tree);

        let _ = hit_node(&tree, 5.0, 5.0);
        assert_eq!(
            hit_node(&tree, 40.0, 40.0),
            Some(right),
            "右兄弟的命中不得被左兄弟的折叠 clip 影响"
        );
        let _ = left;
    }

    #[test]
    fn nested_clips_accumulate_outer_to_inner() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut outer_style = fixed(100.0, 100.0);
        outer_style.clip = Clip::Bounds;
        let outer = add(&mut tree, root, outer_style);
        let mut inner_style = fixed(60.0, 60.0);
        inner_style.clip = Clip::Bounds;
        let inner = add(&mut tree, outer, inner_style);
        let leaf = add(&mut tree, inner, fixed(20.0, 20.0));
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(5.0, 5.0)).expect("命中");
        assert_eq!(hit.node, leaf);
        assert_eq!(hit.clip_chain.len(), 2, "两层 clip 都进链");
        assert_eq!(
            hit.clip_chain.as_slice()[0],
            Rect::new(0.0, 0.0, 100.0, 100.0),
            "外层在前"
        );
        assert_eq!(
            hit.clip_chain.as_slice()[1],
            Rect::new(0.0, 0.0, 60.0, 60.0),
            "内层在后"
        );
    }

    #[test]
    fn clip_chain_caps_at_eight() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut parent = root;
        for _ in 0..10 {
            let mut style = fixed(100.0, 100.0);
            style.clip = Clip::Bounds;
            parent = add(&mut tree, parent, style);
        }
        let leaf = add(&mut tree, parent, fixed(10.0, 10.0));
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(1.0, 1.0)).expect("命中");
        assert_eq!(hit.node, leaf);
        assert_eq!(
            hit.clip_chain.len(),
            crate::paint::scene::MAX_CLIP_DEPTH as usize,
            "超过深度上限时截断，不 panic"
        );
    }

    #[test]
    fn hit_shape_filters_local_points() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 40.0, 20.0)))
            .style(fixed(40.0, 20.0))
            .hit_shape(HitShape::custom(|local: Point| local.x < 20.0))
            .build(&mut tree, root)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(canvas));
        assert_eq!(
            hit_node(&tree, 30.0, 10.0),
            Some(root),
            "hit_shape 拒绝 ⇒ 落到下层（根背景）"
        );
    }

    #[test]
    fn hit_shape_receives_node_local_coords() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let node = NodeBuilder::boxed()
            .style({
                let mut style = fixed(30.0, 30.0);
                style.margin_sides(20.0, 0.0, 0.0, 10.0);
                style
            })
            .hit_shape(HitShape::custom(|local: Point| {
                local.x >= 0.0 && local.y >= 0.0 && local.x < 30.0 && local.y < 30.0
            }))
            .build(&mut tree, parent)
            .expect("挂载成功");
        layout(&mut tree);

        let origin = tree.get(node).expect("存在").layout().bounds.origin;
        assert_eq!(origin, Point::new(10.0, 20.0), "margin 决定位置");
        let hit = hit_test(&tree, Point::new(15.0, 25.0)).expect("命中");
        assert_eq!(hit.node, node);
        assert_eq!(hit.local_point, Point::new(5.0, 5.0));
    }

    #[test]
    fn display_none_is_not_hittable() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut hidden = fixed(100.0, 100.0);
        hidden.display(taffy::style::Display::None);
        let _hidden = add(&mut tree, root, hidden);
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(root));
    }

    #[test]
    fn visible_false_skips_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed(100.0, 100.0))
            .bind_style(|_cx| StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            })
            .build(&mut tree, root)
            .expect("挂载成功");
        tree.set_style_patch(
            node,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        let _ = node;
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 10.0, 10.0),
            Some(root),
            "visible=false 的节点不参与命中"
        );
    }

    #[test]
    fn points_outside_the_window_miss() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = add(&mut tree, root, fixed(50.0, 50.0));
        layout(&mut tree);

        assert!(hit_test(&tree, Point::new(-1.0, 5.0)).is_none());
        assert!(hit_test(&tree, Point::new(500.0, 5.0)).is_none());
    }

    #[test]
    fn overlay_like_last_sibling_wins_regardless_of_size() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let base = add(&mut tree, root, fixed(100.0, 100.0));
        let overlay = add(&mut tree, root, absolute(20.0));
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 5.0, 5.0),
            Some(overlay),
            "绘制序靠后的浮层优先"
        );
        assert_eq!(hit_node(&tree, 50.0, 50.0), Some(base));
    }

    #[test]
    fn override_node_is_hittable_at_its_rect() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 10.0, 30.0, 30.0))
            .build(&mut tree, pane)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 15.0, 15.0), Some(canvas));
        assert_eq!(
            hit_node(&tree, 5.0, 5.0),
            Some(pane),
            "override 之外的区域仍是父节点"
        );
    }

    #[test]
    fn local_point_is_border_box_relative() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let node = add(&mut tree, parent, {
            let mut style = fixed(30.0, 40.0);
            style.margin_sides(20.0, 0.0, 0.0, 10.0);
            style
        });
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(15.0, 25.0)).expect("命中");
        assert_eq!(hit.node, node);
        assert_eq!(hit.local_point, Point::new(5.0, 5.0));
    }

    #[test]
    fn overlapping_siblings_later_wins() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let first = add(&mut tree, root, absolute(100.0));
        let second = add(&mut tree, root, absolute(100.0));
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 50.0, 50.0), Some(second));
        assert_ne!(hit_node(&tree, 50.0, 50.0), Some(first));
    }

    #[test]
    fn clip_chain_is_copy_and_default_is_empty() {
        let chain = ClipChain::default();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        assert!(chain.as_slice().is_empty());
        let copy = chain;
        assert_eq!(copy, chain);
    }

    #[test]
    fn kind_of_override_node_stays_canvas() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(matches!(
            tree.get(canvas).expect("存在").kind(),
            NodeKind::Canvas(_)
        ));
    }

    #[test]
    fn overflowing_child_hits_through_unclipped_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let overflow = add(
            &mut tree,
            parent,
            StyleBuilder::new()
                .absolute()
                .left(150.0)
                .top(10.0)
                .size(30.0, 30.0)
                .build(),
        );
        layout(&mut tree);

        assert_eq!(
            tree.get(overflow).expect("存在").layout().bounds,
            Rect::new(150.0, 10.0, 30.0, 30.0)
        );
        assert_eq!(
            hit_node(&tree, 160.0, 25.0),
            Some(overflow),
            "无 clip 的父必须下钻，允许溢出子节点命中"
        );
    }

    #[test]
    fn clipped_parent_blocks_overflowing_child() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut parent_style = fixed(100.0, 100.0);
        parent_style.clip = Clip::Bounds;
        let parent = add(&mut tree, root, parent_style);
        let overflow = add(
            &mut tree,
            parent,
            StyleBuilder::new()
                .absolute()
                .left(150.0)
                .top(10.0)
                .size(30.0, 30.0)
                .build(),
        );
        layout(&mut tree);

        assert_eq!(
            tree.get(overflow).expect("存在").layout().bounds,
            Rect::new(150.0, 10.0, 30.0, 30.0),
            "布局盒照常存在（taffy 不裁剪）"
        );
        assert_eq!(
            hit_node(&tree, 160.0, 25.0),
            Some(root),
            "有 clip 的父把溢出子节点裁掉"
        );
    }

    #[test]
    fn subtree_bounds_cover_overflowing_descendants() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let overflow = add(
            &mut tree,
            parent,
            StyleBuilder::new()
                .absolute()
                .left(150.0)
                .top(10.0)
                .size(30.0, 30.0)
                .build(),
        );
        layout(&mut tree);

        let bounds = tree.subtree_bounds(parent);
        assert!(
            bounds.contains(Point::new(160.0, 25.0)),
            "子树包围盒必须覆盖父盒之外的溢出子节点：{bounds:?}"
        );
        assert!(
            bounds.contains(tree.get(parent).expect("存在").layout().bounds.origin),
            "也必须覆盖自身布局盒：{bounds:?}"
        );
        assert_eq!(
            tree.subtree_bounds(overflow),
            tree.get(overflow).expect("存在").layout().bounds,
            "叶子节点的子树包围盒就是它自己的布局盒"
        );
    }

    #[test]
    fn override_is_hittable_under_offset_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut root_style = fixed(200.0, 200.0);
        root_style.flex_direction(taffy::style::FlexDirection::Column);
        tree.set_style(root, root_style).expect("root 存活");
        let spacer = add(&mut tree, root, fixed(200.0, 40.0));
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 20.0, 30.0, 30.0))
            .build(&mut tree, pane)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(
            tree.get(canvas).expect("存在").layout().bounds,
            Rect::new(10.0, 60.0, 30.0, 30.0),
            "override 绝对坐标 = 树父 origin + rect（不是包含块）"
        );
        assert_eq!(
            hit_node(&tree, 15.0, 65.0),
            Some(canvas),
            "命中使用该绝对盒"
        );
        assert_eq!(
            hit_node(&tree, 15.0, 15.0),
            Some(spacer),
            "父盒之外命中 spacer"
        );
    }

    fn geometry_node(tree: &mut NodeTree, parent: NodeId, rect: Rect) -> NodeId {
        NodeBuilder::geometry(crate::paint::geometry::GeometryId::for_test(0, 1))
            .layout_override(rect)
            .build(tree, parent)
            .expect("几何节点挂载")
    }

    fn with_transform(tree: &mut NodeTree, node: NodeId, transform: crate::math::Transform) {
        tree.set_geometry_transform(node, transform, Rect::default())
            .expect("几何节点可设置变换");
    }

    #[test]
    fn rotated_geometry_hits_through_inverse_transform() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = geometry_node(&mut tree, root, Rect::new(100.0, 50.0, 40.0, 20.0));
        with_transform(
            &mut tree,
            node,
            crate::math::Transform::rotate(std::f32::consts::FRAC_PI_2),
        );
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(90.0, 60.0)).expect("旋转后可命中");
        assert_eq!(hit.node, node);
        assert!(
            (hit.local_point.x - 10.0).abs() < 1e-3 && (hit.local_point.y - 10.0).abs() < 1e-3,
            "local_point 必须回到**未变换**的内容坐标：{hit:?}"
        );

        assert_eq!(
            hit_node(&tree, 140.0, 60.0),
            Some(root),
            "旋转后的形状不再覆盖原布局盒的右侧"
        );
    }

    #[test]
    fn scaled_geometry_is_hittable_outside_its_layout_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = geometry_node(&mut tree, root, Rect::new(0.0, 0.0, 20.0, 10.0));
        with_transform(&mut tree, node, crate::math::Transform::scale(3.0, 3.0));
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 50.0, 25.0),
            Some(node),
            "缩放后的内容必须在视觉范围内可命中（子树剪枝用变换后的包络）"
        );
        let hit = hit_test(&tree, Point::new(50.0, 25.0)).expect("命中");
        assert!((hit.local_point.x - 50.0 / 3.0).abs() < 1e-3);
        assert_eq!(hit_node(&tree, 70.0, 25.0), Some(root));
    }

    #[test]
    fn non_invertible_transform_skips_node_and_is_counted() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = geometry_node(&mut tree, root, Rect::new(0.0, 0.0, 40.0, 40.0));
        with_transform(&mut tree, node, crate::math::Transform::scale(0.0, 1.0));
        layout(&mut tree);

        let before = tree.hit_test_skips();
        assert_eq!(
            hit_node(&tree, 20.0, 20.0),
            Some(root),
            "不可逆变换的节点不得被当作命中（跳过而不是 panic）"
        );
        assert!(
            tree.hit_test_skips() > before,
            "跳过必须记账（C11 的诊断面）"
        );
    }

    #[test]
    fn disabled_ancestor_makes_its_whole_subtree_unhittable() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = add(&mut tree, root, fixed(100.0, 100.0));
        let child = NodeBuilder::boxed()
            .style(fixed(50.0, 50.0))
            .on_click(crate::event::Action::custom("press"))
            .build(&mut tree, container)
            .expect("挂载成功");
        let active_container = add(&mut tree, root, fixed(100.0, 100.0));
        let active_child = NodeBuilder::boxed()
            .style(fixed(50.0, 50.0))
            .on_click(crate::event::Action::custom("press"))
            .build(&mut tree, active_container)
            .expect("挂载成功");
        tree.set_style_patch_for_test(
            container,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 10.0, 10.0),
            Some(root),
            "祖先禁用 ⇒ 子树不命中（旧实现会落到子节点上）"
        );
        assert!(
            !crate::interaction::focus::focusable(&tree, child),
            "焦点遍历必须与命中同源（禁用子树的节点不可聚焦）"
        );
        assert_eq!(
            hit_node(&tree, 80.0, 80.0),
            Some(root),
            "禁用容器自己的盒也不命中"
        );
        assert_eq!(
            hit_node(&tree, 110.0, 10.0),
            Some(active_child),
            "对照子树必须可命中（证明上面不是「没有被命中目标」的巧合）"
        );
        assert!(
            crate::interaction::focus::focusable(&tree, active_child),
            "对照子节点必须可聚焦（证明继承规则只影响禁用子树）"
        );
    }

    #[test]
    fn disabled_sibling_does_not_block_other_subtrees() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let disabled = add(&mut tree, root, fixed(100.0, 100.0));
        let sibling = add(&mut tree, root, fixed(100.0, 100.0));
        tree.set_style_patch_for_test(
            disabled,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(root), "禁用盒不命中");
        assert_eq!(hit_node(&tree, 150.0, 10.0), Some(sibling), "兄弟不受影响");
    }

    #[test]
    fn hit_focus_and_a11y_agree_on_the_interactive_flag() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let plain = add(&mut tree, root, fixed(10.0, 10.0));
        let flagged = add(&mut tree, root, fixed(10.0, 10.0));
        tree.set_style_patch_for_test(
            flagged,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        layout(&mut tree);

        let target = tree.get(flagged).expect("存在");
        let a11y_disabled = crate::a11y::mapping::is_disabled(target, None, None);
        assert!(a11y_disabled, "a11y 侧必须把它标成 disabled");
        assert!(
            !tree.is_effectively_interactive(flagged),
            "命中/焦点侧必须同样禁用"
        );
        assert_eq!(hit_node(&tree, 15.0, 5.0), Some(root), "命中侧跳过它");

        let plain_target = tree.get(plain).expect("存在");
        assert!(
            !crate::a11y::mapping::is_disabled(plain_target, None, None),
            "未标记的节点在 a11y 侧不是 disabled"
        );
        assert!(tree.is_effectively_interactive(plain));
    }

    #[test]
    fn non_finite_points_never_hit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _node = add(&mut tree, root, fixed(100.0, 100.0));
        layout(&mut tree);

        for point in [
            Point::new(f32::NAN, 10.0),
            Point::new(10.0, f32::NAN),
            Point::new(f32::INFINITY, 10.0),
            Point::new(10.0, f32::NEG_INFINITY),
        ] {
            let before = tree.hit_test_skips();
            assert_eq!(hit_test(&tree, point), None, "{point:?} 不得命中");
            assert!(tree.hit_test_skips() > before, "非有限点必须记账诊断");
        }
    }

    #[test]
    fn non_finite_layout_box_is_skipped_not_matched() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = add(&mut tree, root, fixed(100.0, 100.0));
        layout(&mut tree);
        tree.set_layout_for_test(
            node,
            crate::node::tree::LayoutBox {
                bounds: Rect::new(f32::NAN, f32::NAN, 10.0, 10.0),
                border: crate::math::Edges::ZERO,
            },
        );
        let before = tree.hit_test_skips();
        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(root));
        assert!(tree.hit_test_skips() > before, "非有限布局盒必须记账");
    }

    #[test]
    fn hit_shape_admits_points_outside_the_layout_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = add(&mut tree, root, {
            let mut style = fixed(20.0, 20.0);
            style.margin_sides(50.0, 0.0, 0.0, 50.0);
            style
        });
        let node = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 20.0, 20.0)))
            .style(fixed(20.0, 20.0))
            .hit_shape(HitShape::Rect(Rect::new(-20.0, -20.0, 60.0, 60.0)))
            .build(&mut tree, container)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(
            tree.subtree_bounds(node),
            Rect::new(30.0, 30.0, 60.0, 60.0),
            "子树包围盒必须并入命中形状 AABB（命中剪枝的唯一依据）"
        );
        assert_eq!(
            hit_node(&tree, 40.0, 40.0),
            Some(node),
            "布局盒外、形状内的点必须命中（旧实现只能缩小命中范围）"
        );
        assert_eq!(hit_node(&tree, 55.0, 55.0), Some(node), "布局盒内仍命中");
        assert_eq!(
            hit_node(&tree, 25.0, 25.0),
            Some(root),
            "形状外的点穿透到下层"
        );
    }

    #[test]
    fn hit_shape_envelope_uses_the_node_origin_not_the_subtree_union_origin() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut parent_style = fixed(20.0, 20.0);
        parent_style.margin_sides(50.0, 0.0, 0.0, 50.0);
        let parent = NodeBuilder::boxed()
            .style(parent_style)
            .hit_shape(HitShape::Rect(Rect::new(-20.0, -20.0, 60.0, 60.0)))
            .build(&mut tree, root)
            .expect("父节点挂载成功");
        let mut child_style = fixed(10.0, 10.0);
        child_style
            .position(taffy::style::Position::Absolute)
            .inset(
                taffy::style::LengthPercentageAuto::length(-30.0),
                taffy::style::LengthPercentageAuto::auto(),
                taffy::style::LengthPercentageAuto::auto(),
                taffy::style::LengthPercentageAuto::length(-30.0),
            );
        let child = NodeBuilder::boxed()
            .style(child_style)
            .build(&mut tree, parent)
            .expect("子节点挂载成功");
        layout(&mut tree);

        let expected = Rect::new(-30.0, -30.0, 120.0, 120.0);
        assert_eq!(
            tree.subtree_bounds(parent),
            expected,
            "包络 = 节点盒 ∪ 子盒 ∪ 形状绝对 AABB；旧实现按并集原点平移形状 ⇒ (-50,-50,120,120)"
        );
        let incremental = tree.subtree_bounds(parent);
        tree.recompute_subtree_bounds();
        assert_eq!(
            tree.subtree_bounds(parent),
            incremental,
            "增量包络必须与全量重算逐位一致"
        );
        assert_eq!(
            hit_node(&tree, 80.0, 80.0),
            Some(parent),
            "形状右下必须可命中"
        );
        assert_eq!(
            hit_node(&tree, 40.0, 40.0),
            Some(parent),
            "形状左上必须可命中"
        );

        tree.remove(child).expect("卸载成功");
        assert_eq!(
            tree.subtree_bounds(parent),
            Rect::new(30.0, 30.0, 60.0, 60.0),
            "结构变更后的局部收紧也必须用节点自身原点"
        );
        let after_remove = tree.subtree_bounds(parent);
        tree.recompute_subtree_bounds();
        assert_eq!(tree.subtree_bounds(parent), after_remove);
        assert_eq!(
            hit_node(&tree, 80.0, 80.0),
            Some(parent),
            "形状右下必须仍可命中"
        );
    }

    #[test]
    fn rounded_rect_hit_shape_follows_css_corners() {
        let shape = HitShape::RoundedRect {
            rect: Rect::new(0.0, 0.0, 100.0, 100.0),
            corner_radius: [20.0; 4],
        };

        assert!(shape.contains(Point::new(50.0, 50.0)), "中心命中");
        assert!(shape.contains(Point::new(10.0, 10.0)), "角框内但在圆弧内");
        assert!(
            !shape.contains(Point::new(2.0, 2.0)),
            "角框内、圆弧外（旧矩形命中会误判）"
        );
        assert!(!shape.contains(Point::new(-1.0, 50.0)), "盒外不命中");
        assert!(!shape.contains(Point::new(f32::NAN, 50.0)), "NaN 永不命中");

        let clamped = HitShape::RoundedRect {
            rect: Rect::new(0.0, 0.0, 40.0, 10.0),
            corner_radius: [40.0; 4],
        };
        assert!(clamped.contains(Point::new(20.0, 5.0)), "中心命中");
        assert!(!clamped.contains(Point::new(-1.0, 5.0)));
    }

    #[test]
    fn circle_ellipse_and_path_hit_shapes() {
        let circle = HitShape::Circle {
            center: Point::new(50.0, 40.0),
            radius: 30.0,
        };
        assert!(circle.contains(Point::new(50.0, 40.0)));
        assert!(circle.contains(Point::new(50.0, 69.5)));
        assert!(!circle.contains(Point::new(50.0, 70.5)));
        assert!(!circle.contains(Point::new(f32::INFINITY, 40.0)));

        let ellipse = HitShape::Ellipse {
            center: Point::new(50.0, 50.0),
            radius: crate::math::Size::new(40.0, 20.0),
        };
        assert!(ellipse.contains(Point::new(89.0, 50.0)));
        assert!(!ellipse.contains(Point::new(91.0, 50.0)));
        assert!(ellipse.contains(Point::new(50.0, 69.0)));
        assert!(!ellipse.contains(Point::new(50.0, 71.0)));

        let path = HitShape::path(vec![
            Point::new(0.0, 0.0),
            Point::new(40.0, 0.0),
            Point::new(40.0, 10.0),
            Point::new(10.0, 10.0),
            Point::new(10.0, 40.0),
            Point::new(0.0, 40.0),
        ]);
        assert!(path.contains(Point::new(5.0, 5.0)));
        assert!(path.contains(Point::new(5.0, 35.0)));
        assert!(path.contains(Point::new(35.0, 5.0)));
        assert!(!path.contains(Point::new(35.0, 35.0)), "凹口外");
        assert!(!path.contains(Point::new(-1.0, 5.0)));
        assert_eq!(
            path.bounds(),
            Some(Rect::new(0.0, 0.0, 40.0, 40.0)),
            "路径 AABB 在构造时算好（命中剪枝不重算顶点）"
        );
        assert!(
            !HitShape::path(Vec::new()).contains(Point::ZERO),
            "空路径永不命中"
        );
        assert!(
            !HitShape::path(vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)])
                .contains(Point::new(0.5, 0.5)),
            "少于 3 个点不是多边形"
        );
    }

    #[test]
    fn custom_hit_shape_still_prunes_to_the_layout_box() {
        let shape = HitShape::custom(|_| true);
        assert_eq!(shape.bounds(), None, "自定义谓词没有静态 AABB");
        assert!(shape.contains(Point::new(1000.0, 1000.0)));
        assert_eq!(shape, shape.clone(), "同一份 Arc 的 Custom 可比较");
        assert_ne!(
            shape,
            HitShape::custom(|_| true),
            "不同闭包实例不相等（无法结构比较）"
        );

        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed(10.0, 10.0))
            .hit_shape(HitShape::custom(|_| true))
            .build(&mut tree, root)
            .expect("挂载成功");
        layout(&mut tree);
        assert_eq!(hit_node(&tree, 5.0, 5.0), Some(node));
        assert_eq!(
            hit_node(&tree, 50.0, 50.0),
            Some(root),
            "Custom 不扩大布局盒外的命中范围"
        );
    }

    #[test]
    fn interactive_criterion_is_shared_by_hit_focus_and_a11y() {
        fn absolute_at(x: f32, y: f32, width: f32, height: f32) -> Style {
            StyleBuilder::new()
                .absolute()
                .left(x)
                .top(y)
                .size(width, height)
                .build()
        }

        let mut tree = NodeTree::new();
        let root = tree.root();
        let disabled = add(&mut tree, root, absolute_at(0.0, 0.0, 100.0, 100.0));
        tree.set_style_patch(
            disabled,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        let child = add(&mut tree, disabled, absolute_at(10.0, 10.0, 50.0, 50.0));
        let enabled = add(&mut tree, root, absolute_at(120.0, 0.0, 30.0, 30.0));
        layout(&mut tree);

        assert!(!tree.is_hittable(disabled));
        assert!(
            !tree.is_effectively_interactive(child),
            "祖先禁用 ⇒ 子树禁用"
        );
        assert_eq!(hit_node(&tree, 25.0, 25.0), Some(root), "禁用子树不命中");
        assert!(
            crate::a11y::mapping::a11y_disabled(&tree, child),
            "a11y 与命中/焦点取同一判据"
        );

        assert!(tree.is_hittable(enabled));
        assert!(tree.is_effectively_interactive(enabled));
        assert!(!crate::a11y::mapping::a11y_disabled(&tree, enabled));
        assert_eq!(
            hit_node(&tree, 125.0, 5.0),
            Some(enabled),
            "未禁用节点正常命中"
        );
    }
}
