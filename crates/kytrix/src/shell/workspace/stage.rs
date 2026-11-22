//! The active workspace's stage: its panes, their geometry, and the page over them.
//!
//! The stage owns what the workspace is showing: the pane topology ([`PaneTree`]), the
//! persistent pane subtrees ([`Pane`]), one split bar per split, and the widget gallery page
//! while it is open. It is the one place that turns the topology into rectangles: every
//! frame it reads the stage's measured bounds and, when they moved, lays the panes and the
//! bars out with `LayoutOverride`.
//!
//! # What a structure change costs
//!
//! Splitting mounts one new pane and adds one bar; closing releases one pane and removes
//! it. Neither touches the surviving panes, which is what keeps their widgets (and their
//! state) alive across a rearrangement. Everything that follows from the topology — the
//! close button's visibility, the bars, the fraction the settings store — is reconciled
//! after the change rather than rebuilt from scratch.
//!
//! # Widget config reports
//!
//! A widget reports that its persisted config changed through the
//! [`WidgetConfigChange`] the pane mounts it with. The report sets the stage's dirty flag;
//! the shell's route persists the workspace when it sees the flag, once, after the frame
//! that applied the change. The report never writes the settings itself: the composition
//! root's store is the only writer.

use std::{cell::Cell, rc::Rc};

use app_config::workspace::{WorkspaceLayout, WorkspaceWidget};
use base_ui::{
    style_builder::{PaintBuilder, StyleBuilder},
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, Event},
    interaction::DragPhase,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    window::WindowId,
};
use widget::{config::WidgetConfigChange, identity::WidgetId};
use widget_gallery_ui::gallery::WidgetGalleryEvent;
use widget_registry::registry::WidgetRegistry;

use super::{
    super::{
        intent::Intent,
        metrics,
        ui::{self, Intents},
    },
    gallery::GalleryPage,
    pane::{Pane, PaneParts},
    tree::{PaneId, PaneOrientation, PaneTree, SplitId},
};

/// The stage's debug id.
pub(crate) const WORKSPACE_STAGE: &str = "WORKSPACE_STAGE";

/// The split bar's debug id prefix.
const SPLIT_BAR: &str = "SPLIT_BAR";

/// One split bar and what a drag needs to know about it.
struct SplitBar {
    split: SplitId,
    node: NodeId,
    /// Which way the bar divides its children.
    orientation: PaneOrientation,
    /// The action the bar's drag dispatches; released with the bar.
    action: Action,
    /// The head fraction the bar's drag started from.
    drag_origin: Option<f32>,
    /// The content extent (logical pixels) the bar splits along its axis.
    extent: f32,
}

/// The bar's layout: an absolutely placed box at `rect` with a two-pixel line centred in it.
fn bar_style(orientation: PaneOrientation, rect: Rect) -> ui_core::paint::style::Style {
    let base = StyleBuilder::new()
        .absolute()
        .left(rect.origin.x)
        .top(rect.origin.y);
    match orientation {
        PaneOrientation::Horizontal => base
            .width(metrics::SPLIT_BAR_SIZE)
            .height(rect.size.height)
            .flex_col()
            .items_center()
            .build(),
        PaneOrientation::Vertical => base
            .width(rect.size.width)
            .height(metrics::SPLIT_BAR_SIZE)
            .flex_row()
            .items_center()
            .build(),
    }
}

/// The active workspace.
pub(crate) struct Stage {
    window: WindowId,
    theme: ThemeHandle,
    translate: Translate,
    intents: Intents,
    /// The application's one registry and services.
    registry: Rc<WidgetRegistry>,
    services: Rc<widget::services::WidgetServices>,
    language_signal: ui_core::node::signal::Signal<LanguageCode>,
    language: LanguageCode,

    /// The stage's own root.
    root: NodeId,
    tree: PaneTree,
    panes: Vec<Pane>,
    bars: Vec<SplitBar>,
    gallery: Option<GalleryPage>,
    /// The measured stage size the panes were last laid out at.
    viewport: ui_core::math::Size,
    /// Set when the topology or a fraction changed: the layout has to be recomputed even
    /// though the stage's measured size did not move.
    geometry_dirty: Cell<bool>,
    /// Set when a widget reports a config change the settings must follow.
    dirty: Rc<Cell<bool>>,
}

