//! The workspace: a tree of panes with drag-to-rearrange, split, resize, and
//! instrument linkage coordination.

use app_config::workspace::WorkspaceLayout;
use gpui_kit::{
    App, Context, ElementId, Empty, Entity, EventEmitter, IntoElement, Pixels, Point, Render,
    SharedString, Window,
};
use market_participants_model::market::selection::InstrumentSelection;
use widget::{identity::WidgetId, instrument_link::InstrumentLink};
use widget_registry::registry::WidgetRegistry;

use super::{
    controls::ResizeBar,
    drag::PaneDragSource,
    layout::{StageLayoutElement, StageLayoutNode, StageLayoutNodes},
    placeholder::InstrumentLinkMode,
    tree::{
        DropTargetPosition, NodeId, PaneNode, PaneNodeTree, PaneOrientation, PaneWidthConstraints,
    },
};

const INSTRUMENT_LINK_GROUPS: usize = 9;

/// How the stage changed in a way the stored workspace has to follow.
///
/// The layout is not the only thing mirrored into the settings: a widget's
/// persisted config lives in the same file, so a widget reporting that its config
/// changed is announced through this too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::windows::root) enum StageChange {
    /// A one-shot change whose result is final.
    Committed,
    /// A split bar is being dragged: persisting should wait for a pause.
    Adjusting,
}

pub struct Stage {
    id: ElementId,
    pane_tree: PaneNodeTree,
    drop_target_preview: Option<(NodeId, DropTargetPosition)>,
    /// The pane a drag is carrying, if any. Transient, like the drop preview: its slot keeps the
    /// pane's bounds but paints nothing while the drag preview is the only copy the user sees.
    dragging_pane: Option<ElementId>,
    /// The split whose bar the pointer is holding, if any. Transient, like the drop preview: it
    /// outlives neither the press nor the stage.
    held_split: Option<NodeId>,
    instrument_link_groups: [Option<InstrumentSelection>; INSTRUMENT_LINK_GROUPS],
}

impl EventEmitter<StageChange> for Stage {}

impl Stage {
    /// Rebuilds a stage from a persisted layout.
    ///
    /// This is the only way a stage is created, so an empty one is spelled
    /// [`WorkspaceLayout::single_pane`] rather than a second constructor.
    ///
    /// `id` must not be reused across stages in one process: GPUI keys state such
    /// as hover, drag, and scroll position by element id, so a new stage wearing a
    /// dead stage's id would inherit its transient state.
    pub(crate) fn from_layout(id: u64, layout: &WorkspaceLayout, cx: &mut Context<Self>) -> Self {
        let id =
            ElementId::NamedInteger(SharedString::new_static(std::any::type_name::<Self>()), id);

        Self {
            pane_tree: PaneNodeTree::from_layout(&id, layout, cx),
            id,
            drop_target_preview: None,
            dragging_pane: None,
            held_split: None,
            instrument_link_groups: Default::default(),
        }
    }

    /// Exports the layout this stage is showing.
    pub(crate) fn layout(&self, cx: &App) -> WorkspaceLayout {
        self.pane_tree.snapshot(cx)
    }

    /// Returns the pane topology for read-only queries.
    pub(in crate::windows::root) fn pane_tree(&self) -> &PaneNodeTree {
        &self.pane_tree
    }

    /// Returns the pane topology for structural changes.
    pub(in crate::windows::root) fn pane_tree_mut(&mut self) -> &mut PaneNodeTree {
        &mut self.pane_tree
    }

    /// Returns the drop indicator currently previewed during a pane drag.
    pub(in crate::windows::root) fn drop_target_preview(
        &self,
    ) -> Option<(NodeId, DropTargetPosition)> {
        self.drop_target_preview
    }

    /// Replaces the drop indicator previewed during a pane drag.
    pub(in crate::windows::root) fn set_drop_target_preview(
        &mut self,
        preview: Option<(NodeId, DropTargetPosition)>,
    ) {
        self.drop_target_preview = preview;
    }

