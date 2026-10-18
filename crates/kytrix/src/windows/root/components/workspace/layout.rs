//! Recursive pane layout of the workspace.
//!
//! The split topology is laid out as independent roots instead of one nested
//! Taffy tree. Measurements are collected during prepaint and written back into
//! the pane tree only when they changed.

use base_ui::style::theme::ThemeExt;
use gpui_kit::{
    AnyElement, App, Bounds, Element, Entity, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Style, Window, fill, point, px, relative, size,
};
use smallvec::SmallVec;

use super::{
    stage::Stage,
    tree::{
        DropTargetPosition, NodeId, PaneNode, PaneOrientation, PaneWidthConstraints, SPLIT_BAR_SIZE,
    },
};

const INLINE_STAGE_NODES: usize = 4;

/// The head child's share of the two children's combined size.
///
/// `None` until both children have been measured, so a restored fraction is never
/// replaced by a guess before the first frame has produced a real one.
pub(in crate::windows::root) fn measured_head_fraction(
    orientation: PaneOrientation,
    head: &PaneNode,
    tail: &PaneNode,
) -> Option<f32> {
    let (head_bounds, tail_bounds) = head.bounds().zip(tail.bounds())?;
    let (head_size, tail_size) = match orientation {
        PaneOrientation::Horizontal => (head_bounds.size.width, tail_bounds.size.width),
        PaneOrientation::Vertical => (head_bounds.size.height, tail_bounds.size.height),
    };
    let total_size = head_size + tail_size;
    if total_size <= Pixels::ZERO {
        return None;
    }

    Some(((head_size / total_size) * 1000.0).round() / 1000.0)
}

pub(in crate::windows::root) enum StageLayoutNode {
    Pending,
    Leaf {
        id: NodeId,
        previous_bounds: Option<Bounds<Pixels>>,
        pane: Option<AnyElement>,
    },
    Split {
        id: NodeId,
        previous_bounds: Option<Bounds<Pixels>>,
        orientation: PaneOrientation,
        head_fraction: f32,
        head_index: usize,
        tail_index: usize,
        head_width_constraints: PaneWidthConstraints,
        tail_width_constraints: PaneWidthConstraints,
        resize_bar: Option<AnyElement>,
    },
}

pub(in crate::windows::root) type StageLayoutNodes =
    SmallVec<[StageLayoutNode; INLINE_STAGE_NODES]>;

// PERF: Keep the recursive split topology out of Taffy and lay out each child as a root.
pub(in crate::windows::root) struct StageLayoutElement {
    pub id: gpui_kit::ElementId,
    pub stage: Entity<Stage>,
    pub nodes: StageLayoutNodes,
    pub root_index: usize,
    pub drop_target_preview: Option<(NodeId, DropTargetPosition)>,
}

type StageMeasurements = SmallVec<[(NodeId, Bounds<Pixels>); INLINE_STAGE_NODES]>;
type StageChildren = SmallVec<[AnyElement; INLINE_STAGE_NODES]>;

pub(in crate::windows::root) struct StageLayoutPrepaint {
    children: StageChildren,
    preview_bounds: Option<Bounds<Pixels>>,
}

struct StagePrepaintTraversal<'a> {
    measured_bounds: StageMeasurements,
    layout_changed: bool,
    children: StageChildren,
    window: &'a mut Window,
    cx: &'a mut App,
}