impl Stage {
    /// The dirty flag a widget's config report sets.
    pub(crate) fn dirty_flag(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.dirty)
    }

    /// Builds the stage for `layout` inside `parent` (the page area).
    pub(crate) fn build(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        composition: &crate::composition::Composition,
        intents: Intents,
        visible: ui_core::node::signal::Signal<bool>,
        layout: &WorkspaceLayout,
    ) -> Result<Self, EngineError> {
        let root = NodeBuilder::boxed()
            .id(SmolStr::new_static(WORKSPACE_STAGE))
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .left(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .flex_col()
                    .overflow_hidden()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                visible: Some(*cx.read(&visible)),
                ..StylePatch::default()
            })
            .build(ui::tree(app, window)?, parent)?;
        let language = composition
            .hub
            .read(|config| crate::language::code(config.language));
        let dirty = Rc::new(Cell::new(false));
        let (tree, leaves) = PaneTree::from_layout(layout);
        let mut stage = Self {
            window,
            theme: composition.theme.clone(),
            translate: composition.translate.clone(),
            intents,
            registry: Rc::clone(&composition.catalogue),
            services: Rc::clone(composition.services()),
            language_signal: composition.services().language,
            language,
            root,
            tree,
            panes: Vec::with_capacity(leaves.len()),
            bars: Vec::new(),
            gallery: None,
            viewport: ui_core::math::Size::default(),
            geometry_dirty: Cell::new(true),
            dirty,
        };
        for (pane, stored) in leaves {
            stage.mount_pane(app, window, pane, &stored)?;
        }
        stage.rebuild_bars(app, window)?;
        stage.sync_close_buttons(app, window)?;
        stage.apply_geometry(app)?;

        Ok(stage)
    }

    /// The intent queue the stage's own controls report into.
    fn intents(&self) -> Intents {
        self.intents.clone()
    }

    /// The widget config-change report a pane's widget publishes through.
    fn config_change(&self) -> WidgetConfigChange {
        let dirty = Rc::clone(&self.dirty);
        let window = self.window;
        WidgetConfigChange::new(move |app: &mut App| {
            dirty.set(true);
            let _ = app.request_repaint(window);
        })
    }

    /// Mounts one pane and the widget its layout stored.
    fn mount_pane(
        &mut self,
        app: &mut App,
        window: WindowId,
        pane: PaneId,
        stored: &WorkspaceWidget,
    ) -> Result<(), EngineError> {
        let mounted = Pane::mount(
            app,
            window,
            self.root,
            pane,
            stored,
            PaneParts {
                registry: &self.registry,
                services: &self.services,
                config_change: self.config_change(),
                intents: self.intents(),
                theme: &self.theme,
                translate: &self.translate,
            },
        )?;
        self.panes.push(mounted);

        Ok(())
    }

    /// The pane with this identity, if the stage shows it.
    fn pane(&self, pane: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|candidate| candidate.id == pane)
    }

    fn pane_mut(&mut self, pane: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|candidate| candidate.id == pane)
    }

    /// The widget type the pane is showing (what the gallery opens on).
    fn pane_widget(&self, pane: PaneId) -> Option<WidgetId> {
        self.pane(pane)
            .and_then(|pane| WidgetId::parse(pane.widget_id()).ok())
    }

    /// Writes the workspace's layout as it stands, taking each pane's widget back from the
    /// widget it hosts.
    pub(crate) fn snapshot(&self) -> WorkspaceLayout {
        let panes = &self.panes;
        self.tree.snapshot(|pane| {
            panes
                .iter()
                .find(|candidate| candidate.id == pane)
                .map(Pane::snapshot)
                .unwrap_or_else(|| WorkspaceWidget {
                    id: SmolStr::new_static(app_config::workspace::DEFAULT_WIDGET_ID),
                    config: None,
                })
        })
    }

    /// How many panes the workspace is split into.
    #[cfg(test)]
    pub(crate) fn pane_count(&self) -> usize {
        self.tree.panes().len()
    }

    /// Whether the gallery page is open (including while it leaves).
    #[cfg(test)]
    pub(crate) fn gallery_open(&self) -> bool {
        self.gallery.is_some()
    }

    /// Runs one frame of the stage.
    pub(crate) fn sync(
        &mut self,
        app: &mut App,
        composition: &crate::composition::Composition,
    ) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }

        self.sync_gallery(app, composition)?;
        self.sync_language(app)?;
        self.apply_geometry(app)?;
        for pane in &self.panes {
            pane.sync(app);
        }

        Ok(())
    }

    /// Re-lays the panes out when the stage's measured size changed, or when the topology
    /// or a split fraction moved since the last pass.
    ///
    /// The size check alone is not enough: splitting, closing, choosing a widget and
    /// dragging a splitter all change the rectangles while the window stays the same size,
    /// and those changes are what marks the layout dirty.
    fn apply_geometry(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(target) = app.window(self.window) else {
            return Ok(());
        };
        let size = target
            .tree()
            .get(self.root)
            .map(|node| node.layout().bounds.size)
            .unwrap_or_default();
        if size == self.viewport && !self.geometry_dirty.get() {
            return Ok(());
        }
        self.viewport = size;
        self.geometry_dirty.set(false);
        let viewport = Rect::new(0.0, 0.0, size.width, size.height);
        let geometry = self.tree.layout(viewport);
        for (pane, rect) in geometry.panes {
            if let Some(pane) = self.pane_mut(pane) {
                pane.set_rect(app, rect)?;
            }
        }
        for (split, rect, extent) in geometry.splits {
            if let Some(bar) = self.bars.iter_mut().find(|bar| bar.split == split) {
                bar.extent = extent;
                let style = bar_style(bar.orientation, rect);
                let tree = ui::tree(app, self.window)?;
                tree.set_style(bar.node, style)?;
            }
        }

        Ok(())
    }

    /// Follows the language: the pane headers are the pane's own catalogue copy.
    fn sync_language(&mut self, app: &mut App) -> Result<(), EngineError> {
        let language = app.read_with(&self.language_signal, |language| *language);
        if language == self.language {
            return Ok(());
        }
        self.language = language;
        for pane in &mut self.panes {
            pane.rebuild_header(app, self.window)?;
        }

        Ok(())
    }

    /// Applies one request the shell's route drained.
    ///
    /// Returns whether the workspace changed in a way the stored layout must follow.
    pub(crate) fn apply(
        &mut self,
        app: &mut App,
        composition: &crate::composition::Composition,
        intent: &Intent,
    ) -> Result<bool, EngineError> {
        match intent {
            Intent::SplitPane {
                pane,
                orientation,
                target_on_head,
            } => self.split_pane(app, *pane, *orientation, *target_on_head),
            Intent::ClosePane(pane) => self.close_pane(app, *pane),
            Intent::OpenGallery(pane) => {
                self.open_gallery(app, composition, *pane)?;
                Ok(false)
            }
            Intent::ChooseWidget { pane, widget } => self.choose_widget(app, *pane, widget),

            Intent::SplitDrag {
                split,
                phase,
                delta,
            } => self.drag_split(app, *split, *phase, *delta),
            _ => Ok(false),
        }
    }

    /// Splits a pane, mounting the new pane with the default widget.
    fn split_pane(
        &mut self,
        app: &mut App,
        pane: PaneId,
        orientation: PaneOrientation,
        target_on_head: bool,
    ) -> Result<bool, EngineError> {
        let Some((new_pane, _split)) = self.tree.split_pane(pane, orientation, target_on_head)
        else {
            log::error!("split pane {} failed: it is not shown", pane.index());
            return Ok(false);
        };
        let stored = WorkspaceWidget {
            id: SmolStr::new_static(app_config::workspace::DEFAULT_WIDGET_ID),
            config: None,
        };
        let window = self.window;
        self.mount_pane(app, window, new_pane, &stored)?;
        self.rebuild_bars(app, window)?;
        self.sync_close_buttons(app, window)?;
        self.geometry_dirty.set(true);
        self.apply_geometry(app)?;

        Ok(true)
    }

    /// Closes a pane; the last pane of a workspace cannot be closed.
    fn close_pane(&mut self, app: &mut App, pane: PaneId) -> Result<bool, EngineError> {
        if !self.tree.is_split() {
            log::error!(
                "pane {} is the only pane and cannot be closed",
                pane.index()
            );
            return Ok(false);
        }
        if !self.tree.remove_pane(pane) {
            log::error!("pane {} is not shown", pane.index());
            return Ok(false);
        }
        if let Some(index) = self.panes.iter().position(|candidate| candidate.id == pane) {
            let mut removed = self.panes.remove(index);
            removed.release(app);
        }
        self.rebuild_bars(app, self.window)?;
        self.sync_close_buttons(app, self.window)?;
        self.geometry_dirty.set(true);
        self.apply_geometry(app)?;
        if let Some(page) = &self.gallery
            && page.pane() == pane
        {
            self.close_gallery(app);
        }

        Ok(true)
    }

    /// Replaces a pane's widget with the one the gallery chose.
    fn choose_widget(
        &mut self,
        app: &mut App,
        pane: PaneId,
        widget: &SmolStr,
    ) -> Result<bool, EngineError> {
        let stored = WorkspaceWidget {
            id: widget.clone(),
            config: None,
        };
        let window = self.window;
        let registry = Rc::clone(&self.registry);
        let services = Rc::clone(&self.services);
        let config_change = self.config_change();
        let Some(target) = self.pane_mut(pane) else {
            log::error!("the gallery chose a widget for a pane that is gone");
            return Ok(false);
        };
        target.set_widget(app, window, &stored, &registry, &services, config_change)?;
        self.apply_geometry(app)?;

        Ok(true)
    }

    /// Opens the widget gallery over the workspace for `pane`.
    fn open_gallery(
        &mut self,
        app: &mut App,
        composition: &crate::composition::Composition,
        pane: PaneId,
    ) -> Result<(), EngineError> {
        if self.gallery.is_some() {
            return Ok(());
        }
        let Some(current) = self.pane_widget(pane) else {
            log::error!("the gallery was opened for a pane that is gone");
            return Ok(());
        };
        let page = GalleryPage::open(
            app,
            self.window,
            self.root,
            composition,
            pane,
            Some(current),
        )?;
        self.gallery = Some(page);

        Ok(())
    }

    /// Releases the gallery page if it is open.
    pub(crate) fn close_gallery(&mut self, app: &mut App) {
        if let Some(page) = self.gallery.take() {
            page.release(app);
        }
    }

    /// Polls the gallery's report, applies the choice and lets the page leave.
    fn sync_gallery(
        &mut self,
        app: &mut App,
        composition: &crate::composition::Composition,
    ) -> Result<(), EngineError> {
        let Some(page) = &self.gallery else {
            return Ok(());
        };
        let pane = page.pane();
        let Some(event) = page.take_event() else {
            return Ok(());
        };
        if page.is_closing() {
            return Ok(());
        }
        let mut leaving = true;
        if let WidgetGalleryEvent::Confirm(widget) = event {
            let chosen = SmolStr::new(widget.as_str());
            let _ = self.apply(
                app,
                composition,
                &Intent::ChooseWidget {
                    pane,
                    widget: chosen,
                },
            )?;
            leaving = true;
        }
        if leaving && let Some(page) = self.gallery.as_mut() {
            page.begin_closing();
        }
        if self.gallery.as_ref().is_some_and(|page| page.is_closing()) {
            self.close_gallery(app);
        }

        Ok(())
    }

    /// Rebuilds the split bars from the topology.
    fn rebuild_bars(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        for bar in std::mem::take(&mut self.bars) {
            app.off_action(&bar.action);
            let _ = app.remove_node(window, bar.node);
        }
        let splits = self.tree.splits();
        for split in splits {
            let orientation = self
                .tree
                .split_orientation(split)
                .unwrap_or(PaneOrientation::Horizontal);
            let action = Action::Custom(smol_str::format_smolstr!(
                "kytrix.split.drag.{}",
                split.index()
            ));
            let node = NodeBuilder::boxed()
                .id(smol_str::format_smolstr!("{SPLIT_BAR}.{}", split.index()))
                .style(bar_style(orientation, Rect::default()))
                .on_drag(action.clone())
                .build(ui::tree(app, window)?, self.root)?;
            let bar_line = match orientation {
                PaneOrientation::Horizontal => StyleBuilder::new().width(2.0).height_full().build(),
                PaneOrientation::Vertical => StyleBuilder::new().height(2.0).width_full().build(),
            };
            let mut bar_paint = PaintBuilder::new().build();
            bar_paint.background =
                ui::color(&self.theme, app, base_ui::theme::role::Role::LineBorder);
            NodeBuilder::boxed()
                .style(bar_line)
                .paint(bar_paint)
                .build(ui::tree(app, window)?, node)?;
            let intents = self.intents();
            app.on_action(action.clone(), move |ctx| {
                if let ui_core::event::ActionSource::Drag { phase, delta } = ctx.source() {
                    let intent = Intent::SplitDrag {
                        split,
                        phase,
                        delta,
                    };
                    // The handler runs during event dispatch, before the frame: reporting
                    // the drag and asking for the frame is all it may do.
                    let window = ctx.window();
                    ctx.app_mut().request_repaint(window).ok();
                    intents.borrow_mut().push(intent);
                }
            });
            self.bars.push(SplitBar {
                split,
                node,
                orientation,
                action,
                drag_origin: None,
                extent: 0.0,
            });
        }

        Ok(())
    }

    /// Moves one split's bar with a drag.
    fn drag_split(
        &mut self,
        app: &mut App,
        split: SplitId,
        phase: DragPhase,
        delta: ui_core::math::Point,
    ) -> Result<bool, EngineError> {
        let Some(bar) = self.bars.iter_mut().find(|bar| bar.split == split) else {
            return Ok(false);
        };
        let orientation = self
            .tree
            .split_orientation(split)
            .unwrap_or(PaneOrientation::Horizontal);
        match phase {
            DragPhase::Start => {
                bar.drag_origin = self.tree.fraction(split);
                Ok(false)
            }
            DragPhase::Move => {
                let Some(origin) = bar.drag_origin else {
                    return Ok(false);
                };
                if bar.extent <= 0.0 {
                    return Ok(false);
                }
                let extent = bar.extent;
                let offset = match orientation {
                    PaneOrientation::Horizontal => delta.x,
                    PaneOrientation::Vertical => delta.y,
                };
                self.tree.set_fraction(split, origin + offset / extent);
                self.geometry_dirty.set(true);
                self.apply_geometry(app)?;

                // The drag moved the panes on screen, but the stored layout is written
                // once, on `End`: a settings write per pointer move is a synchronous disk
                // write per frame (ledger C7).
                Ok(false)
            }
            DragPhase::End | DragPhase::Cancel => {
                bar.drag_origin = None;

                Ok(phase == DragPhase::End)
            }
        }
    }

    /// Rebuilds the panes' headers when the close button's visibility changed.
    fn sync_close_buttons(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let visible = self.tree.is_split();
        for pane in &mut self.panes {
            pane.set_close_button_visible(app, window, visible)?;
        }

        Ok(())
    }

    /// Forwards one platform event to the panes' kit controls.
    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        for pane in &mut self.panes {
            pane.dispatch(app, event);
        }
    }

    /// Releases every pane, bar and page the stage owns, and its own root.
    pub(crate) fn release(&mut self, app: &mut App) {
        if let Some(page) = self.gallery.take() {
            page.release(app);
        }
        for bar in std::mem::take(&mut self.bars) {
            app.off_action(&bar.action);
            let _ = app.remove_node(self.window, bar.node);
        }
        for mut pane in std::mem::take(&mut self.panes) {
            pane.release(app);
        }
        let _ = app.remove_node(self.window, self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::{super::pane::HeaderAction, *};

    /// The split directions a pane offers are the two the stage can apply.
    #[test]
    fn a_pane_should_offer_both_split_directions() {
        let pane = PaneId::test(0);
        let right = HeaderAction::SplitRight.intent(pane);
        let down = HeaderAction::SplitDown.intent(pane);
        assert!(matches!(
            right,
            Some(Intent::SplitPane {
                orientation: PaneOrientation::Horizontal,
                target_on_head: true,
                ..
            })
        ));
        assert!(matches!(
            down,
            Some(Intent::SplitPane {
                orientation: PaneOrientation::Vertical,
                target_on_head: true,
                ..
            })
        ));
    }
}
