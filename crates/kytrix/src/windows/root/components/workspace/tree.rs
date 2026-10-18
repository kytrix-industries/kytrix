//! Pane split topology shared by the workspace layout, drag, and control modules.

use std::fmt::{Display, Formatter};

use app_config::workspace::{WorkspaceLayout, WorkspaceOrientation};
use gpui_kit::{App, Bounds, Context, Edges, ElementId, Pixels, Point, px};

use super::{layout::measured_head_fraction, pane::Pane, stage::Stage};

pub(in crate::windows::root) const SPLIT_BAR_SIZE: Pixels = px(4.0);
pub(in crate::windows::root) const PANE_COMPACT_MIN_WIDTH: Pixels = px(88.0);
pub(in crate::windows::root) const PANE_MIN_WIDTH: Pixels = px(120.0);
/// The share a fresh split gives its head child, before anything is measured.
const INITIAL_HEAD_FRACTION: f32 = 0.5;

// Preferred width keeps the linkage slot; compact width collapses it while
// preserving the fixed common actions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::windows::root) struct PaneWidthConstraints {
    preferred: Pixels,
    compact: Pixels,
}

impl PaneWidthConstraints {
    pub(in crate::windows::root) fn leaf() -> Self {
        Self {
            preferred: PANE_MIN_WIDTH,
            compact: PANE_COMPACT_MIN_WIDTH,
        }
    }

    pub(in crate::windows::root) fn combine(
        orientation: PaneOrientation,
        head: Self,
        tail: Self,
    ) -> PaneWidthConstraints {
        match orientation {
            PaneOrientation::Horizontal => Self {
                preferred: head.preferred + SPLIT_BAR_SIZE + tail.preferred,
                compact: head.compact + SPLIT_BAR_SIZE + tail.compact,
            },
            PaneOrientation::Vertical => Self {
                preferred: head.preferred.max(tail.preferred),
                compact: head.compact.max(tail.compact),
            },
        }
    }

    pub(in crate::windows::root) fn enforced_pair(
        available_width: Pixels,
        head: Self,
        tail: Self,
    ) -> (Pixels, Pixels) {
        if available_width >= head.preferred + tail.preferred {
            (head.preferred, tail.preferred)
        } else if available_width >= head.compact + tail.compact {
            (head.compact, tail.compact)
        } else {
            (Pixels::ZERO, Pixels::ZERO)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::windows::root) enum DropTargetPosition {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::windows::root) struct NodeId(pub u64);

impl Display for NodeId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug)]
pub(in crate::windows::root) enum PaneNode {
    Leaf {
        id: NodeId,
        pane: Box<Pane>,
        bounds: Option<Bounds<Pixels>>,
    },
    Split {
        id: NodeId,
        orientation: PaneOrientation,
        /// The head child's share of the content box, excluding the split bar.
        ///
        /// It is a property of the node rather than something derived while
        /// rendering, because a restored layout has no measurements yet: the
        /// stored fraction is what the first frame is drawn from.
        head_fraction: f32,
        head: Box<PaneNode>,
        tail: Box<PaneNode>,
        bounds: Option<Bounds<Pixels>>,
    },
}

impl PaneNode {
    fn leaf(id: NodeId, pane: Pane) -> Self {
        Self::Leaf {
            id,
            pane: Box::new(pane),
            bounds: None,
        }
    }

    fn id(&self) -> NodeId {
        match self {
            Self::Leaf { id, .. } | Self::Split { id, .. } => *id,
        }
    }

    pub(in crate::windows::root) fn bounds(&self) -> Option<&Bounds<Pixels>> {
        match self {
            Self::Leaf { bounds, .. } | Self::Split { bounds, .. } => bounds.as_ref(),
        }
    }

    fn bounds_mut(&mut self) -> &mut Option<Bounds<Pixels>> {
        match self {
            Self::Leaf { bounds, .. } | Self::Split { bounds, .. } => bounds,
        }
    }

