//! The workspace's pane topology and the rectangles it lays out as.
//!
//! The topology is a binary tree: a leaf is one pane, a split is one orientation, the
//! head's share of the content (a fraction, not a measurement) and two children. The
//! stored layout carries the same fraction, so a restored workspace draws the split the
//! user left it at before anything has been measured.
//!
//! # Geometry
//!
//! The window is measured by the engine; this module turns a measured viewport into the
//! rectangles the stage hands each pane through `LayoutOverride`. That keeps the recursive
//! layout out of the engine's flow layout (which would need one nested flex tree per
//! split) and makes the result assertable without a window: [`PaneTree::layout`] is a pure
//! function of the viewport and the fractions, given the minimum pane widths.
//!
//! A split bar takes [`SPLIT_BAR_SIZE`] logical pixels between its children; the head's
//! share is computed inside the content that remains. A horizontal pair is clamped so both
//! panes keep their preferred width while the workspace allows it, then their compact
//! width, and only a workspace narrower than that lets a pane fall below it (the panes then
//! share what is there). Vertical splits have no minimum height: a pane may be short, but
//! the horizontal minimum is what keeps a widget's controls usable.
//!
//! # Identity
//!
//! A pane is identified by its [`PaneId`], not by its node: splitting, closing and
//! rearranging the tree never renumber a surviving pane, so the widget mounted in it (and
//! the state it has collected) survives a split unchanged. A split bar is identified by
//! its [`SplitId`] for the same reason.

use app_config::workspace::{WorkspaceLayout, WorkspaceOrientation, WorkspaceWidget};
use ui_core::math::Rect;

use crate::shell::metrics::{PANE_COMPACT_WIDTH, PANE_MIN_WIDTH, SPLIT_BAR_SIZE};

/// One pane's identity, stable across topology changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PaneId(u64);

impl PaneId {
    /// The numeric identity, for action keys and diagnostics.
    pub(crate) const fn index(self) -> u64 {
        self.0
    }

    /// A pane identity built by a test.
    #[cfg(test)]
    pub(crate) const fn test(raw: u64) -> Self {
        Self(raw)
    }
}

/// One split bar's identity, stable across topology changes of its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SplitId(u64);

impl SplitId {
    /// The numeric identity, for action keys and diagnostics.
    pub(crate) const fn index(self) -> u64 {
        self.0
    }
}

/// The orientation of a split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneOrientation {
    /// Children side by side, separated by a vertical bar.
    Horizontal,
    /// Children stacked, separated by a horizontal bar.
    Vertical,
}

impl From<WorkspaceOrientation> for PaneOrientation {
    fn from(orientation: WorkspaceOrientation) -> Self {
        match orientation {
            WorkspaceOrientation::Horizontal => Self::Horizontal,
            WorkspaceOrientation::Vertical => Self::Vertical,
        }
    }
}

impl From<PaneOrientation> for WorkspaceOrientation {
    fn from(orientation: PaneOrientation) -> Self {
        match orientation {
            PaneOrientation::Horizontal => Self::Horizontal,
            PaneOrientation::Vertical => Self::Vertical,
        }
    }
}

/// One node of the pane topology.
#[derive(Debug, Clone)]
pub(crate) enum PaneNode {
    /// One pane.
    Leaf { pane: PaneId },
    /// Two children and the head's share of the content between the split bar.
    Split {
        split: SplitId,
        orientation: PaneOrientation,
        head_fraction: f32,
        head: Box<PaneNode>,
        tail: Box<PaneNode>,
    },
}

impl PaneNode {
    /// The panes of this subtree, in drawing order.
    #[cfg(test)]
    fn visit_panes(&self, visit: &mut impl FnMut(PaneId)) {
        match self {
            Self::Leaf { pane } => visit(*pane),
            Self::Split { head, tail, .. } => {
                head.visit_panes(visit);
                tail.visit_panes(visit);
            }
        }
    }

    /// The minimum width this subtree needs to be usable with its preferred pane widths.
    fn preferred_width(&self) -> f32 {
        match self {
            Self::Leaf { .. } => PANE_MIN_WIDTH,
            Self::Split {
                orientation,
                head,
                tail,
                ..
            } => match orientation {
                PaneOrientation::Horizontal => {
                    head.preferred_width() + SPLIT_BAR_SIZE + tail.preferred_width()
                }
                PaneOrientation::Vertical => head.preferred_width().max(tail.preferred_width()),
            },
        }
    }

