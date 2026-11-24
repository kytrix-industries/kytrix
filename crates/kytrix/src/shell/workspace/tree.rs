use app_config::workspace::{WorkspaceLayout, WorkspaceOrientation, WorkspaceWidget};
use ui_core::math::{LogicalPx, LogicalRect, px};

use crate::shell::metrics::{PANE_COMPACT_WIDTH, PANE_MIN_WIDTH, SPLIT_BAR_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PaneId(u64);

impl PaneId {
    pub(crate) const fn index(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn test(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SplitId(u64);

impl SplitId {
    pub(crate) const fn index(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneOrientation {
    Horizontal,
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

#[derive(Debug, Clone)]
pub(crate) enum PaneNode {
    Leaf {
        pane: PaneId,
    },
    Split {
        split: SplitId,
        orientation: PaneOrientation,
        head_fraction: f32,
        head: Box<PaneNode>,
        tail: Box<PaneNode>,
    },
}

impl PaneNode {
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

    fn preferred_width(&self) -> LogicalPx {
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

    fn compact_width(&self) -> LogicalPx {
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

#[derive(Debug, Clone)]
pub(crate) struct PaneTree {
    root: PaneNode,
    next_pane: u64,
    next_split: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceGeometry {
    pub panes: Vec<(PaneId, LogicalRect)>,
    pub splits: Vec<(SplitId, LogicalRect, LogicalPx)>,
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::single()
    }
}

impl PaneTree {
    pub(crate) fn single() -> Self {
        Self {
            root: PaneNode::Leaf { pane: PaneId(0) },
            next_pane: 1,
            next_split: 0,
        }
    }

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

    #[cfg(test)]
    pub(crate) fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.root.visit_panes(&mut |pane| panes.push(pane));
        panes
    }

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

    pub(crate) fn is_split(&self) -> bool {
        matches!(&self.root, PaneNode::Split { .. })
    }

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

    pub(crate) fn layout(&mut self, viewport: LogicalRect) -> WorkspaceGeometry {
        let mut geometry = WorkspaceGeometry::default();
        let viewport = LogicalRect::new(
            viewport.origin.x.max(px(0.0)),
            viewport.origin.y.max(px(0.0)),
            viewport.size.width.max(px(0.0)),
            viewport.size.height.max(px(0.0)),
        );
        walk(&mut self.root, viewport, &mut geometry);
        geometry
    }
}

fn walk(node: &mut PaneNode, rect: LogicalRect, geometry: &mut WorkspaceGeometry) {
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

fn split_bounds(
    rect: LogicalRect,
    orientation: PaneOrientation,
    fraction: f32,
    head: &PaneNode,
    tail: &PaneNode,
) -> (LogicalRect, LogicalRect, LogicalRect, LogicalPx, f32) {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.5
    };
    match orientation {
        PaneOrientation::Horizontal => {
            let width = rect.size.width.max(LogicalPx::ZERO);
            let bar = SPLIT_BAR_SIZE.min(width);
            let content = (width - bar).max(LogicalPx::ZERO);
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
            let produced = head_width / content;
            let head_rect =
                LogicalRect::new(rect.origin.x, rect.origin.y, head_width, rect.size.height);
            let bar_rect = LogicalRect::new(
                rect.origin.x + head_width,
                rect.origin.y,
                bar,
                rect.size.height,
            );
            let tail_rect = LogicalRect::new(
                rect.origin.x + head_width + bar,
                rect.origin.y,
                (content - head_width).max(LogicalPx::ZERO),
                rect.size.height,
            );
            let produced = if content > LogicalPx::ZERO {
                (produced * 1000.0).round() / 1000.0
            } else {
                fraction
            };
            (head_rect, bar_rect, tail_rect, content, produced)
        }
        PaneOrientation::Vertical => {
            let height = rect.size.height.max(LogicalPx::ZERO);
            let bar = SPLIT_BAR_SIZE.min(height);
            let content = (height - bar).max(LogicalPx::ZERO);
            let head_height = content * fraction;
            let head_rect =
                LogicalRect::new(rect.origin.x, rect.origin.y, rect.size.width, head_height);
            let bar_rect = LogicalRect::new(
                rect.origin.x,
                rect.origin.y + head_height,
                rect.size.width,
                bar,
            );
            let tail_rect = LogicalRect::new(
                rect.origin.x,
                rect.origin.y + head_height + bar,
                rect.size.width,
                (content - head_height).max(LogicalPx::ZERO),
            );
            let produced = if content > LogicalPx::ZERO {
                ((head_height / content) * 1000.0).round() / 1000.0
            } else {
                fraction
            };
            (head_rect, bar_rect, tail_rect, content, produced)
        }
    }
}

fn enforced_width(
    available: LogicalPx,
    preferred: LogicalPx,
    other_preferred: LogicalPx,
    compact: LogicalPx,
    other_compact: LogicalPx,
) -> LogicalPx {
    if available >= preferred + other_preferred {
        preferred
    } else if available >= compact + other_compact {
        compact
    } else {
        LogicalPx::ZERO
    }
}

#[cfg(test)]
mod tests {
    use app_config::workspace::WorkspaceWidget;

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

    fn viewport(width: f32, height: f32) -> LogicalRect {
        LogicalRect::new(px(0.0), px(0.0), px(width), px(height))
    }

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

    #[test]
    fn a_horizontal_split_should_put_the_bar_between_the_panes() {
        let layout = split(WorkspaceOrientation::Horizontal, 0.5, single(), single());
        let (mut tree, leaves) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(804.0, 400.0));

        let (head, tail) = (geometry.panes[0].1, geometry.panes[1].1);
        let (_, bar, content) = geometry.splits[0];
        assert_eq!(bar.size.width, SPLIT_BAR_SIZE);
        assert_eq!(content, px(800.0));
        assert_eq!(head.size.width, px(400.0));
        assert_eq!(tail.size.width, px(400.0));
        assert_eq!(head.origin.x + head.size.width, bar.origin.x);
        assert_eq!(bar.origin.x + bar.size.width, tail.origin.x);
        assert_eq!(tail.origin.x + tail.size.width, px(804.0));
        assert_eq!(leaves.len(), 2);
    }

    #[test]
    fn a_vertical_split_should_stack_the_panes() {
        let layout = split(WorkspaceOrientation::Vertical, 0.25, single(), single());
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(300.0, 204.0));

        let (head, tail) = (geometry.panes[0].1, geometry.panes[1].1);
        let (_, bar, content) = geometry.splits[0];
        assert_eq!(bar.size.height, SPLIT_BAR_SIZE);
        assert_eq!(content, px(200.0));
        assert_eq!(head.size.height, px(50.0));
        assert_eq!(tail.size.height, px(150.0));
        assert_eq!(head.size.width, px(300.0));
        assert_eq!(tail.origin.y + tail.size.height, px(204.0));
    }

    #[test]
    fn a_horizontal_split_should_clamp_to_the_preferred_pane_width() {
        let layout = split(WorkspaceOrientation::Horizontal, 0.95, single(), single());
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(800.0, 400.0));

        let head = geometry.panes[0].1;
        let tail = geometry.panes[1].1;
        let content = px(800.0) - SPLIT_BAR_SIZE;
        assert_eq!(head.size.width, content - PANE_MIN_WIDTH);
        assert_eq!(tail.size.width, PANE_MIN_WIDTH);
        let split = geometry.splits[0].0;
        assert_eq!(
            tree.fraction(split),
            Some(((head.size.width / content) * 1000.0).round() / 1000.0)
        );
    }

    #[test]
    fn a_narrow_horizontal_split_should_share_what_is_there() {
        let layout = split(WorkspaceOrientation::Horizontal, 0.5, single(), single());
        let (mut tree, _) = PaneTree::from_layout(&layout);
        let geometry = tree.layout(viewport(124.0, 300.0));

        let head = geometry.panes[0].1;
        let tail = geometry.panes[1].1;
        assert_eq!(head.size.width, px(60.0));
        assert_eq!(tail.size.width, px(60.0));
        assert!(head.size.width > px(0.0) && tail.size.width > px(0.0));
    }

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
        assert_eq!(geometry.panes[0].1.size.width, px(200.0));
        assert_eq!(geometry.panes[1].1.size.width, px(600.0));
        assert_eq!(tree.fraction(split), Some(0.25));
    }

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
        assert_eq!(geometry.panes[0].1.size.width, px(400.0));
    }
}