    fn width_constraints(&self) -> PaneWidthConstraints {
        match self {
            Self::Leaf { .. } => PaneWidthConstraints::leaf(),
            Self::Split {
                orientation,
                head,
                tail,
                ..
            } => PaneWidthConstraints::combine(
                *orientation,
                head.width_constraints(),
                tail.width_constraints(),
            ),
        }
    }

    fn update_layout_bounds(
        &mut self,
        measurements: &mut impl Iterator<Item = (NodeId, Bounds<Pixels>)>,
    ) -> bool {
        let Some((measured_id, measured_bounds)) = measurements.next() else {
            return false;
        };
        debug_assert_eq!(self.id(), measured_id);
        if self.id() != measured_id {
            return false;
        }

        let changed = self.bounds().copied() != Some(measured_bounds);
        if changed {
            *self.bounds_mut() = Some(measured_bounds);
        }

        match self {
            Self::Split {
                orientation,
                head_fraction,
                head,
                tail,
                ..
            } => {
                let children_changed = head.update_layout_bounds(measurements)
                    | tail.update_layout_bounds(measurements);
                // The children are measured now, so the fraction the next frame
                // draws from is the one this frame actually produced.
                let fraction_changed = measured_head_fraction(*orientation, head, tail)
                    .is_some_and(|fraction| {
                        let changed = *head_fraction != fraction;
                        *head_fraction = fraction;
                        changed
                    });

                children_changed | fraction_changed | changed
            }
            Self::Leaf { .. } => changed,
        }
    }

    fn is_pane(&self, pane_id: &ElementId) -> bool {
        matches!(self, Self::Leaf { pane, .. } if pane.id == *pane_id)
    }

    fn find(&self, target_id: NodeId) -> Option<&PaneNode> {
        if self.id() == target_id {
            return Some(self);
        }

        match self {
            Self::Split { head, tail, .. } => head.find(target_id).or_else(|| tail.find(target_id)),
            Self::Leaf { .. } => None,
        }
    }

    fn find_mut(&mut self, target_id: NodeId) -> Option<&mut PaneNode> {
        if self.id() == target_id {
            return Some(self);
        }

        match self {
            Self::Split { head, tail, .. } => head
                .find_mut(target_id)
                .or_else(|| tail.find_mut(target_id)),
            Self::Leaf { .. } => None,
        }
    }

    pub(in crate::windows::root) fn find_pane(&self, pane_id: &ElementId) -> Option<&PaneNode> {
        match self {
            Self::Leaf { pane, .. } if pane.id == *pane_id => Some(self),
            Self::Split { head, tail, .. } => {
                head.find_pane(pane_id).or_else(|| tail.find_pane(pane_id))
            }
            Self::Leaf { .. } => None,
        }
    }

    pub(in crate::windows::root) fn find_pane_mut(
        &mut self,
        pane_id: &ElementId,
    ) -> Option<&mut PaneNode> {
        match self {
            Self::Leaf { pane, .. } if pane.id == *pane_id => Some(self),
            Self::Split { head, tail, .. } => head
                .find_pane_mut(pane_id)
                .or_else(|| tail.find_pane_mut(pane_id)),
            Self::Leaf { .. } => None,
        }
    }

    pub(in crate::windows::root) fn for_each_pane(&self, visitor: &mut impl FnMut(&Pane)) {
        match self {
            Self::Leaf { pane, .. } => visitor(pane),
            Self::Split { head, tail, .. } => {
                head.for_each_pane(visitor);
                tail.for_each_pane(visitor);
            }
        }
    }

    fn leaf_at(
        &self,
        position: Point<Pixels>,
        excluded_pane_id: &ElementId,
    ) -> Option<(NodeId, Bounds<Pixels>)> {
        match self {
            Self::Leaf {
                id,
                pane,
                bounds: Some(bounds),
            } if pane.id != *excluded_pane_id && bounds.contains(&position) => Some((*id, *bounds)),
            Self::Split { head, tail, .. } => head
                .leaf_at(position, excluded_pane_id)
                .or_else(|| tail.leaf_at(position, excluded_pane_id)),
            _ => None,
        }
    }