    /// The minimum width this subtree needs with its compact pane widths.
    fn compact_width(&self) -> f32 {
        match self {
            Self::Leaf { .. } => PANE_COMPACT_WIDTH,
            Self::Split {
                orientation,
                head,
                tail,
                ..
            } => match orientation {
                PaneOrientation::Horizontal => {
                    head.compact_width() + SPLIT_BAR_SIZE + tail.compact_width()
                }
                PaneOrientation::Vertical => head.compact_width().max(tail.compact_width()),
            },
        }
    }

    fn find_leaf_mut(&mut self, pane: PaneId) -> Option<&mut PaneNode> {
        match self {
            Self::Leaf { pane: leaf } if *leaf == pane => Some(self),
            Self::Split { head, tail, .. } => head
                .find_leaf_mut(pane)
                .or_else(|| tail.find_leaf_mut(pane)),
            Self::Leaf { .. } => None,
        }
    }
}

/// The pane topology of one workspace.
#[derive(Debug, Clone)]
pub(crate) struct PaneTree {
    root: PaneNode,
    next_pane: u64,
    next_split: u64,
}

/// The rectangles one measured viewport lays the workspace out as.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceGeometry {
    /// Each pane and the rectangle its chrome takes (window coordinates of the stage).
    pub panes: Vec<(PaneId, Rect)>,
    /// Each split bar and its rectangle, and the content extent the bar splits (logical
    /// pixels along the split's axis, excluding the bar) for a drag to be measured against.
    pub splits: Vec<(SplitId, Rect, f32)>,
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::single()
    }
}

impl PaneTree {
    /// A workspace with one pane showing nothing in particular.
    pub(crate) fn single() -> Self {
        Self {
            root: PaneNode::Leaf { pane: PaneId(0) },
            next_pane: 1,
            next_split: 0,
        }
    }

    /// Rebuilds a topology from a stored layout.
    ///
    /// Panes are numbered in pre-order, and the leaf widgets are returned in the same
    /// order so the stage mounts each one in its pane without walking the tree again.
    pub(crate) fn from_layout(layout: &WorkspaceLayout) -> (Self, Vec<(PaneId, WorkspaceWidget)>) {
        let mut tree = Self {
            root: PaneNode::Leaf { pane: PaneId(0) },
            next_pane: 0,
            next_split: 0,
        };
        let mut leaves = Vec::new();
        tree.root = tree.node_from_layout(layout, &mut leaves);

        (tree, leaves)
    }

    fn node_from_layout(
        &mut self,
        layout: &WorkspaceLayout,
        leaves: &mut Vec<(PaneId, WorkspaceWidget)>,
    ) -> PaneNode {
        match layout {
            WorkspaceLayout::Pane { widget } => {
                let pane = PaneId(self.next_pane);
                self.next_pane += 1;
                leaves.push((pane, widget.clone()));
                PaneNode::Leaf { pane }
            }
            WorkspaceLayout::Split {
                orientation,
                head_fraction,
                head,
                tail,
            } => {
                let head_node = self.node_from_layout(head, leaves);
                let tail_node = self.node_from_layout(tail, leaves);
                let split = SplitId(self.next_split);
                self.next_split += 1;
                PaneNode::Split {
                    split,
                    orientation: PaneOrientation::from(*orientation),
                    head_fraction: *head_fraction,
                    head: Box::new(head_node),
                    tail: Box::new(tail_node),
                }
            }
        }
    }

    /// Exports the topology as the stored layout, taking each pane's widget from `widget`.
    pub(crate) fn snapshot(
        &self,
        mut widget: impl FnMut(PaneId) -> WorkspaceWidget,
    ) -> WorkspaceLayout {
        fn snapshot_node(
            node: &PaneNode,
            widget: &mut impl FnMut(PaneId) -> WorkspaceWidget,
        ) -> WorkspaceLayout {
            match node {
                PaneNode::Leaf { pane } => WorkspaceLayout::Pane {
                    widget: widget(*pane),
                },
                PaneNode::Split {
                    orientation,
                    head_fraction,
                    head,
                    tail,
                    ..
                } => WorkspaceLayout::Split {
                    orientation: WorkspaceOrientation::from(*orientation),
                    head_fraction: *head_fraction,
                    head: Box::new(snapshot_node(head, widget)),
                    tail: Box::new(snapshot_node(tail, widget)),
                },
            }
        }

        snapshot_node(&self.root, &mut widget)
    }