impl StagePrepaintTraversal<'_> {
    fn prepaint_child(&mut self, mut child: AnyElement, bounds: Bounds<Pixels>) {
        child.layout_as_root(bounds.size.into(), self.window, self.cx);
        child.prepaint_at(bounds.origin, self.window, self.cx);
        self.children.push(child);
    }

    fn prepaint_node(
        &mut self,
        nodes: &mut [StageLayoutNode],
        node_index: usize,
        bounds: Bounds<Pixels>,
    ) {
        let node = std::mem::replace(&mut nodes[node_index], StageLayoutNode::Pending);
        match node {
            StageLayoutNode::Pending => {
                log::error!("workspace layout node was consumed twice");
            }
            StageLayoutNode::Leaf {
                id,
                previous_bounds,
                pane,
            } => {
                self.layout_changed |= previous_bounds != Some(bounds);
                self.measured_bounds.push((id, bounds));
                let Some(pane) = pane else {
                    log::error!("workspace pane element missing during prepaint");
                    return;
                };
                self.prepaint_child(pane, bounds);
            }
            StageLayoutNode::Split {
                id,
                previous_bounds,
                orientation,
                head_fraction,
                head_index,
                tail_index,
                head_width_constraints,
                tail_width_constraints,
                resize_bar,
            } => {
                self.layout_changed |= previous_bounds != Some(bounds);
                self.measured_bounds.push((id, bounds));
                let (head_bounds, resize_bar_bounds, tail_bounds) = split_child_bounds(
                    bounds,
                    orientation,
                    head_fraction,
                    head_width_constraints,
                    tail_width_constraints,
                    self.window.scale_factor(),
                    |position| self.window.pixel_snap(position),
                );
                self.prepaint_node(nodes, head_index, head_bounds);
                let Some(resize_bar) = resize_bar else {
                    log::error!("workspace resize bar missing during prepaint");
                    return;
                };
                self.prepaint_child(resize_bar, resize_bar_bounds);
                self.prepaint_node(nodes, tail_index, tail_bounds);
            }
        }
    }
}

impl IntoElement for StageLayoutElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for StageLayoutElement {
    type RequestLayoutState = ();
    type PrepaintState = StageLayoutPrepaint;

    fn id(&self) -> Option<gpui_kit::ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: size(relative(1.0).into(), relative(1.0).into()),
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mut traversal = StagePrepaintTraversal {
            measured_bounds: StageMeasurements::with_capacity(self.nodes.len()),
            layout_changed: false,
            children: StageChildren::with_capacity(self.nodes.len()),
            window,
            cx,
        };
        traversal.prepaint_node(&mut self.nodes, self.root_index, bounds);
        let StagePrepaintTraversal {
            measured_bounds,
            layout_changed,
            children,
            ..
        } = traversal;

        if layout_changed {
            self.stage.update(cx, |stage, _| {
                stage.pane_tree_mut().update_layout_bounds(&measured_bounds);
            });
        }

        let preview_bounds = self.drop_target_preview.and_then(|(node_id, position)| {
            measured_bounds
                .iter()
                .find_map(|(id, bounds)| (*id == node_id).then_some(*bounds))
                .map(|bounds| calc_preview_bounds(&bounds, position))
        });

        StageLayoutPrepaint {
            children,
            preview_bounds,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for child in &mut prepaint.children {
            child.paint(window, cx);
        }
        if let Some(preview_bounds) = prepaint.preview_bounds {
            // The drop preview is the brand color at 40%: enough to name where the pane would
            // land, transparent enough that the pane under it still reads. The alpha is set on
            // the resolved HSLA the role already carries, so the frame converts nothing.
            let mut preview = cx.theme().roles.brand.base.hsla;
            preview.a = 0.4;
            window.paint_quad(fill(preview_bounds, preview));
        }
    }
}