    fn split(
        &mut self,
        split_id: NodeId,
        new_leaf: PaneNode,
        orientation: PaneOrientation,
        target_on_head: bool,
    ) -> bool {
        let Self::Leaf { id, pane, bounds } = self else {
            return false;
        };

        let current_leaf = Self::leaf(*id, pane.as_ref().clone());
        let parent_bounds = *bounds;
        let (head, tail) = if target_on_head {
            (Box::new(current_leaf), Box::new(new_leaf))
        } else {
            (Box::new(new_leaf), Box::new(current_leaf))
        };

        *self = Self::Split {
            id: split_id,
            orientation,
            head_fraction: INITIAL_HEAD_FRACTION,
            head,
            tail,
            bounds: parent_bounds,
        };
        true
    }

    fn remove_pane(&mut self, pane_id: &ElementId) -> bool {
        let remove_head = match self {
            Self::Leaf { .. } => return false,
            Self::Split { head, .. } if head.is_pane(pane_id) => Some(true),
            Self::Split { tail, .. } if tail.is_pane(pane_id) => Some(false),
            Self::Split { .. } => None,
        };

        if let Some(remove_head) = remove_head {
            self.collapse_child(remove_head);
            return true;
        }

        match self {
            Self::Split { head, tail, .. } => {
                head.remove_pane(pane_id) || tail.remove_pane(pane_id)
            }
            Self::Leaf { .. } => false,
        }
    }

    /// Rebuilds a node from its persisted layout.
    ///
    /// `next_node_id` is advanced in preorder, so a restored tree numbers its
    /// nodes from zero exactly as a freshly built one does.
    fn from_layout(
        stage_id: &ElementId,
        layout: &WorkspaceLayout,
        next_node_id: &mut u64,
        cx: &mut Context<Stage>,
    ) -> Self {
        let id = NodeId(*next_node_id);
        *next_node_id += 1;

        match layout {
            WorkspaceLayout::Pane { widget } => {
                let pane = Pane::restored(stage_id, id, widget, cx.entity(), cx);
                Self::leaf(id, pane)
            }
            WorkspaceLayout::Split {
                orientation,
                head_fraction,
                head,
                tail,
            } => Self::Split {
                id,
                orientation: PaneOrientation::from(*orientation),
                head_fraction: *head_fraction,
                head: Box::new(Self::from_layout(stage_id, head, next_node_id, cx)),
                tail: Box::new(Self::from_layout(stage_id, tail, next_node_id, cx)),
                bounds: None,
            },
        }
    }

    /// Exports this node as its persisted layout.
    ///
    /// Measurements are deliberately left out: they belong to a window size, and
    /// the next window measures itself.
    fn snapshot(&self, cx: &App) -> WorkspaceLayout {
        match self {
            Self::Leaf { pane, .. } => WorkspaceLayout::Pane {
                widget: pane.snapshot(cx),
            },
            Self::Split {
                orientation,
                head_fraction,
                head,
                tail,
                ..
            } => WorkspaceLayout::Split {
                orientation: WorkspaceOrientation::from(*orientation),
                head_fraction: *head_fraction,
                head: Box::new(head.snapshot(cx)),
                tail: Box::new(tail.snapshot(cx)),
            },
        }
    }

    fn collapse_child(&mut self, remove_head: bool) {
        let removed_leaf = match &*self {
            Self::Split { head, .. } if remove_head => head.as_ref(),
            Self::Split { tail, .. } => tail.as_ref(),
            Self::Leaf { .. } => return,
        };
        let Self::Leaf { id, pane, bounds } = removed_leaf else {
            return;
        };

        // A temporary leaf lets us move the surviving Box out of the split without cloning it.
        let placeholder = Self::Leaf {
            id: *id,
            pane: pane.clone(),
            bounds: *bounds,
        };
        let Self::Split { head, tail, .. } = std::mem::replace(self, placeholder) else {
            return;
        };
        *self = if remove_head { *tail } else { *head };
    }
}