    /// Every pane, in drawing order.
    #[cfg(test)]
    pub(crate) fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.root.visit_panes(&mut |pane| panes.push(pane));
        panes
    }

    /// Every split, in drawing order.
    pub(crate) fn splits(&self) -> Vec<SplitId> {
        fn visit(node: &PaneNode, splits: &mut Vec<SplitId>) {
            match node {
                PaneNode::Leaf { .. } => {}
                PaneNode::Split {
                    split, head, tail, ..
                } => {
                    splits.push(*split);
                    visit(head, splits);
                    visit(tail, splits);
                }
            }
        }
        let mut splits = Vec::new();
        visit(&self.root, &mut splits);
        splits
    }

    /// Whether this workspace is split (a pane in a split can be closed).
    pub(crate) fn is_split(&self) -> bool {
        matches!(&self.root, PaneNode::Split { .. })
    }

    /// Splits `target`, returning the split it created and the new pane it adds.
    ///
    /// The two panes of the new split are `target` and the new one; `target_on_head` keeps
    /// `target` on the head side, which is what "keep on the left / top" means.
    pub(crate) fn split_pane(
        &mut self,
        target: PaneId,
        orientation: PaneOrientation,
        target_on_head: bool,
    ) -> Option<(PaneId, SplitId)> {
        let split = SplitId(self.next_split);
        let new_pane = PaneId(self.next_pane);
        let node = self.root.find_leaf_mut(target)?;
        let leaf = std::mem::replace(
            node,
            PaneNode::Leaf {
                pane: PaneId(u64::MAX),
            },
        );
        let new_leaf = PaneNode::Leaf { pane: new_pane };
        let (head, tail) = if target_on_head {
            (Box::new(leaf), Box::new(new_leaf))
        } else {
            (Box::new(new_leaf), Box::new(leaf))
        };
        self.next_split += 1;
        self.next_pane += 1;
        *node = PaneNode::Split {
            split,
            orientation,
            head_fraction: 0.5,
            head,
            tail,
        };

        Some((new_pane, split))
    }

    /// Removes `pane` and promotes its sibling into the split's place.
    ///
    /// The last pane cannot be removed: a workspace always shows exactly one.
    pub(crate) fn remove_pane(&mut self, pane: PaneId) -> bool {
        if !self.is_split() {
            return false;
        }
        fn remove(node: &mut PaneNode, pane: PaneId) -> bool {
            let PaneNode::Split { head, tail, .. } = node else {
                return false;
            };
            let head_is_pane =
                matches!(head.as_ref(), PaneNode::Leaf { pane: leaf } if *leaf == pane);
            let tail_is_pane =
                matches!(tail.as_ref(), PaneNode::Leaf { pane: leaf } if *leaf == pane);
            if head_is_pane {
                let surviving = std::mem::replace(
                    tail.as_mut(),
                    PaneNode::Leaf {
                        pane: PaneId(u64::MAX),
                    },
                );
                *node = surviving;
                return true;
            }
            if tail_is_pane {
                let surviving = std::mem::replace(
                    head.as_mut(),
                    PaneNode::Leaf {
                        pane: PaneId(u64::MAX),
                    },
                );
                *node = surviving;
                return true;
            }
            remove(head, pane) || remove(tail, pane)
        }

        remove(&mut self.root, pane)
    }

    /// Sets one split's head fraction (clamped to a usable share).
    pub(crate) fn set_fraction(&mut self, split: SplitId, fraction: f32) -> bool {
        fn set(node: &mut PaneNode, split: SplitId, fraction: f32) -> bool {
            match node {
                PaneNode::Split {
                    split: id,
                    head_fraction,
                    ..
                } if *id == split => {
                    *head_fraction = fraction.clamp(0.0, 1.0);
                    true
                }
                PaneNode::Split { head, tail, .. } => {
                    set(head, split, fraction) || set(tail, split, fraction)
                }
                PaneNode::Leaf { .. } => false,
            }
        }

        set(&mut self.root, split, fraction)
    }

    /// The orientation of one split.
    pub(crate) fn split_orientation(&self, split: SplitId) -> Option<PaneOrientation> {
        fn get(node: &PaneNode, split: SplitId) -> Option<PaneOrientation> {
            match node {
                PaneNode::Split {
                    split: id,
                    orientation,
                    ..
                } if *id == split => Some(*orientation),
                PaneNode::Split { head, tail, .. } => get(head, split).or_else(|| get(tail, split)),
                PaneNode::Leaf { .. } => None,
            }
        }

        get(&self.root, split)
    }

    /// The head fraction of one split.
    pub(crate) fn fraction(&self, split: SplitId) -> Option<f32> {
        fn get(node: &PaneNode, split: SplitId) -> Option<f32> {
            match node {
                PaneNode::Split {
                    split: id,
                    head_fraction,
                    ..
                } if *id == split => Some(*head_fraction),
                PaneNode::Split { head, tail, .. } => get(head, split).or_else(|| get(tail, split)),
                PaneNode::Leaf { .. } => None,
            }
        }

        get(&self.root, split)
    }

    /// Lays the workspace out in `viewport`.
    ///
    /// The fractions are updated to the shares the layout actually produced, so a drag
    /// against a clamped split still persists a fraction the next frame can draw.
    pub(crate) fn layout(&mut self, viewport: Rect) -> WorkspaceGeometry {
        let mut geometry = WorkspaceGeometry::default();
        let viewport = Rect::new(
            viewport.origin.x.max(0.0),
            viewport.origin.y.max(0.0),
            viewport.size.width.max(0.0),
            viewport.size.height.max(0.0),
        );
        walk(&mut self.root, viewport, &mut geometry);
        geometry
    }
}