fn split_child_bounds(
    bounds: Bounds<Pixels>,
    orientation: PaneOrientation,
    head_fraction: f32,
    head_width_constraints: PaneWidthConstraints,
    tail_width_constraints: PaneWidthConstraints,
    scale_factor: f32,
    snap: impl Fn(Pixels) -> Pixels,
) -> (Bounds<Pixels>, Bounds<Pixels>, Bounds<Pixels>) {
    let head_fraction = head_fraction.clamp(0.0, 1.0);
    match orientation {
        PaneOrientation::Horizontal => {
            let bar_width = if bounds.size.width < SPLIT_BAR_SIZE {
                bounds.size.width.max(Pixels::ZERO)
            } else {
                SPLIT_BAR_SIZE
            };
            let content_width = (bounds.size.width - bar_width).max(Pixels::ZERO);
            let (head_minimum_width, tail_minimum_width) = PaneWidthConstraints::enforced_pair(
                content_width,
                head_width_constraints,
                tail_width_constraints,
            );
            let desired_split_x = snap(bounds.origin.x + content_width * head_fraction);
            let raw_minimum_split_x = bounds.left() + head_minimum_width;
            let raw_maximum_split_x = bounds.left() + content_width - tail_minimum_width;
            let snapped_minimum_split_x = snap_up(raw_minimum_split_x, scale_factor);
            let snapped_maximum_split_x = snap_down(raw_maximum_split_x, scale_factor);
            let (minimum_split_x, maximum_split_x) = if snapped_minimum_split_x
                <= snapped_maximum_split_x
            {
                (snapped_minimum_split_x, snapped_maximum_split_x)
            } else {
                // An interval narrower than one device pixel cannot preserve
                // both logical minimums exactly. Use its nearest device pixel.
                let only_aligned_split =
                    snap(raw_minimum_split_x + (raw_maximum_split_x - raw_minimum_split_x) / 2.0);
                (only_aligned_split, only_aligned_split)
            };
            let split_x = desired_split_x
                .max(minimum_split_x)
                .min(maximum_split_x)
                .max(bounds.left())
                .min(bounds.right());
            let maximum_bar_end_x = snap_down(bounds.right() - tail_minimum_width, scale_factor)
                .max(split_x)
                .min(bounds.right());
            let bar_end_x = snap(split_x + bar_width)
                .max(split_x)
                .min(maximum_bar_end_x);
            let head_width = split_x - bounds.origin.x;
            let head = Bounds::new(bounds.origin, size(head_width, bounds.size.height));
            let bar = Bounds::new(
                point(split_x, bounds.origin.y),
                size(bar_end_x - split_x, bounds.size.height),
            );
            let tail = Bounds::new(
                point(bar_end_x, bounds.origin.y),
                size(bounds.right() - bar_end_x, bounds.size.height),
            );
            (head, bar, tail)
        }
        PaneOrientation::Vertical => {
            let bar_height = if bounds.size.height < SPLIT_BAR_SIZE {
                bounds.size.height.max(Pixels::ZERO)
            } else {
                SPLIT_BAR_SIZE
            };
            let content_height = (bounds.size.height - bar_height).max(Pixels::ZERO);
            let split_y = snap(bounds.origin.y + content_height * head_fraction)
                .max(bounds.top())
                .min(bounds.bottom());
            let bar_end_y = snap(split_y + bar_height).max(split_y).min(bounds.bottom());
            let head_height = split_y - bounds.origin.y;
            let head = Bounds::new(bounds.origin, size(bounds.size.width, head_height));
            let bar = Bounds::new(
                point(bounds.origin.x, split_y),
                size(bounds.size.width, bar_end_y - split_y),
            );
            let tail = Bounds::new(
                point(bounds.origin.x, bar_end_y),
                size(bounds.size.width, bounds.bottom() - bar_end_y),
            );
            (head, bar, tail)
        }
    }
}

fn snap_up(value: Pixels, scale_factor: f32) -> Pixels {
    px(((value / px(1.0)) * scale_factor).ceil() / scale_factor)
}

fn snap_down(value: Pixels, scale_factor: f32) -> Pixels {
    px(((value / px(1.0)) * scale_factor).floor() / scale_factor)
}

fn calc_preview_bounds(bounds: &Bounds<Pixels>, position: DropTargetPosition) -> Bounds<Pixels> {
    match position {
        DropTargetPosition::Left => Bounds::new(
            bounds.origin,
            size(bounds.size.width / 2.0, bounds.size.height),
        ),
        DropTargetPosition::Right => Bounds::new(
            point(bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y),
            size(bounds.size.width / 2.0, bounds.size.height),
        ),
        DropTargetPosition::Top => Bounds::new(
            point(bounds.origin.x, bounds.origin.y),
            size(bounds.size.width, bounds.size.height / 2.0),
        ),
        DropTargetPosition::Bottom => Bounds::new(
            point(bounds.origin.x, bounds.origin.y + bounds.size.height / 2.0),
            size(bounds.size.width, bounds.size.height / 2.0),
        ),
        DropTargetPosition::Center => bounds.to_owned(),
    }
}
