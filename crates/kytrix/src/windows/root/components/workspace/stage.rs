//! The workspace: a tree of panes with drag-to-rearrange, split, resize, and
//! instrument linkage coordination.

use app_config::workspace::WorkspaceLayout;
use gpui_kit::{
    App, AppContext, Context, ElementId, Empty, Entity, EventEmitter, IntoElement, ParentElement,
    Pixels, Point, Render, SharedString, Styled, Window, div,
};
use market_participants_model::market::selection::InstrumentSelection;
use widget::{identity::WidgetId, instrument_link::InstrumentLink};
use widget_registry::registry::WidgetRegistry;

use super::{
    controls::ResizeBar,
    drag::PaneDragSource,
    layout::{StageLayoutElement, StageLayoutNode, StageLayoutNodes},
    pages::WorkspacePages,
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
    /// The page drawn over the panes. It belongs to the stage: a workspace switch replaces the
    /// stage, and a choice that was being made in the workspace that is gone has nowhere to
    /// land.
    pages: Entity<WorkspacePages>,
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
            pages: cx.new(|_| WorkspacePages::new()),
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

    /// Opens the page in which `pane_id`'s widget is chosen, over the stage.
    ///
    /// The page is created by its host rather than while the stage draws: it makes the field
    /// it is searched with, which is an entity, and it takes the caret, so building it inside
    /// a frame would be the re-entrancy a frame must not have.
    ///
    /// A second page over the first would have nothing to choose for, so opening while one is
    /// already open is a no-op.
    pub(in crate::windows::root) fn open_widget_gallery(
        &mut self,
        pane_id: &ElementId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.pane_widget_id(pane_id, cx) else {
            log::error!("widget gallery opened from a pane that no longer exists");
            return;
        };
        let pane_id = pane_id.clone();
        let stage = cx.entity().downgrade();
        let pages = self.pages.clone();

        pages.update(cx, |pages, cx| {
            pages.open_widget_gallery(pane_id, current, stage, window, cx);
        });
    }

    /// Whether the page a pane's widget is chosen from is open, including one that is leaving.
    pub(in crate::windows::root) fn is_widget_gallery_open(&self, cx: &App) -> bool {
        self.pages.read(cx).is_open()
    }

    /// Takes the page over the stage away at once, if there is one.
    pub(in crate::windows::root) fn close_page(&self, cx: &mut Context<Self>) {
        self.pages.update(cx, |pages, cx| pages.close(cx));
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

        div()
            .relative()
            .size_full()
            .child(StageLayoutElement {
                id: self.id.clone(),
                stage: cx.entity(),
                nodes,
                root_index,
                drop_target_preview: self.drop_target_preview,
            })
            // The workspace's own page covers the panes. It is drawn after them so it is
            // above them, and it decides for itself when it has nothing to show.
            .child(self.pages.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use app_config::{widget::WidgetConfig, workspace::WorkspaceLayout};
    use gpui_kit::{
        AnyView, App, AppContext, Bounds, Context, ElementId, Empty, Entity, FocusHandle,
        IntoElement, Modifiers, MouseButton, ParentElement, Pixels, Render, Styled, TestAppContext,
        VisualTestContext, Window, div, point, px, size,
    };
    use i18n::t;
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
    use widget_gallery_ui::gallery::WidgetGalleryEvent;
    use widget_registry::registry::{WidgetRegistration, WidgetRegistry};

    use crate::windows::root::components::workspace::{
        Stage,
        pages::PAGE_TRANSITION_DURATION,
        placeholder::InstrumentLinkMode,
        stage::StageChange,
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

    /// The widget a picker test switches a pane to. It has nothing to draw and nothing to
    /// load, and its name is the text a test searches the page for: the framework's own
    /// empty widget is not one the page offers, so the name narrows the page to this one.
    struct PickerWidget;

    impl WidgetView for PickerWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("kytrix.test.widget-picker");
        const DISPLAY_NAME: &'static str = "widget.names.empty";
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

    impl Render for PickerWidget {
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
    /// through, the catalogue its panes are given widgets from, and the test widgets
    /// the link and picker tests install afterwards.
    fn install(cx: &mut App) {
        gpui_kit::base::init(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
        cx.global_mut::<WidgetRegistry>()
            .register(WidgetRegistration::for_view::<TestWidget>())
            .expect("the test widget should register");
        cx.global_mut::<WidgetRegistry>()
            .register(WidgetRegistration::for_view::<PickerWidget>())
            .expect("the picker widget should register");
    }

    /// A stage with one pane showing the widget a pane falls back to.
    fn stage_with_one_pane(cx: &mut TestAppContext) -> (Entity<Stage>, ElementId) {
        cx.update(install);
        let stage =
            cx.update(|cx| cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx)));
        let pane_id = cx.update(|cx| match stage.read(cx).pane_tree().root() {
            PaneNode::Leaf { pane, .. } => pane.id.clone(),
            PaneNode::Split { .. } => unreachable!("a single pane workspace is one leaf"),
        });
        (stage, pane_id)
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

    /// A window whose root draws one stage inset from the window's own origin, so the stage's
    /// rectangles and the window's are not the same rectangles.
    struct OffsetStageWindow {
        stage: Entity<Stage>,
    }

    impl Render for OffsetStageWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .absolute()
                    .left(px(80.0))
                    .top(px(40.0))
                    .w(px(700.0))
                    .h(px(600.0))
                    .child(self.stage.clone()),
            )
        }
    }

    /// A window whose root draws one stage, resized, drawn once, and handed back together with
    /// the context that drives it.
    fn open_stage_window<'a>(
        stage: &Entity<Stage>,
        cx: &'a mut TestAppContext,
    ) -> &'a mut VisualTestContext {
        open_window(
            cx,
            StageWindow {
                stage: stage.clone(),
            },
        )
    }

    /// A window whose root draws the stage inset from the window's own origin, so the stage's
    /// rectangles are not the window's: a page placed in the stage's coordinates has to take
    /// the stage origin off before it is drawn.
    fn open_offset_stage_window<'a>(
        stage: &Entity<Stage>,
        cx: &'a mut TestAppContext,
    ) -> &'a mut VisualTestContext {
        open_window(
            cx,
            OffsetStageWindow {
                stage: stage.clone(),
            },
        )
    }

    /// A window root drawing one view, resized, drawn once, and handed back together with the
    /// context that drives it.
    fn open_window<V: Render>(cx: &mut TestAppContext, view: V) -> &mut VisualTestContext {
        cx.update(base_ui::style::theme::install);
        let (_, cx) = cx.add_window_view(move |_, _| view);
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx
    }

    /// A window root that holds the stage it is showing, the way the workspaces do: replacing
    /// the stage is what switching workspaces does.
    struct StageHost {
        stage: Option<Entity<Stage>>,
    }

    impl Render for StageHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().children(self.stage.clone())
        }
    }

    /// The debug selectors of the page the picker tests look it up by.
    const GALLERY_PANEL: &str = "WIDGET_GALLERY";
    const GALLERY_SEARCH: &str = "WIDGET_GALLERY_SEARCH";
    const GALLERY_CONFIRM: &str = "WIDGET_GALLERY_CONFIRM";
    const GALLERY_CANCEL: &str = "WIDGET_GALLERY_CANCEL";
    const PICKER_WIDGET_ROW: &str = "WIDGET_GALLERY_ROW_kytrix.test.widget-picker";

    /// The workspace's page host, which is what covers the panes while a page exists.
    const WORKSPACE_PAGES: &str = "WORKSPACE_PAGES";

    /// Lets the page's transition finish and repaints, so what is drawn is what has settled.
    fn settle_page(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(PAGE_TRANSITION_DURATION + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The bounds the stage has measured for its root.
    fn stage_bounds(cx: &mut VisualTestContext, stage: &Entity<Stage>) -> Bounds<Pixels> {
        cx.update(|_, cx| {
            stage
                .read(cx)
                .pane_tree()
                .root()
                .bounds()
                .copied()
                .expect("the frame that drew the stage should have measured it")
        })
    }

    /// Opens the picker from `pane_id` and draws the frame it opens on.
    fn open_picker(stage: &Entity<Stage>, pane_id: &ElementId, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            stage.update(cx, |stage, cx| {
                stage.open_widget_gallery(pane_id, window, cx);
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Clicks the element drawn at one debug selector and repaints, without moving the clock:
    /// what the next assertion reads is the frame that click asked for.
    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the page should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Narrows the page to the picker widget, so its row is on screen at the top of the page.
    fn search_for_picker_widget(cx: &mut VisualTestContext) {
        let field = cx
            .debug_bounds(GALLERY_SEARCH)
            .expect("the page should draw the field it is searched with")
            .center();
        cx.simulate_click(field, Modifiers::none());
        cx.run_until_parked();
        let name = cx.update(|_, cx| t!(cx, PickerWidget::DISPLAY_NAME));
        cx.simulate_input(name.as_ref());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Asserts two rectangles are the same to within a device pixel: the page is placed from
    /// the pane's measured rectangle, and both are snapped by the layout.
    fn assert_same_bounds(actual: Bounds<Pixels>, expected: Bounds<Pixels>, what: &str) {
        let distance = |a: Pixels, b: Pixels| (f32::from(a) - f32::from(b)).abs();
        assert!(
            distance(actual.origin.x, expected.origin.x) <= 1.0
                && distance(actual.origin.y, expected.origin.y) <= 1.0
                && distance(actual.size.width, expected.size.width) <= 1.0
                && distance(actual.size.height, expected.size.height) <= 1.0,
            "{what}: drew {actual:?}, expected {expected:?}"
        );
    }

    /// The page enters from the bottom of the workspace: on the frame it opens it sits one
    /// workspace-height below its place, halfway through the transition it is halfway up, and
    /// once the transition has run it covers the stage.
    ///
    /// The page is the stage's own size at every point of the slide: what the transition moves
    /// is the whole surface, not the rectangle it is drawn in.
    #[gpui_kit::test]
    fn the_page_should_slide_up_from_the_bottom_of_the_workspace(cx: &mut TestAppContext) {
        let (stage, [_, right]) = stage_with_two_panes(cx);
        let cx = open_stage_window(&stage, cx);
        let stage_rect = stage_bounds(cx, &stage);

        open_picker(&stage, &right, cx);
        let opened = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should be drawn on the frame it opens on");
        let below = Bounds::new(
            point(
                stage_rect.origin.x,
                stage_rect.origin.y + stage_rect.size.height,
            ),
            stage_rect.size,
        );
        assert_same_bounds(
            opened,
            below,
            "the page should open one workspace-height below its place",
        );
        assert!(
            opened.top() >= stage_rect.bottom() - px(1.0),
            "the opening page must start at the workspace's bottom edge: opened {opened:?}, \
             stage {stage_rect:?}"
        );

        cx.executor().advance_clock(PAGE_TRANSITION_DURATION / 2);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let mid = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should still be drawn halfway through the slide");
        assert!(
            mid.top() > stage_rect.top() && mid.top() < stage_rect.bottom(),
            "halfway through the slide the page should sit between the workspace's top and \
             bottom: mid {mid:?}, stage {stage_rect:?}"
        );
        assert_eq!(
            mid.size, stage_rect.size,
            "the sliding page keeps the workspace's size: mid {mid:?}, stage {stage_rect:?}"
        );

        settle_page(cx);
        let settled = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the settled page should still be drawn");
        assert_same_bounds(
            settled,
            stage_bounds(cx, &stage),
            "the settled page should cover the whole stage",
        );
    }

    /// The page is placed in the stage's own coordinates, not the window's: a stage inset from
    /// the window origin slides its page from its own bottom edge and settles on its own
    /// rectangle.
    ///
    /// The stage here starts at (80, 40). A page that kept the window origin would be drawn 80
    /// by 40 pixels past every rectangle asserted below, and no assertion could hold.
    #[gpui_kit::test]
    fn the_page_should_follow_an_inset_stage(cx: &mut TestAppContext) {
        let (stage, [_, right]) = stage_with_two_panes(cx);
        let cx = open_offset_stage_window(&stage, cx);
        let stage_rect = stage_bounds(cx, &stage);
        assert!(
            stage_rect.origin.x >= px(80.0) && stage_rect.origin.y >= px(40.0),
            "the fixture needs the stage away from the window origin: stage {stage_rect:?}"
        );

        open_picker(&stage, &right, cx);
        let opened = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should be drawn on the frame it opens on");
        let below = Bounds::new(
            point(
                stage_rect.origin.x,
                stage_rect.origin.y + stage_rect.size.height,
            ),
            stage_rect.size,
        );
        assert_same_bounds(
            opened,
            below,
            "the page should open on the inset stage's own bottom edge, in window coordinates",
        );

        settle_page(cx);
        let settled = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the settled page should still be drawn");
        assert_same_bounds(
            settled,
            stage_bounds(cx, &stage),
            "the settled page should cover the whole inset stage",
        );
    }

    /// Confirming changes the pane the page was opened from and no other: the widget beside it
    /// keeps both its instance and its rectangle.
    #[gpui_kit::test]
    fn confirming_a_widget_should_change_only_the_pane_it_was_opened_from(cx: &mut TestAppContext) {
        let (stage, [left, right]) = stage_with_two_panes(cx);
        let cx = open_stage_window(&stage, cx);
        let left_before = cx.update(|_, cx| stage.read(cx).pane_widget_id(&left, cx));
        let right_before = cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx));
        let right_rect = pane_bounds(cx, &stage, &right);
        assert_ne!(
            left_before,
            Some(PickerWidget::ID),
            "the fixture starts on the widget a pane falls back to"
        );

        open_picker(&stage, &left, cx);
        settle_page(cx);
        search_for_picker_widget(cx);
        click(cx, PICKER_WIDGET_ROW);
        click(cx, GALLERY_CONFIRM);

        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&left, cx)),
            Some(PickerWidget::ID),
            "confirming should apply the chosen widget to the pane the page was opened from"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx)),
            right_before,
            "confirming must not change the widget of the pane beside it"
        );
        assert_eq!(
            pane_bounds(cx, &stage, &right),
            right_rect,
            "confirming must not move the pane beside it"
        );

        settle_page(cx);
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx)),
            right_before,
            "the pane beside the page must still be showing its own widget once the page is gone"
        );
    }

    /// Dismissing the page while it is still sliding up turns it around from where it had
    /// reached: it neither jumps down to the bottom nor continues up, the pane keeps the widget
    /// it had, and the page is cleared once the slide down has finished.
    #[gpui_kit::test]
    fn cancelling_the_page_mid_slide_should_turn_it_around(cx: &mut TestAppContext) {
        let (stage, [_, right]) = stage_with_two_panes(cx);
        let cx = open_stage_window(&stage, cx);
        let before = cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx));
        let stage_rect = stage_bounds(cx, &stage);

        open_picker(&stage, &right, cx);
        cx.executor().advance_clock(PAGE_TRANSITION_DURATION / 3);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let opening = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the opening page should be drawn");
        assert!(
            opening.top() > stage_rect.top() && opening.top() < stage_rect.bottom(),
            "the page should have moved off the bottom before it is dismissed: \
             page {opening:?}, stage {stage_rect:?}"
        );

        cx.simulate_keystrokes("escape");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let turning = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should still be drawn while it turns around");
        assert_same_bounds(
            turning,
            opening,
            "the reversal should start where the opening had reached",
        );

        cx.executor().advance_clock(PAGE_TRANSITION_DURATION / 10);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let leaving = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should still be drawn while it slides back down");
        assert!(
            leaving.top() > turning.top(),
            "the page should move back down from where it turned around: \
             leaving {leaving:?}, turning {turning:?}"
        );

        settle_page(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the page should be gone once the reversal has finished"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx)),
            before,
            "dismissing the page must leave the pane showing the widget it had"
        );
    }

    /// A page can start leaving before it was ever drawn: a cancellation that arrives in the
    /// first frame the page exists opens and closes it without a frame in between.
    ///
    /// `Presence` has no drawn page to slide back down in that case — its first absent sample
    /// runs a transition from zero to zero and reports `Exiting` at zero progress — so the
    /// cleanup clears the page at zero progress too. Otherwise the host would keep the stage
    /// covered for the length of a transition that draws nothing.
    #[gpui_kit::test]
    fn a_page_that_starts_leaving_before_it_was_drawn_should_be_cleared_at_once(
        cx: &mut TestAppContext,
    ) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        cx.update(|window, cx| {
            stage.update(cx, |stage, cx| {
                stage.open_widget_gallery(&pane_id, window, cx);
            });
        });
        let gallery = cx.update(|_, cx| {
            stage
                .read(cx)
                .pages
                .read(cx)
                .gallery()
                .expect("the page should be open")
                .clone()
        });
        cx.update(|_, cx| gallery.update(cx, |_, cx| cx.emit(WidgetGalleryEvent::Cancel)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "a page that was never drawn should not outlive its cancellation"
        );
        assert!(
            cx.debug_bounds(WORKSPACE_PAGES).is_none(),
            "and the host should not keep the stage covered either"
        );
        assert!(
            cx.update(|_, cx| !stage.read(cx).is_widget_gallery_open(cx)),
            "the workspace should have let the page go"
        );
    }

    /// A window resized under an open page follows it: the page is as large as the stage it
    /// covers, not the stage it opened on.
    #[gpui_kit::test]
    fn resizing_the_window_under_an_open_page_should_keep_it_covering_the_stage(
        cx: &mut TestAppContext,
    ) {
        let (stage, [_, right]) = stage_with_two_panes(cx);
        let cx = open_stage_window(&stage, cx);
        let old_stage = stage_bounds(cx, &stage);

        open_picker(&stage, &right, cx);
        settle_page(cx);

        cx.simulate_resize(size(px(1200.0), px(900.0)));
        // The first frame after the resize measures the new stage; the page is placed by the
        // frame that draws it, so it takes one more frame to follow the measurements.
        for _ in 0..2 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }
        let resized_stage = stage_bounds(cx, &stage);
        assert!(
            resized_stage.size.width > old_stage.size.width
                && resized_stage.size.height > old_stage.size.height,
            "the resize should have reached the stage: resized {resized_stage:?}, \
             old {old_stage:?}"
        );
        let page = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should still be drawn after the resize");
        assert_same_bounds(
            page,
            resized_stage,
            "the page should cover the resized stage rather than the size it opened on",
        );
    }

    /// Switching workspaces while the page is still sliding drops the stage and the page with
    /// it: the transition it was in the middle of neither keeps the stage alive nor panics when
    /// the stage's tree is replaced.
    #[gpui_kit::test]
    fn replacing_the_stage_mid_slide_should_release_its_page(cx: &mut TestAppContext) {
        let (stage, [_, right]) = stage_with_two_panes(cx);
        let stage_weak = stage.downgrade();
        cx.update(base_ui::style::theme::install);
        let (host, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, _| StageHost { stage: Some(stage) }
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        open_picker(&stage, &right, cx);
        cx.executor().advance_clock(PAGE_TRANSITION_DURATION / 3);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let gallery_weak = cx.update(|_, cx| {
            stage
                .read(cx)
                .pages
                .read(cx)
                .gallery()
                .expect("the page should be open")
                .downgrade()
        });
        drop(stage);

        cx.update(|_, cx| {
            host.update(cx, |host, cx| {
                host.stage = None;
                cx.notify();
            });
        });
        // The frame that drops the stage's element tree is the one after the one that stops
        // drawing it.
        for _ in 0..2 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }

        assert!(
            stage_weak.upgrade().is_none(),
            "a page in the middle of its slide should not keep the stage it belongs to alive"
        );
        assert!(
            gallery_weak.upgrade().is_none(),
            "the page should be released with the stage that held it"
        );
    }

    /// While the page exists, the panes under it are not clickable: the host covers the whole
    /// stage from the first frame, so a click on a pane reaches neither that pane's click
    /// handling nor its close control, and the page stays open. The page's own controls are
    /// painted above the host and still work.
    #[gpui_kit::test]
    fn an_open_page_should_block_the_panes_under_it(cx: &mut TestAppContext) {
        let (stage, [left, right]) = stage_with_two_panes(cx);
        let left_applications = Rc::new(Cell::new(0));
        let left_link = link('a', true, left_applications.clone());
        let right_link = link('b', true, Rc::new(Cell::new(0)));
        install_widget(&stage, &left, left_link.clone(), cx);
        install_widget(&stage, &right, right_link, cx);
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&left, InstrumentLinkMode::FollowMouse, cx);
            });
        });
        let cx = open_stage_window(&stage, cx);
        let right_rect = pane_bounds(cx, &stage, &right);

        open_picker(&stage, &left, cx);
        assert!(
            cx.debug_bounds(WORKSPACE_PAGES).is_some(),
            "an open page should cover the stage under it with its host"
        );

        // A click on the other pane's body, which would otherwise be read as a pane click and
        // move the follow-mouse link to that pane's instrument.
        cx.simulate_click(right_rect.center(), Modifiers::none());
        assert_eq!(
            left_link.selection(),
            selection('a'),
            "a click on another pane must not reach it through the open page"
        );
        assert_eq!(
            left_applications.get(),
            0,
            "the pane under the page must not have handled the click"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pages.read(cx).pane_id().cloned()),
            Some(left.clone()),
            "the click must not move the page to another pane"
        );
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_some(),
            "the click must not dismiss the page"
        );
        assert_eq!(
            pane_bounds(cx, &stage, &right),
            right_rect,
            "the pane beside the page must not be moved or closed"
        );

        // A click on the other pane's header, on the close control at its end.
        let header = point(
            right_rect.right() - px(19.0),
            right_rect.origin.y + px(19.0),
        );
        cx.simulate_click(header, Modifiers::none());
        assert_eq!(
            left_link.selection(),
            selection('a'),
            "a click on another pane's header must not reach it through the open page"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx)),
            Some(TestWidget::ID),
            "the pane beside the page must still be there, showing its own widget"
        );
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_some(),
            "the click on the header must not dismiss the page"
        );

        // The page itself is above the host: once it has slid up its cancel control still
        // takes the dismissal, and the host goes with the page.
        settle_page(cx);
        click(cx, GALLERY_CANCEL);
        settle_page(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the page's own control should have dismissed it"
        );
        assert!(
            cx.debug_bounds(WORKSPACE_PAGES).is_none(),
            "the host should be gone with the page"
        );
    }

    /// The host stays over the stage while the page slides back down, not only while it slides
    /// up: once the leaving page has uncovered part of the stage, a click there must still be
    /// the host's, not that pane's.
    #[gpui_kit::test]
    fn a_page_that_is_leaving_should_still_block_the_panes_under_it(cx: &mut TestAppContext) {
        let (stage, [left, right]) = stage_with_two_panes(cx);
        let left_applications = Rc::new(Cell::new(0));
        let left_link = link('a', true, left_applications.clone());
        let right_link = link('b', true, Rc::new(Cell::new(0)));
        install_widget(&stage, &left, left_link.clone(), cx);
        install_widget(&stage, &right, right_link, cx);
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_instrument_link_mode(&left, InstrumentLinkMode::FollowMouse, cx);
            });
        });
        let cx = open_stage_window(&stage, cx);
        let right_rect = pane_bounds(cx, &stage, &right);

        open_picker(&stage, &left, cx);
        settle_page(cx);
        cx.simulate_keystrokes("escape");
        cx.executor().advance_clock(PAGE_TRANSITION_DURATION / 3);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(WORKSPACE_PAGES).is_some(),
            "a page on its way out should still cover the stage under it"
        );
        let target = point(right_rect.right() - px(24.0), right_rect.center().y);

        cx.simulate_click(target, Modifiers::none());
        assert_eq!(
            left_link.selection(),
            selection('a'),
            "a click on another pane must not reach it through a leaving page"
        );
        assert_eq!(
            left_applications.get(),
            0,
            "the pane under the leaving page must not have handled the click"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&right, cx)),
            Some(TestWidget::ID),
            "the click must not close the pane beside the page"
        );
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_some(),
            "the click must not dismiss the page that is already leaving"
        );

        settle_page(cx);
        assert!(
            cx.debug_bounds(WORKSPACE_PAGES).is_none(),
            "the host should be gone with the page"
        );
    }

    /// Closing the page does not blur the window: a focus the user moved somewhere else while
    /// the page was sliding back down — a settings page, for instance — stays where they put it.
    #[gpui_kit::test]
    fn closing_the_page_should_not_blur_the_focus_the_user_moved(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        let probe = cx.update(|_, cx| {
            cx.new(|cx| FocusProbe {
                handle: cx.focus_handle(),
            })
        });
        let handle = cx.update(|_, cx| probe.read(cx).handle.clone());

        open_picker(&stage, &pane_id, cx);
        settle_page(cx);
        click(cx, GALLERY_CANCEL);
        // The page is still sliding back down, and the user moves the focus elsewhere within
        // that window. The old cleanup took it away with an unconditional window blur.
        cx.update(|window, cx| handle.focus(window, cx));
        assert!(
            cx.update(|window, _| handle.is_focused(window)),
            "the test should have been able to move the focus while the page slides back down"
        );

        settle_page(cx);

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the page should be gone once it has slid back down"
        );
        assert!(
            cx.update(|window, _| handle.is_focused(window)),
            "closing the page must not blur the focus the user moved elsewhere"
        );
    }

    /// A focus handle a test can move the window's focus to, standing in for whatever the user
    /// focuses while the page is open.
    struct FocusProbe {
        handle: FocusHandle,
    }

    /// Confirming applies the chosen widget at once and holds the page over the switch until it
    /// has slid back down, which is when the new widget is uncovered.
    #[gpui_kit::test]
    fn confirming_a_widget_should_apply_it_then_reveal_the_pane(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        let changes = Rc::new(Cell::new(0usize));
        let committed = changes.clone();
        let _changes = cx.update(|_, cx| {
            cx.subscribe(&stage, move |_, _: &StageChange, _| {
                committed.set(committed.get() + 1);
            })
        });
        open_picker(&stage, &pane_id, cx);
        settle_page(cx);

        search_for_picker_widget(cx);
        click(cx, PICKER_WIDGET_ROW);
        assert_eq!(
            changes.get(),
            0,
            "opening the page and picking a row are not changes to the workspace"
        );
        click(cx, GALLERY_CONFIRM);

        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx)),
            Some(PickerWidget::ID),
            "confirming should apply the chosen widget immediately"
        );
        assert_eq!(
            changes.get(),
            1,
            "confirming should persist the pane's new widget through the usual committed change"
        );
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_some(),
            "the page should still cover the switching pane while it slides back down"
        );

        settle_page(cx);

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the page should be gone once it has slid back down"
        );
        assert_eq!(
            changes.get(),
            1,
            "the slide itself should not report another change"
        );
    }

    /// Backing out changes nothing and takes the page away once it has slid back down.
    #[gpui_kit::test]
    fn cancelling_the_picker_should_leave_the_pane_alone(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        let before = cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx));
        open_picker(&stage, &pane_id, cx);
        settle_page(cx);

        click(cx, GALLERY_CANCEL);
        settle_page(cx);

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "backing out should take the page away once it has slid back down"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx)),
            before,
            "backing out should leave the pane showing the widget it was showing"
        );
    }

    /// Enter is the page's own confirmation, and it goes through the same report the confirm
    /// control uses: the selected widget is applied to the pane the page belongs to.
    #[gpui_kit::test]
    fn enter_should_confirm_the_widget_selected_in_the_picker(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        open_picker(&stage, &pane_id, cx);
        settle_page(cx);

        search_for_picker_widget(cx);
        click(cx, PICKER_WIDGET_ROW);
        cx.simulate_keystrokes("enter");

        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx)),
            Some(PickerWidget::ID),
            "Enter should confirm the widget the page has selected"
        );
        settle_page(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the confirmed page should be gone once it has slid back down"
        );
    }

    /// Escape is the page's own way out, and it leaves the pane alone just as the cancel control
    /// does.
    #[gpui_kit::test]
    fn escape_should_close_the_picker_without_changing_the_pane(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        let before = cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx));
        open_picker(&stage, &pane_id, cx);
        settle_page(cx);

        cx.simulate_keystrokes("escape");
        settle_page(cx);

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "Escape should take the page away once it has slid back down"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx)),
            before,
            "Escape should leave the pane showing the widget it was showing"
        );
    }

    /// Reduced motion cuts the slide and the fade to their ends: the page is the whole stage on
    /// the frame it opens, and closing it is a frame rather than a slide.
    #[gpui_kit::test]
    fn reduced_motion_should_open_the_picker_at_full_size_and_close_it_at_once(
        cx: &mut TestAppContext,
    ) {
        let (stage, [_, right]) = stage_with_two_panes(cx);
        cx.update(|cx| cx.set_reduce_motion(true));
        let cx = open_stage_window(&stage, cx);
        let stage_rect = stage_bounds(cx, &stage);
        let pane = pane_bounds(cx, &stage, &right);
        assert!(
            pane.size.width < stage_rect.size.width,
            "the fixture needs a pane smaller than the stage: pane {pane:?}, \
             stage {stage_rect:?}"
        );

        open_picker(&stage, &right, cx);
        let opened = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the page should be drawn");
        assert_same_bounds(
            opened,
            stage_rect,
            "reduced motion should open the page at its settled size",
        );
        assert!(
            f32::from(opened.origin.x) < f32::from(pane.origin.x)
                && opened.size.width > pane.size.width,
            "reduced motion must open the page at the stage, not at the pane it was opened \
             from: opened {opened:?}, pane {pane:?}"
        );

        click(cx, GALLERY_CANCEL);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "reduced motion should close the page on the frame it is dismissed on"
        );
    }

    /// A confirmation and a cancellation can arrive for the same page: the one behind the choice
    /// is the one that counts, and the second neither panics nor undoes it.
    #[gpui_kit::test]
    fn a_second_report_while_the_picker_leaves_should_not_undo_the_first(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);
        open_picker(&stage, &pane_id, cx);
        settle_page(cx);
        search_for_picker_widget(cx);
        click(cx, PICKER_WIDGET_ROW);

        click(cx, GALLERY_CONFIRM);
        // The page is still drawn while it slides back down, so the Escape of a second press reaches
        // it and reports a cancellation for a choice that is already made.
        cx.simulate_keystrokes("escape");

        settle_page(cx);

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the page should have left once its slide finished"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx)),
            Some(PickerWidget::ID),
            "a report that arrives while the page leaves must not undo the choice behind it"
        );
    }

    /// The page takes the keyboard when it opens: its search field is focused. When the page is
    /// gone, the focus it held goes with it — GPUI releases a handle whose entity was dropped
    /// instead of keeping the window pointed at a page that can no longer be drawn. Both ways
    /// out have to leave the window in that state; neither may panic and neither may leave the
    /// focus behind.
    #[gpui_kit::test]
    fn closing_the_picker_should_release_the_focus_it_held(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let cx = open_stage_window(&stage, cx);

        open_picker(&stage, &pane_id, cx);
        settle_page(cx);
        click(cx, GALLERY_SEARCH);
        assert!(
            cx.update(|window, cx| window.focused(cx)).is_some(),
            "the page should hold the keyboard while it is open"
        );
        click(cx, GALLERY_CANCEL);
        settle_page(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the page should be gone once it has slid back down"
        );
        assert!(
            cx.update(|window, cx| window.focused(cx)).is_none(),
            "a cancelled page should leave no focus behind"
        );

        open_picker(&stage, &pane_id, cx);
        settle_page(cx);
        click(cx, GALLERY_SEARCH);
        assert!(
            cx.update(|window, cx| window.focused(cx)).is_some(),
            "the page should hold the keyboard while it is open"
        );
        search_for_picker_widget(cx);
        click(cx, PICKER_WIDGET_ROW);
        click(cx, GALLERY_CONFIRM);
        settle_page(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the confirmed page should be gone once it has slid back down"
        );
        assert!(
            cx.update(|window, cx| window.focused(cx)).is_none(),
            "a confirmed page should leave no focus behind"
        );
    }

    /// Opening the page on a pane that already shows a widget preselects that widget:
    /// confirming without searching or choosing anything confirms the widget the pane shows,
    /// and the workspace applies nothing — the pane keeps the instance it already has and no
    /// committed change is reported. The page still closes, so the confirmation really was
    /// made rather than the control being disabled.
    #[gpui_kit::test]
    fn confirming_the_widget_the_pane_already_shows_should_keep_its_instance(
        cx: &mut TestAppContext,
    ) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.set_widget(&pane_id, TestWidget::ID.as_str(), cx);
            });
        });
        let cx = open_stage_window(&stage, cx);
        let changes = Rc::new(Cell::new(0usize));
        let committed = changes.clone();
        let _changes = cx.update(|_, cx| {
            cx.subscribe(&stage, move |_, _: &StageChange, _| {
                committed.set(committed.get() + 1);
            })
        });

        open_picker(&stage, &pane_id, cx);
        settle_page(cx);
        click(cx, GALLERY_CONFIRM);

        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(&pane_id, cx)),
            Some(TestWidget::ID),
            "confirming the widget the pane already shows should not replace it"
        );
        assert_eq!(
            changes.get(),
            0,
            "confirming the widget the pane already shows is not a change to the workspace"
        );
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_some(),
            "the page should still cover the pane while it slides back down"
        );

        settle_page(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "the confirmed page should be gone once it has slid back down"
        );
        assert_eq!(
            changes.get(),
            0,
            "the slide itself should not report a change either"
        );
    }

    /// Switching workspaces replaces the stage, and the page a stage held goes with it: nothing
    /// of the choice session outlives the stage it was opened in, and the page is not what keeps
    /// that stage alive.
    #[gpui_kit::test]
    fn replacing_the_stage_should_drop_its_open_picker(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_one_pane(cx);
        let stage_weak = stage.downgrade();
        cx.update(base_ui::style::theme::install);
        let (host, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, _| StageHost { stage: Some(stage) }
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        open_picker(&stage, &pane_id, cx);
        settle_page(cx);
        let gallery_weak = cx.update(|_, cx| {
            stage
                .read(cx)
                .pages
                .read(cx)
                .gallery()
                .expect("the page should be open")
                .downgrade()
        });
        drop(stage);

        cx.update(|_, cx| {
            host.update(cx, |host, cx| {
                host.stage = None;
                cx.notify();
            });
        });
        // The frame that drops the stage's element tree is the one after the one that stops
        // drawing it.
        for _ in 0..2 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }

        assert!(
            stage_weak.upgrade().is_none(),
            "an open page should not be what keeps the stage it belongs to alive"
        );
        assert!(
            gallery_weak.upgrade().is_none(),
            "the page should be released with the stage that held it"
        );
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