/// Lays one node out and writes the fraction each split produced back into the tree.
fn walk(node: &mut PaneNode, rect: Rect, geometry: &mut WorkspaceGeometry) {
    match node {
        PaneNode::Leaf { pane } => geometry.panes.push((*pane, rect)),
        PaneNode::Split {
            split,
            orientation,
            head_fraction,
            head,
            tail,
        } => {
            let (head_rect, bar_rect, tail_rect, extent, produced) =
                split_bounds(rect, *orientation, *head_fraction, head, tail);
            *head_fraction = produced;
            geometry.splits.push((*split, bar_rect, extent));
            walk(head, head_rect, geometry);
            walk(tail, tail_rect, geometry);
        }
    }
}

/// The rectangles of one split: head, bar, tail; the content extent the bar splits; and
/// the fraction the layout produced.
fn split_bounds(
    rect: Rect,
    orientation: PaneOrientation,
    fraction: f32,
    head: &PaneNode,
    tail: &PaneNode,
) -> (Rect, Rect, Rect, f32, f32) {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.5
    };
    match orientation {
        PaneOrientation::Horizontal => {
            let bar = SPLIT_BAR_SIZE.min(rect.size.width.max(0.0));
            let content = (rect.size.width - bar).max(0.0);
            let head_min = enforced_width(
                content,
                head.preferred_width(),
                tail.preferred_width(),
                head.compact_width(),
                tail.compact_width(),
            );
            let tail_min = enforced_width(
                content,
                tail.preferred_width(),
                head.preferred_width(),
                tail.compact_width(),
                head.compact_width(),
            );
            let head_width = (content * fraction).clamp(
                head_min.min(content),
                (content - tail_min).max(head_min).min(content),
            );
            let produced = if content > 0.0 {
                (head_width / content * 1000.0).round() / 1000.0
            } else {
                fraction
            };
            let head_rect = Rect::new(rect.origin.x, rect.origin.y, head_width, rect.size.height);
            let bar_rect = Rect::new(
                rect.origin.x + head_width,
                rect.origin.y,
                bar,
                rect.size.height,
            );
            let tail_rect = Rect::new(
                rect.origin.x + head_width + bar,
                rect.origin.y,
                (content - head_width).max(0.0),
                rect.size.height,
            );
            (head_rect, bar_rect, tail_rect, content, produced)
        }
        PaneOrientation::Vertical => {
            let bar = SPLIT_BAR_SIZE.min(rect.size.height.max(0.0));
            let content = (rect.size.height - bar).max(0.0);
            let head_height = content * fraction;
            let produced = if content > 0.0 {
                (head_height / content * 1000.0).round() / 1000.0
            } else {
                fraction
            };
            let head_rect = Rect::new(rect.origin.x, rect.origin.y, rect.size.width, head_height);
            let bar_rect = Rect::new(
                rect.origin.x,
                rect.origin.y + head_height,
                rect.size.width,
                bar,
            );
            let tail_rect = Rect::new(
                rect.origin.x,
                rect.origin.y + head_height + bar,
                rect.size.width,
                (content - head_height).max(0.0),
            );
            (head_rect, bar_rect, tail_rect, content, produced)
        }
    }
}