#[derive(Debug)]
pub(in crate::windows::root) struct PaneNodeTree {
    pub root: PaneNode,
    next_node_id: u64,
}

impl PaneNodeTree {
    /// Rebuilds a tree from a persisted layout.
    pub(in crate::windows::root) fn from_layout(
        stage_id: &ElementId,
        layout: &WorkspaceLayout,
        cx: &mut Context<Stage>,
    ) -> Self {
        let mut next_node_id = 0;

        Self {
            root: PaneNode::from_layout(stage_id, layout, &mut next_node_id, cx),
            next_node_id,
        }
    }

    /// Exports this tree as a persisted layout.
    pub(in crate::windows::root) fn snapshot(&self, cx: &App) -> WorkspaceLayout {
        self.root.snapshot(cx)
    }

    pub(in crate::windows::root) fn root(&self) -> &PaneNode {
        &self.root
    }

    /// Returns whether this stage holds more than one pane.
    ///
    /// A pane in a split can be closed; the last remaining one cannot, so it
    /// draws no close button.
    pub(in crate::windows::root) fn is_split(&self) -> bool {
        matches!(&self.root, PaneNode::Split { .. })
    }

    pub(in crate::windows::root) fn split_pane(
        &mut self,
        stage_id: &ElementId,
        pane_id: &ElementId,
        orientation: PaneOrientation,
        target_on_head: bool,
        cx: &mut Context<Stage>,
    ) -> bool {
        if self.root.find_pane(pane_id).is_none() {
            return false;
        }

        let new_leaf_id = self.next_node_id();
        let new_pane = Pane::new(stage_id, new_leaf_id, cx.entity(), cx);
        let new_leaf = PaneNode::leaf(new_leaf_id, new_pane);
        let split_id = self.next_node_id();

        self.root
            .find_pane_mut(pane_id)
            .is_some_and(|target| target.split(split_id, new_leaf, orientation, target_on_head))
    }

    pub(in crate::windows::root) fn remove_pane(&mut self, pane_id: &ElementId) -> bool {
        self.root.remove_pane(pane_id)
    }

    pub(in crate::windows::root) fn move_pane(
        &mut self,
        from_pane_id: &ElementId,
        to_pane_id: &ElementId,
        position: DropTargetPosition,
    ) -> bool {
        if from_pane_id == to_pane_id {
            return self.root.find_pane(from_pane_id).is_some();
        }

        let Some(PaneNode::Leaf {
            pane: moved_pane, ..
        }) = self.root.find_pane(from_pane_id)
        else {
            return false;
        };
        let moved_pane = moved_pane.as_ref().clone();
        if self.root.find_pane(to_pane_id).is_none() {
            return false;
        }

        let (orientation, target_on_head) = match position {
            DropTargetPosition::Left => (PaneOrientation::Horizontal, false),
            DropTargetPosition::Right => (PaneOrientation::Horizontal, true),
            DropTargetPosition::Top => (PaneOrientation::Vertical, false),
            DropTargetPosition::Bottom => (PaneOrientation::Vertical, true),
            DropTargetPosition::Center => return self.swap_panes(from_pane_id, to_pane_id),
        };

        if !self.root.remove_pane(from_pane_id) {
            return false;
        }

        let new_leaf_id = self.next_node_id();
        let new_leaf = PaneNode::leaf(new_leaf_id, moved_pane);
        let split_id = self.next_node_id();

        self.root
            .find_pane_mut(to_pane_id)
            .is_some_and(|target| target.split(split_id, new_leaf, orientation, target_on_head))
    }