    /// Marks `pane_id` as being dragged.
    ///
    /// The pane keeps its slot and its bounds — the rest of the layout does not move — but nothing
    /// is painted there: the drag preview under the cursor becomes the only copy of the pane the
    /// user sees. The mark is cleared when that preview is released, which is where every way a
    /// drag ends meets: the drop, the mouse up, and a drag cancelled without either all drop it.
    pub(in crate::windows::root) fn begin_pane_drag<Preview: 'static>(
        &mut self,
        pane_id: &ElementId,
        preview: &Entity<Preview>,
        cx: &mut Context<Self>,
    ) {
        if self.dragging_pane.as_ref() == Some(pane_id) {
            return;
        }

        self.dragging_pane = Some(pane_id.clone());
        cx.observe_release(preview, {
            let pane_id = pane_id.clone();
            move |stage, _, cx| {
                if stage.dragging_pane.as_ref() == Some(&pane_id) {
                    stage.dragging_pane = None;
                    cx.notify();
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Returns the split whose bar the pointer is holding, if any.
    pub(in crate::windows::root) fn held_split(&self) -> Option<NodeId> {
        self.held_split
    }

    /// Latches the split bar the pointer went down on.
    ///
    /// The bar paints its held state from this rather than from GPUI's active state, because that
    /// state is the click state and GPUI clears it as soon as the press turns into the drag the
    /// bar starts, which is the whole gesture.
    pub(in crate::windows::root) fn hold_split(
        &mut self,
        split_id: NodeId,
        cx: &mut Context<Self>,
    ) {
        if self.held_split != Some(split_id) {
            self.held_split = Some(split_id);
            cx.notify();
        }
    }

    /// Releases the held split bar, if the pointer is still holding this one.
    pub(in crate::windows::root) fn release_split(
        &mut self,
        split_id: NodeId,
        cx: &mut Context<Self>,
    ) {
        if self.held_split == Some(split_id) {
            self.held_split = None;
            cx.notify();
        }
    }

    fn append_layout_node(
        &self,
        node: &PaneNode,
        close_button_visible: bool,
        nodes: &mut StageLayoutNodes,
        cx: &mut Context<Self>,
    ) -> (usize, PaneWidthConstraints) {
        let node_index = nodes.len();
        nodes.push(StageLayoutNode::Pending);

        let width_constraints = match node {
            PaneNode::Leaf { id, pane, bounds } => {
                // A dragged pane keeps its slot and its bounds, but the preview under the cursor
                // is the only copy drawn: the slot gets an empty element, so the widget and the
                // header are not painted twice.
                let pane = if self.dragging_pane.as_ref() == Some(&pane.id) {
                    Empty.into_any_element()
                } else {
                    let mut pane = pane.as_ref().clone();
                    pane.close_button_visible = close_button_visible;
                    pane.into_any_element()
                };
                nodes[node_index] = StageLayoutNode::Leaf {
                    id: *id,
                    previous_bounds: *bounds,
                    pane: Some(pane),
                };
                PaneWidthConstraints::leaf()
            }
            PaneNode::Split {
                id,
                orientation,
                head_fraction,
                head,
                tail,
                bounds,
                ..
            } => {
                let (head_index, head_width_constraints) =
                    self.append_layout_node(head, close_button_visible, nodes, cx);
                let (tail_index, tail_width_constraints) =
                    self.append_layout_node(tail, close_button_visible, nodes, cx);
                nodes[node_index] = StageLayoutNode::Split {
                    id: *id,
                    previous_bounds: *bounds,
                    orientation: *orientation,
                    head_fraction: *head_fraction,
                    head_index,
                    tail_index,
                    head_width_constraints,
                    tail_width_constraints,
                    resize_bar: Some(
                        ResizeBar::new(self.id.clone(), cx.entity(), *orientation, *id)
                            .into_any_element(),
                    ),
                };
                PaneWidthConstraints::combine(
                    *orientation,
                    head_width_constraints,
                    tail_width_constraints,
                )
            }
        };

        (node_index, width_constraints)
    }

    pub(in crate::windows::root) fn split_pane(
        &mut self,
        pane_id: &ElementId,
        orientation: PaneOrientation,
        target_on_head: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .pane_tree
            .split_pane(&self.id, pane_id, orientation, target_on_head, cx)
        {
            cx.emit(StageChange::Committed);
            cx.notify();
        } else {
            log::error!("split pane failed: pane not found");
        }
    }

    pub(in crate::windows::root) fn close_pane(
        &mut self,
        pane_id: &ElementId,
        cx: &mut Context<Self>,
    ) {
        if self.pane_tree.remove_pane(pane_id) {
            self.clear_empty_instrument_link_groups();
            cx.emit(StageChange::Committed);
            cx.notify();
        } else {
            log::error!("remove pane failed: pane not found or is the only pane");
        }
    }

    pub(in crate::windows::root) fn set_widget(
        &mut self,
        pane_id: &ElementId,
        widget_id: &str,
        cx: &mut Context<Self>,
    ) {
        let showing = match self.pane_tree.root.find_pane(pane_id) {
            Some(PaneNode::Leaf { pane, .. }) => pane.widget_id(cx).as_str() == widget_id,
            _ => {
                log::error!("set widget failed: pane not found");
                return;
            }
        };
        // Confirming the widget the pane is already showing changes nothing: the pane
        // keeps the instance it has, with the data and the state it has collected, and
        // an accidental confirm is therefore not a pane that starts over.
        if showing {
            return;
        }

        let stage = cx.entity().downgrade();
        let config_change = super::pane::config_change(pane_id, &stage);
        let widget = match WidgetRegistry::create_global(widget_id, None, config_change, cx) {
            Ok(widget) => widget,
            Err(error) => {
                log::error!("failed to create widget `{widget_id}`: {error}");
                return;
            }
        };
        self.set_instrument_link_mode(pane_id, InstrumentLinkMode::Unlinked, cx);
        let Some(PaneNode::Leaf { pane, .. }) = self.pane_tree.root.find_pane_mut(pane_id) else {
            log::error!("set widget failed: pane not found");
            return;
        };
        pane.set_widget(widget, cx);
        cx.emit(StageChange::Committed);
        cx.notify();
    }

    /// The widget type the pane is showing; `None` when there is no such pane.
    pub(in crate::windows::root) fn pane_widget_id(
        &self,
        pane_id: &ElementId,
        cx: &App,
    ) -> Option<WidgetId> {
        let Some(PaneNode::Leaf { pane, .. }) = self.pane_tree.root.find_pane(pane_id) else {
            return None;
        };

        Some(pane.widget_id(cx).clone())
    }

    /// Announces that the widget in `pane_id` changed the config it persists.
    ///
    /// The pane is only checked for, never read: the widget's config is pulled
    /// from the widget when the workspace is snapshotted. A pane that is gone has
    /// nothing to write back, and a pane that has since been given another widget
    /// writes back the one it now shows — which is the config that belongs in the
    /// settings either way.
    pub(in crate::windows::root) fn widget_config_changed(
        &mut self,
        pane_id: &ElementId,
        cx: &mut Context<Self>,
    ) {
        if self.pane_tree.root.find_pane(pane_id).is_none() {
            log::error!("widget config changed in a pane that no longer exists");

            return;
        }

        cx.emit(StageChange::Committed);
    }

    /// Whether rebuilding this stage replaced a widget config it could not read.
    ///
    /// Such a config is repaired in memory as the stage is built, so the caller
    /// has to write the workspaces out again for the repair to outlive this run.
    pub(in crate::windows::root) fn has_repaired_widget_config(&self) -> bool {
        let mut repaired = false;
        self.pane_tree.root.for_each_pane(&mut |pane| {
            repaired |= pane.widget_config_repaired();
        });

        repaired
    }

    pub(in crate::windows::root) fn set_instrument_link_mode(
        &mut self,
        pane_id: &ElementId,
        mode: InstrumentLinkMode,
        cx: &mut Context<Self>,
    ) {
        if matches!(mode, InstrumentLinkMode::Group(_)) && mode.group_index().is_none() {
            return;
        }
        let previous = {
            let Some(PaneNode::Leaf { pane, .. }) = self.pane_tree.root.find_pane_mut(pane_id)
            else {
                return;
            };
            if pane.instrument_link.is_none() || pane.instrument_link_mode == mode {
                return;
            }
            let previous = pane.instrument_link_mode;
            pane.set_instrument_link_mode(mode, cx);
            previous
        };

        if let Some(index) = previous.group_index()
            && !self.has_instrument_link_group_member(index)
        {
            self.instrument_link_groups[index] = None;
        }

        let Some(index) = mode.group_index() else {
            return;
        };
        if let Some(selection) = self.instrument_link_groups[index].clone() {
            if let Some(link) = self.pane_instrument_link(pane_id) {
                link.apply_remote(selection, cx);
            }
        } else if let Some(link) = self.pane_instrument_link(pane_id) {
            self.instrument_link_groups[index] = Some(link.selection());
        }
    }

    pub(in crate::windows::root) fn instrument_selection_changed(
        &mut self,
        pane_id: &ElementId,
        selection: InstrumentSelection,
        cx: &mut Context<Self>,
    ) {
        let Some(InstrumentLinkMode::Group(group)) = self.pane_instrument_link_mode(pane_id) else {
            return;
        };
        let index = usize::from(group - 1);
        self.instrument_link_groups[index] = Some(selection.clone());
        let links = self.instrument_links_in_mode(InstrumentLinkMode::Group(group), Some(pane_id));
        for link in links {
            link.apply_remote(selection.clone(), cx);
        }
    }

    pub(in crate::windows::root) fn pane_clicked(
        &mut self,
        pane_id: &ElementId,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self
            .pane_instrument_link(pane_id)
            .map(|link| link.selection())
        else {
            return;
        };
        let links = self.instrument_links_in_mode(InstrumentLinkMode::FollowMouse, Some(pane_id));
        for link in links {
            link.apply_remote(selection.clone(), cx);
        }
    }

    fn pane_instrument_link(&self, pane_id: &ElementId) -> Option<InstrumentLink> {
        let PaneNode::Leaf { pane, .. } = self.pane_tree.root.find_pane(pane_id)? else {
            return None;
        };
        pane.instrument_link.clone()
    }

    fn pane_instrument_link_mode(&self, pane_id: &ElementId) -> Option<InstrumentLinkMode> {
        let PaneNode::Leaf { pane, .. } = self.pane_tree.root.find_pane(pane_id)? else {
            return None;
        };
        Some(pane.instrument_link_mode)
    }

    fn instrument_links_in_mode(
        &self,
        mode: InstrumentLinkMode,
        excluded_pane_id: Option<&ElementId>,
    ) -> Vec<InstrumentLink> {
        let mut links = Vec::with_capacity(INSTRUMENT_LINK_GROUPS);
        self.pane_tree.root.for_each_pane(&mut |pane| {
            if pane.instrument_link_mode == mode
                && excluded_pane_id.is_none_or(|excluded| excluded != &pane.id)
                && let Some(link) = pane.instrument_link.clone()
            {
                links.push(link);
            }
        });
        links
    }

    fn has_instrument_link_group_member(&self, index: usize) -> bool {
        let mode = InstrumentLinkMode::Group(index as u8 + 1);
        let mut found = false;
        self.pane_tree.root.for_each_pane(&mut |pane| {
            found |= pane.instrument_link_mode == mode;
        });
        found
    }

    fn clear_empty_instrument_link_groups(&mut self) {
        for index in 0..INSTRUMENT_LINK_GROUPS {
            if !self.has_instrument_link_group_member(index) {
                self.instrument_link_groups[index] = None;
            }
        }
    }

    pub(in crate::windows::root) fn resize_split(
        &mut self,
        split_id: NodeId,
        drag_origin: Point<Pixels>,
        current_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self
            .pane_tree
            .resize_split(split_id, drag_origin, current_position)
        {
            cx.emit(StageChange::Adjusting);
            cx.notify();
        }
    }

    pub(in crate::windows::root) fn commit_drop(
        &mut self,
        dragged: &PaneDragSource,
        cx: &mut Context<Self>,
    ) {
        let Some((target_node_id, position)) = self.drop_target_preview.take() else {
            return;
        };

        let Some(target_pane_id) = self.pane_tree.pane_id_at_node(target_node_id).cloned() else {
            log::error!("drop target node {} is not a pane", target_node_id);
            cx.notify();
            return;
        };

        if !self
            .pane_tree
            .move_pane(&dragged.pane_id, &target_pane_id, position)
        {
            log::error!("drag and drop to node {} failed", target_node_id);
        }
        cx.emit(StageChange::Committed);
        cx.notify();
    }
}

impl Render for Stage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut nodes = StageLayoutNodes::new();
        // Whether a pane can be closed follows from the topology, so it is read
        // here rather than stored on every pane and kept in step by hand.
        let close_button_visible = self.pane_tree.is_split();
        let (root_index, _) =
            self.append_layout_node(self.pane_tree.root(), close_button_visible, &mut nodes, cx);
        StageLayoutElement {
            id: self.id.clone(),
            stage: cx.entity(),
            nodes,
            root_index,
            drop_target_preview: self.drop_target_preview,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::{widget::WidgetConfig, workspace::WorkspaceLayout};
    use gpui_kit::{
        AnyView, App, AppContext, Bounds, Context, ElementId, Empty, Entity, IntoElement,
        Modifiers, MouseButton, ParentElement, Pixels, Render, Styled, TestAppContext,
        VisualTestContext, Window, div, point, px, size,
    };
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{
            route::{VenueListingKey, VenueRoute},
            venue::VenueListingId,
        },
        market::selection::InstrumentSelection,
    };
    use widget::{
        any::AnyWidget,
        category::WidgetCategory,
        config::WidgetConfigChange,
        descriptor::WidgetDescriptor,
        identity::{WidgetId, WidgetInstanceId},
        instrument_link::InstrumentLink,
        status::{WidgetAction, WidgetStatus},
        surface::WidgetSurface,
        view::WidgetView,
    };
    use widget_registry::registry::{WidgetRegistration, WidgetRegistry};

    use crate::windows::root::components::workspace::{
        Stage,
        placeholder::InstrumentLinkMode,
        tree::{PaneNode, PaneOrientation},
    };

    /// A widget that only exists to carry an instrument link.
    struct TestWidget {
        link: Option<InstrumentLink>,
    }

    impl WidgetView for TestWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("kytrix.test.instrument-link");
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
            Self { link: None }
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn instrument_link(&self) -> Option<InstrumentLink> {
            self.link.clone()
        }
    }

    impl Render for TestWidget {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    /// A listing of one repeated hex digit, which is a canonical identity value.
    fn listing_id(digit: char) -> VenueListingId {
        let value = format!("listing-id/v1/{}", digit.to_string().repeat(64));
        VenueListingId::from_identity_value(&value).expect("the fixture listing id is canonical")
    }

    /// A selection is the listing key and nothing else: the fixture varies the
    /// listing id, so a test can tell one listing of a route from another.
    fn selection(digit: char) -> InstrumentSelection {
        InstrumentSelection {
            listing: VenueListingKey::new(
                VenueRoute::new(
                    VenueId::Binance,
                    VenueScopeId::new("spot").expect("the fixture scope is canonical"),
                ),
                listing_id(digit),
            ),
        }
    }

    fn link(digit: char, accepts: bool, applications: Rc<Cell<usize>>) -> InstrumentLink {
        InstrumentLink::new(selection(digit), move |_, _| {
            applications.set(applications.get() + 1);
            accepts
        })
    }

    fn widget(link: InstrumentLink, cx: &mut App) -> Entity<AnyWidget> {
        let status: Entity<WidgetStatus> = cx.new(|_| WidgetStatus::new());
        let body = cx.new({
            let link = link.clone();
            let status = status.clone();
            move |cx| {
                let mut widget = TestWidget::new(
                    WidgetInstanceId::new(),
                    (),
                    status,
                    |_| {},
                    WidgetConfigChange::default(),
                    WidgetAction::new("common.retry", |_| {}),
                    cx,
                );
                widget.link = Some(link);
                widget
            }
        });
        let surface = cx.new({
            let body: AnyView = body.clone().into();
            move |cx| {
                WidgetSurface::new(
                    status,
                    body,
                    widget::any::has_content_of::<TestWidget>(),
                    cx,
                )
            }
        });
        let build_header = widget::any::build_header_of(body.downgrade());

        cx.new(move |_| {
            AnyWidget::new(
                WidgetDescriptor::new(
                    TestWidget::ID,
                    TestWidget::DISPLAY_NAME,
                    TestWidget::CATEGORY,
                    <() as WidgetConfig>::SCHEMA_VERSION,
                ),
                WidgetInstanceId::new(),
                surface,
                build_header,
                widget::any::persisted_config_of::<TestWidget>(),
                Some(link),
            )
        })
    }

    /// Everything a stage reads while it is built: the copy its widgets report
    /// through, the catalogue its panes are given widgets from, and the test widget
    /// the link tests install afterwards.
    fn install(cx: &mut App) {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
        cx.global_mut::<WidgetRegistry>()
            .register(WidgetRegistration::for_view::<TestWidget>())
            .expect("the test widget should register");
    }

    fn stage_with_two_panes(cx: &mut TestAppContext) -> (Entity<Stage>, [ElementId; 2]) {
        cx.update(install);
        let stage =
            cx.update(|cx| cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx)));
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                let root_id = match &stage.pane_tree.root {
                    PaneNode::Leaf { pane, .. } => pane.id.clone(),
                    PaneNode::Split { .. } => unreachable!(),
                };
                stage.split_pane(&root_id, PaneOrientation::Horizontal, true, cx);
            });
        });
        let pane_ids = cx.update(|cx| {
            let mut pane_ids = Vec::with_capacity(2);
            stage
                .read(cx)
                .pane_tree
                .root
                .for_each_pane(&mut |pane| pane_ids.push(pane.id.clone()));
            pane_ids
        });
        let pane_ids = pane_ids
            .try_into()
            .expect("split workspace should contain two panes");
        (stage, pane_ids)
    }

    /// A window whose root draws one stage, so a drag the test starts has a real layout
    /// and a real preview to be drawn into.
    struct StageWindow {
        stage: Entity<Stage>,
    }

    impl Render for StageWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.stage.clone())
        }
    }

    /// A dragged pane is drawn by the preview under the pointer, not by the slot it came
    /// from: the slot keeps the pane's bounds and paints nothing until the drag is over.
    #[gpui_kit::test]
    fn a_dragged_pane_should_keep_its_slot_without_painting_it(cx: &mut TestAppContext) {
        let (stage, [first, second]) = stage_with_two_panes(cx);
        cx.update(base_ui::style::theme::install);
        let (_, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, _| StageWindow { stage }
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let first_bounds = pane_bounds(cx, &stage, &first);
        let second_bounds = pane_bounds(cx, &stage, &second);
        // Which pane a header belongs to is told by the pane its box sits in: every pane draws
        // its own header, and the frame keeps one box per selector.
        let header = cx
            .debug_bounds("PANE_HEADER")
            .expect("the panes should draw their headers");
        let (dragged, survivor, dragged_bounds, survivor_bounds) =
            if first_bounds.contains(&header.center()) {
                (&first, &second, first_bounds, second_bounds)
            } else {
                (&second, &first, second_bounds, first_bounds)
            };
        let press = header.center();

        cx.simulate_mouse_down(press, MouseButton::Left, Modifiers::none());
        // Past the 2px threshold the press becomes the drag this test is about. The pointer
        // stays inside the dragged pane, which is never its own drop target.
        cx.simulate_mouse_move(press + point(px(12.0), px(0.0)), None, Modifiers::none());

        assert!(
            cx.debug_bounds("PANE_DRAG_GHOST").is_some(),
            "the pane should be drawn under the pointer while it is dragged"
        );
        let header = cx
            .debug_bounds("PANE_HEADER")
            .expect("the pane that was not dragged should still draw its header");
        assert!(
            survivor_bounds.contains(&header.center()),
            "the pane at the original position must not be drawn: the pane under the pointer is \
             the only copy"
        );
        assert_eq!(
            pane_bounds(cx, &stage, dragged),
            dragged_bounds,
            "the slot the pane was lifted from should keep its bounds instead of collapsing"
        );
        assert_eq!(
            pane_bounds(cx, &stage, survivor),
            survivor_bounds,
            "the pane beside it should not move into the slot"
        );

        // Outside the stage, so nothing takes the drop: the pane comes back when the preview
        // is released.
        cx.simulate_mouse_up(
            point(px(-10.0), px(-10.0)),
            MouseButton::Left,
            Modifiers::none(),
        );

        assert!(
            cx.debug_bounds("PANE_DRAG_GHOST").is_none(),
            "the preview should be gone with the drag"
        );
        assert!(
            cx.debug_bounds("PANE_HEADER").is_some(),
            "the pane should draw its header in its slot again once the drag is over"
        );
        assert_eq!(
            pane_bounds(cx, &stage, dragged),
            dragged_bounds,
            "the pane should be back in the slot it kept all along"
        );
    }

    /// The bounds the stage has measured for `pane_id`.
    fn pane_bounds(
        cx: &mut VisualTestContext,
        stage: &Entity<Stage>,
        pane_id: &ElementId,
    ) -> Bounds<Pixels> {
        cx.update(|_, cx| {
            stage
                .read(cx)
                .pane_tree()
                .pane_bounds(pane_id)
                .expect("the frame that drew the stage should have measured its panes")
        })
    }

    fn install_widget(
        stage: &Entity<Stage>,
        pane_id: &ElementId,
        link: InstrumentLink,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let widget = widget(link, cx);
            stage.update(cx, |stage, cx| {
                let Some(PaneNode::Leaf { pane, .. }) = stage.pane_tree.root.find_pane_mut(pane_id)
                else {
                    panic!("test pane should exist");
                };
                pane.set_widget(widget, cx);
            });
        });
    }

    fn run_joining_and_changing_a_group_only_updates_other_members(cx: &mut TestAppContext) {
        let (stage, [first, second]) = stage_with_two_panes(cx);
        let first_applications = Rc::new(Cell::new(0));
        let second_applications = Rc::new(Cell::new(0));
        let first_link = link('a', true, first_applications.clone());
        let second_link = link('b', true, second_applications.clone());
        install_widget(&stage, &first, first_link.clone(), cx);
        install_widget(&stage, &second, second_link.clone(), cx);

        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&first, InstrumentLinkMode::Group(1), cx);
                stage.set_instrument_link_mode(&second, InstrumentLinkMode::Group(1), cx);
            });
        });
        assert_eq!(second_link.selection(), selection('a'));

        cx.update(|cx| first_link.local_changed(selection('c'), cx));

        assert_eq!(second_link.selection(), selection('c'));
        assert_eq!(
            (first_applications.get(), second_applications.get()),
            (0, 2)
        );
        assert_eq!(
            cx.update(|cx| stage.read(cx).instrument_link_groups[0].clone()),
            Some(selection('c'))
        );
    }

    fn run_rejected_group_request_keeps_membership_and_selection(cx: &mut TestAppContext) {
        let (stage, [first, second]) = stage_with_two_panes(cx);
        let first_link = link('a', true, Rc::new(Cell::new(0)));
        let rejected_applications = Rc::new(Cell::new(0));
        let second_link = link('b', false, rejected_applications.clone());
        install_widget(&stage, &first, first_link, cx);
        install_widget(&stage, &second, second_link.clone(), cx);

        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&first, InstrumentLinkMode::Group(2), cx);
                stage.set_instrument_link_mode(&second, InstrumentLinkMode::Group(2), cx);
            });
        });

        assert_eq!(second_link.selection(), selection('b'));
        assert_eq!(rejected_applications.get(), 1);
        assert_eq!(
            cx.update(|cx| stage.read(cx).pane_instrument_link_mode(&second)),
            Some(InstrumentLinkMode::Group(2))
        );
    }

    fn run_pane_click_updates_followers_and_last_group_member_clears_value(
        cx: &mut TestAppContext,
    ) {
        let (stage, [first, second]) = stage_with_two_panes(cx);
        let first_link = link('a', true, Rc::new(Cell::new(0)));
        let second_link = link('b', true, Rc::new(Cell::new(0)));
        install_widget(&stage, &first, first_link, cx);
        install_widget(&stage, &second, second_link.clone(), cx);

        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&second, InstrumentLinkMode::FollowMouse, cx);
                stage.pane_clicked(&first, cx);
            });
        });
        assert_eq!(second_link.selection(), selection('a'));

        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&first, InstrumentLinkMode::Group(3), cx);
                stage.set_instrument_link_mode(&second, InstrumentLinkMode::Group(3), cx);
                stage.set_instrument_link_mode(&first, InstrumentLinkMode::Unlinked, cx);
                stage.close_pane(&second, cx);
            });
        });
        assert_eq!(
            cx.update(|cx| stage.read(cx).instrument_link_groups[2].clone()),
            None
        );
    }

    /// A pane that already shows a widget is left alone when the same widget is set
    /// again: it keeps the instance it has — with the data and the state collected in
    /// it — and the instrument link it follows.
    fn run_setting_the_widget_a_pane_already_shows_changes_nothing(cx: &mut TestAppContext) {
        let (stage, [first, _]) = stage_with_two_panes(cx);
        install_widget(&stage, &first, link('a', true, Rc::new(Cell::new(0))), cx);
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&first, InstrumentLinkMode::FollowMouse, cx);
            });
        });

        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_widget(&first, TestWidget::ID.as_str(), cx);
            });
        });

        assert_eq!(
            cx.update(|cx| stage.read(cx).pane_widget_id(&first, cx)),
            Some(TestWidget::ID),
            "the pane still shows the widget it was showing"
        );
        assert_eq!(
            cx.update(|cx| stage.read(cx).pane_instrument_link_mode(&first)),
            Some(InstrumentLinkMode::FollowMouse),
            "a pane is not reset to the widget the user was already looking at"
        );
    }

    #[test]
    fn joining_and_changing_a_group_only_updates_other_members() {
        let mut cx = TestAppContext::single();
        run_joining_and_changing_a_group_only_updates_other_members(&mut cx);
    }

    #[test]
    fn setting_the_widget_a_pane_already_shows_changes_nothing() {
        let mut cx = TestAppContext::single();
        run_setting_the_widget_a_pane_already_shows_changes_nothing(&mut cx);
    }
    #[test]
    fn rejected_group_request_keeps_membership_and_selection() {
        let mut cx = TestAppContext::single();
        run_rejected_group_request_keeps_membership_and_selection(&mut cx);
    }

    #[test]
    fn pane_click_updates_followers_and_last_group_member_clears_value() {
        let mut cx = TestAppContext::single();
        run_pane_click_updates_followers_and_last_group_member_clears_value(&mut cx);
    }
}