/// The minimum width one side of a horizontal split keeps, given what is available.
///
/// Preferred widths while the workspace allows them, compact widths while it allows those,
/// and nothing once it does not (the two panes then share what is there).
fn enforced_width(
    available: f32,
    preferred: f32,
    other_preferred: f32,
    compact: f32,
    other_compact: f32,
) -> f32 {
    if available >= preferred + other_preferred {
        preferred
    } else if available >= compact + other_compact {
        compact
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use app_config::workspace::WorkspaceWidget;
    use ui_core::math::Rect;

    use super::*;

    fn widget(id: &str) -> WorkspaceWidget {
        WorkspaceWidget {
            id: smol_str::SmolStr::new(id),
            config: None,
        }
    }

    fn single() -> WorkspaceLayout {
        WorkspaceLayout::Pane {
            widget: widget("kytrix.empty"),
        }
    }

    fn split(
        orientation: WorkspaceOrientation,
        head_fraction: f32,
        head: WorkspaceLayout,
        tail: WorkspaceLayout,
    ) -> WorkspaceLayout {
        WorkspaceLayout::Split {
            orientation,
            head_fraction,
            head: Box::new(head),
            tail: Box::new(tail),
        }
    }

    fn viewport(width: f32, height: f32) -> Rect {
        Rect::new(0.0, 0.0, width, height)
    }

    /// A restored single-pane workspace mounts one pane and draws it over the whole stage.
    #[test]
    fn a_single_pane_should_fill_the_stage() {
        let (mut tree, leaves) = PaneTree::from_layout(&single());
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1.id.as_str(), "kytrix.empty");

        let geometry = tree.layout(viewport(800.0, 600.0));
        assert_eq!(geometry.panes, vec![(leaves[0].0, viewport(800.0, 600.0))]);
        assert!(geometry.splits.is_empty());
        assert!(!tree.is_split());
    }

    /// A horizontal split puts a four-pixel bar between its two panes and gives the head
    /// the fraction of the content that remains.
    #[test]
    fn a_horizontal_split_should_put_the_bar_between_the_panes() {
        let layout = split(WorkspaceOrientation::Horizontal, 0.5, single(), single());
        let (mut tree, leaves) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(804.0, 400.0));

        let (head, tail) = (geometry.panes[0].1, geometry.panes[1].1);
        let (_, bar, content) = geometry.splits[0];
        assert_eq!(bar.size.width, SPLIT_BAR_SIZE);
        assert_eq!(content, 800.0);
        assert_eq!(head.size.width, 400.0);
        assert_eq!(tail.size.width, 400.0);
        assert_eq!(head.origin.x + head.size.width, bar.origin.x);
        assert_eq!(bar.origin.x + bar.size.width, tail.origin.x);
        assert_eq!(tail.origin.x + tail.size.width, 804.0);
        assert_eq!(leaves.len(), 2);
    }

    /// A vertical split stacks the panes and separates them with a horizontal bar.
    #[test]
    fn a_vertical_split_should_stack_the_panes() {
        let layout = split(WorkspaceOrientation::Vertical, 0.25, single(), single());
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(300.0, 204.0));

        let (head, tail) = (geometry.panes[0].1, geometry.panes[1].1);
        let (_, bar, content) = geometry.splits[0];
        assert_eq!(bar.size.height, SPLIT_BAR_SIZE);
        assert_eq!(content, 200.0);
        assert_eq!(head.size.height, 50.0);
        assert_eq!(tail.size.height, 150.0);
        assert_eq!(head.size.width, 300.0);
        assert_eq!(tail.origin.y + tail.size.height, 204.0);
    }

    /// A horizontal split never squeezes a pane below its preferred width while the
    /// workspace has room for both: the head stops at the minimum instead of following the
    /// fraction any further.
    #[test]
    fn a_horizontal_split_should_clamp_to_the_preferred_pane_width() {
        let layout = split(WorkspaceOrientation::Horizontal, 0.95, single(), single());
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(800.0, 400.0));

        let head = geometry.panes[0].1;
        let tail = geometry.panes[1].1;
        let content = 800.0 - SPLIT_BAR_SIZE;
        assert_eq!(head.size.width, content - PANE_MIN_WIDTH);
        assert_eq!(tail.size.width, PANE_MIN_WIDTH);
        // The fraction the layout produced is what a drag writes back.
        let split = geometry.splits[0].0;
        assert_eq!(
            tree.fraction(split),
            Some(((head.size.width / content) * 1000.0).round() / 1000.0)
        );
    }

    /// A workspace narrower than two compact panes still lays both out: the panes share the
    /// content rather than one of them going negative.
    #[test]
    fn a_narrow_horizontal_split_should_share_what_is_there() {
        let layout = split(WorkspaceOrientation::Horizontal, 0.5, single(), single());
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(124.0, 300.0));

        let head = geometry.panes[0].1;
        let tail = geometry.panes[1].1;
        assert_eq!(head.size.width, 60.0);
        assert_eq!(tail.size.width, 60.0);
        assert!(head.size.width > 0.0 && tail.size.width > 0.0);
    }

    /// A split inserts the new pane next to the target, keeps the target's identity, and
    /// numbers the new pane once.
    #[test]
    fn splitting_should_add_one_pane_and_keep_the_target() {
        let (mut tree, leaves) = PaneTree::from_layout(&single());
        let target = leaves[0].0;

        let (new_pane, _split) = tree
            .split_pane(target, PaneOrientation::Horizontal, true)
            .expect("a live pane splits");
        assert_ne!(new_pane, target);
        assert_eq!(tree.panes(), vec![target, new_pane]);
        assert!(tree.is_split());

        let (second, _) = tree
            .split_pane(new_pane, PaneOrientation::Vertical, true)
            .expect("the new pane splits too");
        assert_eq!(tree.panes(), vec![target, new_pane, second]);
    }

    /// Closing a pane promotes its sibling into the split's place, and the last pane of a
    /// workspace cannot be closed.
    #[test]
    fn closing_should_promote_the_sibling_and_keep_the_last_pane() {
        let (mut tree, leaves) = PaneTree::from_layout(&split(
            WorkspaceOrientation::Horizontal,
            0.5,
            single(),
            single(),
        ));
        let (head, tail) = (leaves[0].0, leaves[1].0);

        assert!(tree.remove_pane(head));
        assert_eq!(tree.panes(), vec![tail]);
        assert!(!tree.is_split());
        assert!(!tree.remove_pane(tail), "the last pane stays");
    }

    /// Closing a pane nested one split down promotes that split's other pane and leaves the
    /// resting topology intact.
    #[test]
    fn closing_a_nested_pane_should_keep_the_rest_of_the_tree() {
        let nested = split(WorkspaceOrientation::Horizontal, 0.5, single(), single());
        let layout = split(WorkspaceOrientation::Vertical, 0.5, single(), nested);
        let (mut tree, leaves) = PaneTree::from_layout(&layout);
        let (root_pane, nested_head, nested_tail) = (leaves[0].0, leaves[1].0, leaves[2].0);

        assert!(tree.remove_pane(nested_head));
        assert_eq!(tree.panes(), vec![root_pane, nested_tail]);
        assert!(tree.is_split());
    }

    /// A stored layout round-trips through the topology: the structure, the fractions and
    /// each pane's widget are what was stored.
    #[test]
    fn a_stored_layout_should_round_trip() {
        let layout = split(
            WorkspaceOrientation::Vertical,
            0.375,
            single(),
            split(WorkspaceOrientation::Horizontal, 0.625, single(), single()),
        );
        let (tree, leaves) = PaneTree::from_layout(&layout);
        let restored = tree.snapshot(|pane| {
            leaves
                .iter()
                .find(|(id, _)| *id == pane)
                .map(|(_, widget)| widget.clone())
                .expect("every pane has a widget")
        });

        assert_eq!(restored, layout);
    }

    /// Setting a fraction writes it into the tree and the next layout draws it, so a drag
    /// that stops mid-way persists the position the user left the bar at.
    #[test]
    fn setting_a_fraction_should_move_the_bar() {
        let (mut tree, _) = PaneTree::from_layout(&split(
            WorkspaceOrientation::Horizontal,
            0.5,
            single(),
            single(),
        ));
        let split = tree.layout(viewport(804.0, 400.0)).splits[0].0;

        assert!(tree.set_fraction(split, 0.25));
        let geometry = tree.layout(viewport(804.0, 400.0));
        assert_eq!(geometry.panes[0].1.size.width, 200.0);
        assert_eq!(geometry.panes[1].1.size.width, 600.0);
        assert_eq!(tree.fraction(split), Some(0.25));
    }

    /// A fraction that is not a number cannot reach the layout: the split draws at half.
    #[test]
    fn a_nonsense_fraction_should_draw_at_half() {
        let layout = split(
            WorkspaceOrientation::Horizontal,
            f32::NAN,
            single(),
            single(),
        );
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(804.0, 400.0));
        assert_eq!(geometry.panes[0].1.size.width, 400.0);
    }
}