    pub(in crate::windows::root) fn resize_split(
        &mut self,
        split_id: NodeId,
        drag_origin: Point<Pixels>,
        current_position: Point<Pixels>,
    ) -> bool {
        let Some(PaneNode::Split {
            orientation,
            head_fraction,
            head,
            tail,
            ..
        }) = self.root.find_mut(split_id)
        else {
            return false;
        };
        let (Some(mut head_bounds), Some(mut tail_bounds)) =
            (head.bounds().copied(), tail.bounds().copied())
        else {
            return false;
        };
        let head_width_constraints = head.width_constraints();
        let tail_width_constraints = tail.width_constraints();

        match *orientation {
            PaneOrientation::Horizontal => {
                let absolute_drag_start_position = head_bounds.top_right() + drag_origin;
                let mut delta = current_position.x - absolute_drag_start_position.x;
                let available_width = head_bounds.size.width + tail_bounds.size.width;
                let (head_minimum_width, tail_minimum_width) = PaneWidthConstraints::enforced_pair(
                    available_width,
                    head_width_constraints,
                    tail_width_constraints,
                );
                delta = delta
                    .max(head_minimum_width - head_bounds.size.width)
                    .min(tail_bounds.size.width - tail_minimum_width);
                head_bounds = head_bounds.extend(Edges {
                    right: delta,
                    ..Default::default()
                });
                tail_bounds = tail_bounds.extend(Edges {
                    left: -delta,
                    ..Default::default()
                });
            }
            PaneOrientation::Vertical => {
                let absolute_drag_start_position = head_bounds.bottom_left() + drag_origin;
                let delta = current_position.y - absolute_drag_start_position.y;
                head_bounds = head_bounds.extend(Edges {
                    bottom: delta,
                    ..Default::default()
                });
                tail_bounds = tail_bounds.extend(Edges {
                    top: -delta,
                    ..Default::default()
                });
            }
        }

        let changed = head.bounds().copied() != Some(head_bounds)
            || tail.bounds().copied() != Some(tail_bounds);
        if changed {
            *head.bounds_mut() = Some(head_bounds);
            *tail.bounds_mut() = Some(tail_bounds);
            // The dragged frames render from the node's own fraction, so it has
            // to move with the bounds rather than wait for the next measurement.
            if let Some(fraction) = measured_head_fraction(*orientation, head, tail) {
                *head_fraction = fraction;
            }
        }
        changed
    }

    pub(in crate::windows::root) fn pane_bounds(
        &self,
        pane_id: &ElementId,
    ) -> Option<Bounds<Pixels>> {
        self.root
            .find_pane(pane_id)
            .and_then(|node| node.bounds().copied())
    }

    pub(in crate::windows::root) fn pane_id_at_node(&self, node_id: NodeId) -> Option<&ElementId> {
        match self.root.find(node_id)? {
            PaneNode::Leaf { pane, .. } => Some(&pane.id),
            PaneNode::Split { .. } => None,
        }
    }

    pub(in crate::windows::root) fn drop_target_leaf_at(
        &self,
        mouse_position: Point<Pixels>,
        excluded_pane_id: &ElementId,
    ) -> Option<(NodeId, Bounds<Pixels>)> {
        self.root.leaf_at(mouse_position, excluded_pane_id)
    }

    pub(in crate::windows::root) fn update_layout_bounds(
        &mut self,
        measured_bounds: &[(NodeId, Bounds<Pixels>)],
    ) -> bool {
        let mut measured_bounds = measured_bounds.iter().copied();
        let changed = self.root.update_layout_bounds(&mut measured_bounds);
        debug_assert!(measured_bounds.next().is_none());
        changed
    }

    fn swap_panes(&mut self, pane_a_id: &ElementId, pane_b_id: &ElementId) -> bool {
        let Some(PaneNode::Leaf {
            id: node_a_id,
            pane: pane_a,
            ..
        }) = self.root.find_pane(pane_a_id)
        else {
            return false;
        };
        let (node_a_id, pane_a) = (*node_a_id, pane_a.clone());

        let Some(PaneNode::Leaf {
            id: node_b_id,
            pane: pane_b,
            ..
        }) = self.root.find_pane(pane_b_id)
        else {
            return false;
        };
        let (node_b_id, pane_b) = (*node_b_id, pane_b.clone());

        let Some(PaneNode::Leaf { pane, .. }) = self.root.find_mut(node_a_id) else {
            return false;
        };
        *pane = pane_b;

        let Some(PaneNode::Leaf { pane, .. }) = self.root.find_mut(node_b_id) else {
            return false;
        };
        *pane = pane_a;
        true
    }

    fn next_node_id(&mut self) -> NodeId {
        let id = NodeId(self.next_node_id);
        self.next_node_id += 1;
        id
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::windows::root) enum PaneOrientation {
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

#[cfg(test)]
mod tests {
    use app_config::{widget::VersionedWidgetConfig, workspace::WorkspaceWidget};
    use gpui_kit::{
        App, AppContext, Context, Empty, Entity, IntoElement, Render, TestAppContext, Window,
    };
    use smol_str::SmolStr;
    use widget::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetStatus},
        view::WidgetView,
    };
    use widget_registry::registry::{WidgetRegistration, WidgetRegistry};

    use super::*;

    /// A widget with no options, which is all a layout round trip needs.
    struct TestWidget;

    impl WidgetView for TestWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("kytrix.test.tree-widget");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }
    }

    impl Render for TestWidget {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    fn install(cx: &mut App) {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
        cx.global_mut::<WidgetRegistry>()
            .register(WidgetRegistration::for_view::<TestWidget>())
            .expect("the test widget should register");
    }

    fn pane(widget: WorkspaceWidget) -> WorkspaceLayout {
        WorkspaceLayout::Pane { widget }
    }

    fn widget(id: &str) -> WorkspaceWidget {
        WorkspaceWidget {
            id: SmolStr::new(id),
            config: None,
        }
    }

    /// The widget a pane falls back to, as a layout names it.
    fn default_widget() -> WorkspaceWidget {
        widget(app_config::workspace::DEFAULT_WIDGET_ID)
    }

    /// Two levels of split, asymmetric fractions on both, and both kinds of pane.
    fn nested_layout() -> WorkspaceLayout {
        WorkspaceLayout::Split {
            orientation: WorkspaceOrientation::Horizontal,
            head_fraction: 0.3,
            head: Box::new(pane(widget(TestWidget::ID.as_str()))),
            tail: Box::new(WorkspaceLayout::Split {
                orientation: WorkspaceOrientation::Vertical,
                head_fraction: 0.75,
                head: Box::new(pane(default_widget())),
                tail: Box::new(pane(widget(TestWidget::ID.as_str()))),
            }),
        }
    }

    fn restored_stage(layout: &WorkspaceLayout, cx: &mut TestAppContext) -> Entity<Stage> {
        let layout = layout.clone();
        cx.update(|cx| cx.new(|cx| Stage::from_layout(1, &layout, cx)))
    }

    fn layouts(stage: &Entity<Stage>, cx: &mut TestAppContext) -> WorkspaceLayout {
        cx.update(|cx| stage.read(cx).layout(cx))
    }

    fn pane_ids(stage: &Entity<Stage>, cx: &mut TestAppContext) -> Vec<ElementId> {
        cx.update(|cx| {
            let mut ids = Vec::new();
            stage
                .read(cx)
                .pane_tree()
                .root
                .for_each_pane(&mut |pane| ids.push(pane.id.clone()));
            ids
        })
    }

    fn shown_widget(
        stage: &Entity<Stage>,
        pane_id: &ElementId,
        cx: &mut TestAppContext,
    ) -> Option<WidgetId> {
        cx.update(|cx| stage.read(cx).pane_widget_id(pane_id, cx))
    }

    #[test]
    fn a_layout_should_survive_a_snapshot_and_restore() {
        let mut cx = TestAppContext::single();
        cx.update(install);
        let layout = nested_layout();

        let stage = restored_stage(&layout, &mut cx);

        assert_eq!(
            layouts(&stage, &mut cx),
            layout,
            "a restored layout must export exactly what was restored"
        );
        assert_eq!(pane_ids(&stage, &mut cx).len(), 3);
    }

    #[test]
    fn a_restored_split_should_keep_its_fraction() {
        let mut cx = TestAppContext::single();
        cx.update(install);
        let layout = WorkspaceLayout::Split {
            orientation: WorkspaceOrientation::Vertical,
            head_fraction: 0.2,
            head: Box::new(pane(default_widget())),
            tail: Box::new(pane(default_widget())),
        };

        let stage = restored_stage(&layout, &mut cx);

        let WorkspaceLayout::Split {
            orientation,
            head_fraction,
            ..
        } = layouts(&stage, &mut cx)
        else {
            panic!("a split must come back as a split");
        };
        assert_eq!(orientation, WorkspaceOrientation::Vertical);
        assert_eq!(
            head_fraction, 0.2,
            "a restored split draws the proportions the user left it at"
        );
    }

    /// A pane stores the config its widget reports, and a widget that persists no
    /// options reports none — whatever envelope its pane was restored from.
    #[test]
    fn a_restored_pane_should_show_the_widget_the_layout_names() {
        let mut cx = TestAppContext::single();
        cx.update(install);
        let envelope = VersionedWidgetConfig::encode(&()).expect("a unit config should encode");
        let layout = pane(WorkspaceWidget {
            id: SmolStr::new(TestWidget::ID.as_str()),
            config: Some(envelope),
        });

        let stage = restored_stage(&layout, &mut cx);
        let pane_id = pane_ids(&stage, &mut cx)
            .into_iter()
            .next()
            .expect("the layout names one pane");

        assert_eq!(
            shown_widget(&stage, &pane_id, &mut cx),
            Some(TestWidget::ID)
        );
        assert_eq!(
            layouts(&stage, &mut cx),
            pane(widget(TestWidget::ID.as_str())),
            "the pane comes back showing the widget, with no config to store"
        );
    }

    /// A widget this build no longer has is not a pane the application refuses to
    /// rebuild: the pane falls back to the widget a pane shows when nobody has
    /// chosen, and what it stores is that widget.
    #[test]
    fn an_unregistered_widget_should_fall_back_to_the_default_widget() {
        let mut cx = TestAppContext::single();
        cx.update(install);
        let layout = pane(widget("kytrix.test.missing"));

        let stage = restored_stage(&layout, &mut cx);
        let pane_id = pane_ids(&stage, &mut cx)
            .into_iter()
            .next()
            .expect("the layout names one pane");

        assert_eq!(
            shown_widget(&stage, &pane_id, &mut cx),
            Some(
                WidgetId::parse(app_config::workspace::DEFAULT_WIDGET_ID)
                    .expect("the default widget id is canonical")
            ),
            "one widget the application cannot rebuild is not a lost workspace"
        );
        assert_eq!(layouts(&stage, &mut cx), WorkspaceLayout::single_pane());
    }

    #[test]
    fn a_restored_tree_should_number_its_nodes_from_zero() {
        let mut cx = TestAppContext::single();
        cx.update(install);
        let layout = nested_layout();

        let stage = restored_stage(&layout, &mut cx);

        let node_ids = cx.update(|cx| {
            let pane_tree = stage.read(cx).pane_tree();
            let mut ids = Vec::new();
            fn collect(node: &PaneNode, ids: &mut Vec<NodeId>) {
                ids.push(node.id());
                if let PaneNode::Split { head, tail, .. } = node {
                    collect(head, ids);
                    collect(tail, ids);
                }
            }
            collect(&pane_tree.root, &mut ids);
            ids
        });

        assert_eq!(
            node_ids,
            [NodeId(0), NodeId(1), NodeId(2), NodeId(3), NodeId(4)]
        );
    }
}
