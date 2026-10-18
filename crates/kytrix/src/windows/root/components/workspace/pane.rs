//! A single workspace pane: its frame, its header row, and the widget it hosts.

use app_config::workspace::WorkspaceWidget;
use base_ui::{separator::Separator, style::theme::ThemeExt};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, Styled, WeakEntity, Window, div, prelude::FluentBuilder,
};
use smol_str::SmolStr;
use widget::{
    any::AnyWidget, config::WidgetConfigChange, identity::WidgetId, instrument_link::InstrumentLink,
};
use widget_registry::registry::WidgetRegistry;

use super::{
    controls::{PaneCloseButton, PaneSplitButton, PaneWidgetButton},
    drag::PaneDragRegion,
    placeholder::{InstrumentLinkMode, InstrumentLinker},
    stage::Stage,
    tree::NodeId,
};

#[derive(IntoElement, Debug, Clone)]
pub(in crate::windows::root) struct Pane {
    pub id: ElementId,
    stage: WeakEntity<Stage>,
    /// The widget this pane shows. Every pane shows one: the user's choice, or the
    /// widget a pane falls back to while they have not made one.
    widget: Entity<AnyWidget>,
    pub instrument_link: Option<InstrumentLink>,
    pub instrument_link_mode: InstrumentLinkMode,
    /// Whether this pane draws a header. The detached copy that follows the
    /// cursor during a pane drag draws the body only: GPUI prepaints the drag
    /// view *after* `prepaint_deferred_draws`, so a popup host rendered inside it
    /// would be painted without ever being prepainted and panic.
    dragging: bool,
    /// Whether this pane is one of several, and therefore closeable.
    pub close_button_visible: bool,
    /// Whether restoring this pane's widget replaced a config it could not read.
    ///
    /// The pane reports it so the workspaces can write the repaired config out
    /// once, at startup, instead of leaving the broken one in the settings file.
    widget_config_repaired: bool,
}

impl Pane {
    /// Builds a new pane, showing the widget a pane falls back to.
    ///
    /// This is what a split adds: a pane nobody has chosen a widget for.
    pub(in crate::windows::root) fn new(
        stage_id: &ElementId,
        node_id: NodeId,
        stage: Entity<Stage>,
        cx: &mut Context<Stage>,
    ) -> Self {
        let id = pane_element_id(stage_id, node_id);
        let widget = default_widget(&id, &stage.downgrade(), cx);

        Self::build(
            id,
            widget,
            stage,
            InstrumentLinkMode::Unlinked,
            false,
            false,
            cx,
        )
    }

    /// Rebuilds the pane a stored layout names, showing the widget that layout
    /// stores.
    ///
    /// A widget that is no longer registered falls back to the default widget: one
    /// pane the application cannot rebuild is not a reason to lose the rest of the
    /// workspace, and a pane showing a hint is better than a pane that refuses to
    /// exist. A stored config that cannot be read does not fall back, because it
    /// decodes into the widget's defaults — the pane comes back with default
    /// settings and [`widget_config_repaired`](Self::widget_config_repaired) reports
    /// that the stored one has to be replaced.
    pub(in crate::windows::root) fn restored(
        stage_id: &ElementId,
        node_id: NodeId,
        stored: &WorkspaceWidget,
        stage: Entity<Stage>,
        cx: &mut Context<Stage>,
    ) -> Self {
        let id = pane_element_id(stage_id, node_id);
        let stage_weak = stage.downgrade();
        let (widget, repaired) = create_widget(Some(stored), &id, &stage_weak, cx);
        let mut pane = Self::build(
            id,
            widget,
            stage,
            InstrumentLinkMode::Unlinked,
            false,
            false,
            cx,
        );
        pane.widget_config_repaired = repaired;

        pane
    }

    /// Builds one pane around an already created widget.
    pub(in crate::windows::root) fn build(
        id: ElementId,
        widget: Entity<AnyWidget>,
        stage: Entity<Stage>,
        instrument_link_mode: InstrumentLinkMode,
        dragging: bool,
        close_button_visible: bool,
        cx: &mut App,
    ) -> Self {
        let stage_weak = stage.downgrade();
        let instrument_link = widget.read(cx).instrument_link().cloned();
        if let Some(link) = &instrument_link {
            Self::bind_instrument_link(&id, &stage_weak, link);
        }

        Self {
            id,
            stage: stage_weak,
            widget,
            instrument_link,
            instrument_link_mode,
            dragging,
            close_button_visible,
            widget_config_repaired: false,
        }
    }

    pub(in crate::windows::root) fn set_widget(&mut self, widget: Entity<AnyWidget>, cx: &mut App) {
        self.instrument_link = widget.read(cx).instrument_link().cloned();
        if let Some(link) = &self.instrument_link {
            Self::bind_instrument_link(&self.id, &self.stage, link);
        }
        self.instrument_link_mode = InstrumentLinkMode::Unlinked;
        self.widget = widget;
    }

    /// The widget type this pane is showing.
    pub(in crate::windows::root) fn widget_id<'a>(&self, cx: &'a App) -> &'a WidgetId {
        self.widget.read(cx).widget_id()
    }

    /// The widget this pane shows, in the form persistence stores it.
    ///
    /// The config comes from the widget itself rather than from the envelope this
    /// pane was restored with: a widget changes its own config while it runs, and
    /// the envelope is a record of where it started.
    pub(in crate::windows::root) fn snapshot(&self, cx: &App) -> WorkspaceWidget {
        let widget = self.widget.read(cx);

        WorkspaceWidget {
            id: SmolStr::new(widget.widget_id().as_str()),
            config: widget.config(cx),
        }
    }

    /// Whether restoring this pane replaced a config that could not be read.
    pub(in crate::windows::root) const fn widget_config_repaired(&self) -> bool {
        self.widget_config_repaired
    }

    fn bind_instrument_link(pane_id: &ElementId, stage: &WeakEntity<Stage>, link: &InstrumentLink) {
        let pane_id = pane_id.clone();
        let stage = stage.clone();
        link.set_change_handler(move |selection, cx| {
            let Some(stage) = stage.upgrade() else {
                return;
            };
            stage.update(cx, |stage, cx| {
                stage.instrument_selection_changed(&pane_id, selection, cx);
            });
        });
    }

    pub(in crate::windows::root) fn set_instrument_link_mode(
        &mut self,
        mode: InstrumentLinkMode,
        _cx: &mut App,
    ) {
        self.instrument_link_mode = mode;
    }

    /// Builds the copy of this pane that follows the cursor during a drag.
    pub(in crate::windows::root) fn detached_for_drag(
        &self,
        stage: Entity<Stage>,
        cx: &mut App,
    ) -> Self {
        Self::build(
            self.id.clone(),
            self.widget.clone(),
            stage,
            self.instrument_link_mode,
            true,
            false,
            cx,
        )
    }
}

/// The element id of the pane `node_id` stands for.
fn pane_element_id(stage_id: &ElementId, node_id: NodeId) -> ElementId {
    ElementId::NamedInteger(SharedString::from(stage_id.to_string()), node_id.0)
}

/// Creates the widget a pane shows, from the widget its layout stored.
///
/// The stored widget is built when this build still knows it, and the widget a pane
/// falls back to otherwise. A stored config that cannot be read is not an error —
/// it decodes into the widget's defaults, and the repair it reports is what the
/// workspaces write back.
fn create_widget(
    stored: Option<&WorkspaceWidget>,
    pane_id: &ElementId,
    stage: &WeakEntity<Stage>,
    cx: &mut App,
) -> (Entity<AnyWidget>, bool) {
    if let Some(stored) = stored {
        let config = stored.config.clone();
        let change = config_change(pane_id, stage);
        match WidgetRegistry::create_global(stored.id.as_str(), config.clone(), change, cx) {
            Ok(created) => {
                let repaired = config.is_some_and(|stored_config| {
                    let repaired = created.read(cx).config(cx) != Some(stored_config);
                    if repaired {
                        log::warn!(
                            "widget `{}` had a configuration this build cannot read; \
                             it was reset to its defaults",
                            stored.id
                        );
                    }

                    repaired
                });

                return (created, repaired);
            }
            Err(error) => log::error!("failed to restore widget `{}`: {error}", stored.id),
        }
    }

    (default_widget(pane_id, stage, cx), false)
}

/// The widget a pane shows when it has none of its own.
pub(in crate::windows::root) fn default_widget(
    pane_id: &ElementId,
    stage: &WeakEntity<Stage>,
    cx: &mut App,
) -> Entity<AnyWidget> {
    WidgetRegistry::create_default_global(
        app_config::workspace::DEFAULT_WIDGET_ID,
        config_change(pane_id, stage),
        cx,
    )
    .expect("the catalogue registers the widget a pane falls back to")
}

/// Builds the report a widget hosted by the pane `pane_id` publishes its config
/// changes through.
///
/// The report outlives the widget it was built for — a widget can report from a
/// task that settles after its pane has moved on — so it names the pane instead of
/// holding it, defers through the stage, and lets the stage check that the pane is
/// still there. The deferred hop is required rather than defensive: a widget
/// reports from inside its own update, and the stage is very often already inside
/// that update, so calling into it directly would re-enter the entity and panic.
pub(in crate::windows::root) fn config_change(
    pane_id: &ElementId,
    stage: &WeakEntity<Stage>,
) -> WidgetConfigChange {
    let pane_id = pane_id.clone();
    let stage = stage.clone();
    WidgetConfigChange::new(move |cx: &mut App| {
        let pane_id = pane_id.clone();
        let stage = stage.clone();
        cx.defer(move |cx| {
            let Some(stage) = stage.upgrade() else {
                return;
            };
            stage.update(cx, |stage, cx| {
                stage.widget_config_changed(&pane_id, cx);
            });
        });
    })
}

impl RenderOnce for Pane {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            stage,
            widget,
            instrument_link,
            instrument_link_mode,
            dragging,
            close_button_visible,
            // The repair flag is read by the workspaces when they decide whether to
            // write back, never by a frame.
            ..
        } = self;
        // The surface is read every frame, and it reads the state underneath it every
        // frame in turn. That is the whole chain by which a widget's report reaches
        // this pane: a frame is only scheduled for an entity a window has read, and
        // the entity that reports is the widget's own state — so what the pane has
        // to read is the surface that presents it.
        let surface = widget.read(cx).surface().clone();
        let header = header(
            &id,
            stage.upgrade(),
            &widget,
            instrument_link.is_some(),
            instrument_link_mode,
            dragging,
            close_button_visible,
            window,
            cx,
        );

        let theme = cx.theme();

        // The pane owns the inset and the spacing of its children: its padding keeps
        // the body 4px from every edge, whether or not a header is drawn, and its
        // `gap_1` separates the header from the body. The banner's gap above the body
        // is the surface's own.
        div()
            .size_full()
            .overflow_hidden()
            .rounded_sm()
            .bg(theme.roles.surface.canvas.hsla)
            .text_color(theme.roles.fg.default.hsla)
            .border_1()
            .border_color(theme.roles.line.divider.hsla)
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .when_some(header, |pane, header| pane.child(header))
            .child(
                div()
                    .debug_selector(|| "PANE_BODY".into())
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(surface),
            )
            .on_any_mouse_down(move |_, _, cx| {
                if let Some(stage) = stage.upgrade() {
                    stage.update(cx, |stage, cx| stage.pane_clicked(&id, cx));
                }
            })
    }
}

/// Builds one frame's header row, if this pane draws one.
///
/// The widget contributes two groups and the pane fixes everything around them,
/// so the row is assembled here rather than by the widget. It is rebuilt every
/// frame and never cached: a header that skipped a frame would show a control
/// that is no longer true.
///
/// The parameter count is the pane's own contract rather than a lint to
/// suppress: the workspace allows `too_many_arguments` because these row
/// builders take the projection they draw, not a struct they could share.
fn header(
    id: &ElementId,
    stage: Option<Entity<Stage>>,
    widget: &Entity<AnyWidget>,
    supports_instrument_link: bool,
    instrument_link_mode: InstrumentLinkMode,
    dragging: bool,
    close_button_visible: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    if dragging {
        return None;
    }

    let stage = stage?;
    let groups = widget.update(cx, |widget, cx| widget.header(window, cx));
    // The header is the one step of the pane that is lifted off the page: it reads as a title
    // bar over the floor the body sits on (`surface.raised` over `surface.canvas`), inset from
    // the pane's top and sides by the same 4px the body keeps, rounded like it, and wrapped in
    // the 1px `line.divider` border that closes it.
    let header_surface = cx.theme().roles.surface.raised.hsla;
    let header_border = cx.theme().roles.line.divider.hsla;
    let instrument_link = supports_instrument_link.then(|| {
        let pane_id = id.clone();
        let stage = stage.clone();
        div()
            .debug_selector(|| "PANE_CHROME_LINK".into())
            .h_full()
            .w(super::controls::CHROME_LINK_WIDTH)
            .flex_none()
            .overflow_hidden()
            .child(
                InstrumentLinker::new((pane_id.clone(), "ChromeLink"), instrument_link_mode)
                    .on_change(move |mode, _, cx| {
                        stage.update(cx, |stage, cx| {
                            stage.set_instrument_link_mode(&pane_id, mode, cx);
                        });
                    }),
            )
    });

    Some(
        div()
            .id((id.clone(), "PaneHeader"))
            .debug_selector(|| "PANE_HEADER".into())
            .bg(header_surface)
            .border_1()
            .border_color(header_border)
            .rounded_sm()
            .p_1()
            .min_w_0()
            .flex_none()
            .flex()
            .items_center()
            .overflow_hidden()
            .gap_1()
            .children(instrument_link)
            .when(supports_instrument_link, |header| {
                header.child(Separator::vertical().w_0p5().my_1p5())
            })
            .children(groups.left)
            .child(PaneDragRegion::new(
                (id.clone(), "HeaderDrag").into(),
                id.clone(),
                stage.clone(),
            ))
            .children(groups.right)
            .child(Separator::vertical().w_0p5().my_1p5())
            .child(PaneWidgetButton::new(id.clone(), stage.clone()))
            .child(PaneSplitButton::new(id.clone(), stage.clone()))
            .when(close_button_visible, |actions| {
                actions.child(PaneCloseButton::new(id.clone(), stage))
            })
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use app_config::{
        widget::{VersionedWidgetConfig, WidgetConfig, WidgetConfigSchemaVersion},
        workspace::{WorkspaceLayout, WorkspaceWidget},
    };
    use base_ui::style;
    use gpui_kit::{
        AnyView, App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement,
        Modifiers, ParentElement, Render, SharedString, Styled, TestAppContext, VisualTestContext,
        WeakEntity, Window, div, px, size,
    };
    use smol_str::SmolStr;
    use widget::{
        any::AnyWidget,
        category::WidgetCategory,
        config::WidgetConfigChange,
        descriptor::WidgetDescriptor,
        error::LocalizedError,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
        surface::WidgetSurface,
        view::{WidgetHeader, WidgetView},
    };
    use widget_registry::registry::WidgetRegistry;

    use super::{Pane, Stage};
    use crate::windows::root::components::workspace::placeholder::InstrumentLinkMode;

    /// The config the pane's test widget persists, so a test can tell the envelope a
    /// pane stores from the one it was handed.
    #[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
    struct TestConfig {
        /// The value the widget reports now.
        current: u64,
    }

    impl WidgetConfig for TestConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
    }

    /// A widget that persists a config a test can change while the pane is open.
    ///
    /// It reports nothing, so it discards the state it is handed: what a test needs
    /// from it is the config it holds and the surface the pane presents it through.
    struct ConfiguredWidget {
        config: TestConfig,
    }

    impl WidgetView for ConfiguredWidget {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.configured");
        const DISPLAY_NAME: &'static str = "widget.names.indices";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self { config }
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn config(&self) -> Option<&Self::Config> {
            Some(&self.config)
        }
    }

    impl Render for ConfiguredWidget {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            gpui_kit::Empty
        }
    }

    /// The widget a test can change the config of, and the erased instance around it.
    struct ConfiguredHandles {
        entity: Entity<ConfiguredWidget>,
    }

    impl ConfiguredHandles {
        /// Builds the widget the way the registry does: erased, over its own config,
        /// with the surface the pane presents it through.
        fn create(config: TestConfig, cx: &mut App) -> (Self, Entity<AnyWidget>) {
            let status: Entity<WidgetStatus> = cx.new(|_| WidgetStatus::new());
            let entity = cx.new(|_| ConfiguredWidget { config });
            let descriptor = WidgetDescriptor::new(
                ConfiguredWidget::ID,
                ConfiguredWidget::DISPLAY_NAME,
                ConfiguredWidget::CATEGORY,
                TestConfig::SCHEMA_VERSION,
            );
            let instance_id = WidgetInstanceId::new();
            let body: AnyView = entity.clone().into();
            let build_header = widget::any::build_header_of(entity.downgrade());
            let persisted_config = widget::any::persisted_config_of::<ConfiguredWidget>();
            let surface = cx.new(|cx| {
                WidgetSurface::new(
                    status,
                    body,
                    widget::any::has_content_of::<ConfiguredWidget>(),
                    cx,
                )
            });
            let any = cx.new(move |_| {
                AnyWidget::new(
                    descriptor,
                    instance_id,
                    surface,
                    build_header,
                    persisted_config,
                    None,
                )
            });

            (Self { entity }, any)
        }

        /// Reports a different config, the way a user picking something does.
        fn report(&self, current: u64, cx: &mut App) {
            self.entity.update(cx, |widget, _| {
                widget.config.current = current;
            });
        }
    }

    /// A widget whose body and header are both observable, so the pane's own
    /// behaviour can be told apart from the widget's. It reports no state of its own:
    /// the tests that report one do it through the state the pane presents.
    struct TestWidget {
        instance_id: WidgetInstanceId,
        content: Rc<Cell<bool>>,
        header_calls: Rc<Cell<usize>>,
    }

    impl WidgetView for TestWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("kytrix.test.pane-widget");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self {
                instance_id,
                content: Rc::new(Cell::new(false)),
                header_calls: Rc::new(Cell::new(0)),
            }
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            self.content.get()
        }

        fn header(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> WidgetHeader {
            self.header_calls.set(self.header_calls.get() + 1);

            WidgetHeader::empty().right(
                div()
                    .debug_selector(|| "TEST_HEADER_RIGHT".into())
                    .h_4()
                    .w_6(),
            )
        }
    }

    impl Render for TestWidget {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| "TEST_WIDGET_BODY".into())
                .size_full()
        }
    }

    /// The widget plus the state a test drives it with.
    struct TestWidgetHandles {
        entity: Entity<TestWidget>,
        status: Entity<WidgetStatus>,
        /// The surface the framework would have built around it, built here so the
        /// pane can present this widget the way it presents a registered one.
        surface: Entity<WidgetSurface>,
        content: Rc<Cell<bool>>,
        header_calls: Rc<Cell<usize>>,
    }

    impl TestWidgetHandles {
        /// Creates a widget the way the registry does: state first, then the surface
        /// around it, then one load.
        fn create(cx: &mut App) -> Self {
            let status: Entity<WidgetStatus> = cx.new(|_| WidgetStatus::new());
            let entity = cx.new({
                let status = status.clone();
                move |cx| {
                    let weak: WeakEntity<TestWidget> = cx.weak_entity();
                    let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                        let _ = weak.update(cx, |widget, cx| widget.load(cx));
                    });

                    TestWidget::new(
                        WidgetInstanceId::new(),
                        (),
                        status,
                        |_| {},
                        WidgetConfigChange::default(),
                        recovery,
                        cx,
                    )
                }
            });
            let body: AnyView = entity.clone().into();
            let surface = cx.new(|cx| {
                WidgetSurface::new(
                    status.clone(),
                    body,
                    widget::any::has_content_of::<TestWidget>(),
                    cx,
                )
            });
            entity.update(cx, |widget, cx| widget.load(cx));

            let widget = entity.read(cx);
            Self {
                entity: entity.clone(),
                status,
                surface,
                content: widget.content.clone(),
                header_calls: widget.header_calls.clone(),
            }
        }

        /// Returns a recovery a test can attach to a failure it reports.
        fn recovery(&self, runs: &Rc<Cell<usize>>) -> WidgetAction {
            let runs = runs.clone();
            WidgetAction::new("common.retry", move |_| runs.set(runs.get() + 1))
        }
        /// Erases the widget the way the registry does.
        fn any_widget(&self, cx: &mut App) -> Entity<AnyWidget> {
            let descriptor = WidgetDescriptor::new(
                TestWidget::ID,
                TestWidget::DISPLAY_NAME,
                TestWidget::CATEGORY,
                WidgetConfigSchemaVersion::V1,
            );
            let surface = self.surface.clone();
            let instance_id = self.entity.read(cx).instance_id;
            let build_header = widget::any::build_header_of(self.entity.downgrade());

            cx.new(move |_| {
                AnyWidget::new(
                    descriptor,
                    instance_id,
                    surface,
                    build_header,
                    widget::any::persisted_config_of::<TestWidget>(),
                    None,
                )
            })
        }

        /// Makes the widget report that it has content, then rebuilds the frame.
        fn show_content(&self, cx: &mut VisualTestContext) {
            self.content.set(true);
            self.force_frame(cx);
        }

        /// Makes the widget report that it has nothing to render.
        fn hide_content(&self, cx: &mut VisualTestContext) {
            self.content.set(false);
            self.force_frame(cx);
        }

        fn force_frame(&self, cx: &mut VisualTestContext) {
            // The pane is the window root, so a refresh is the only way to make
            // it render again without notifying it.
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }

        fn report(
            &self,
            cx: &mut VisualTestContext,
            report: impl FnOnce(&Entity<WidgetStatus>, &mut App),
        ) {
            let status = self.status.clone();
            cx.update(|_, cx| report(&status, cx));
            cx.run_until_parked();
        }
    }

    struct PaneHarness {
        /// Held, not read: a pane whose stage is gone draws no header at all, so
        /// the harness is what keeps the pane's owner alive for a test.
        _stage: Entity<Stage>,
        panes: Vec<Pane>,
    }

    impl Render for PaneHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().children(self.panes.clone())
        }
    }

    fn install(cx: &mut App) {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
        style::theme::install(cx);
    }

    /// Opens one pane hosting `widget`.
    fn open_pane_with(
        cx: &mut TestAppContext,
        widget: Entity<AnyWidget>,
    ) -> (Entity<Stage>, &mut VisualTestContext) {
        let stage =
            cx.update(|cx| cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx)));
        let (_, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, cx| PaneHarness {
                _stage: stage.clone(),
                panes: vec![Pane::build(
                    ElementId::Name("test-pane".into()),
                    widget,
                    stage,
                    InstrumentLinkMode::Unlinked,
                    false,
                    false,
                    cx,
                )],
            }
        });
        cx.simulate_resize(size(px(320.0), px(220.0)));
        cx.run_until_parked();

        (stage, cx)
    }

    /// A pane's widget, created the way the framework creates one when nobody has
    /// chosen: the widget a pane falls back to.
    fn default_widget(cx: &mut App) -> Entity<AnyWidget> {
        WidgetRegistry::create_default_global(
            app_config::workspace::DEFAULT_WIDGET_ID,
            WidgetConfigChange::default(),
            cx,
        )
        .expect("the catalogue should register the widget a pane falls back to")
    }

    /// Opens several panes side by side, so two flag combinations can be
    /// compared in one frame.
    fn open_flagged_panes<'a>(
        cx: &'a mut TestAppContext,
        flags: &[(bool, bool)],
    ) -> &'a mut VisualTestContext {
        let stage =
            cx.update(|cx| cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx)));
        let flags = flags.to_vec();
        let (_, cx) = cx.add_window_view({
            let pane_stage = stage.clone();
            move |_, cx| PaneHarness {
                _stage: pane_stage.clone(),
                panes: flags
                    .into_iter()
                    .enumerate()
                    .map(|(index, (dragging, close_button_visible))| {
                        Pane::build(
                            ElementId::NamedInteger(
                                SharedString::new_static("test-pane"),
                                index as u64,
                            ),
                            default_widget(cx),
                            stage.clone(),
                            InstrumentLinkMode::Unlinked,
                            dragging,
                            close_button_visible,
                            cx,
                        )
                    })
                    .collect(),
            }
        });
        cx.simulate_resize(size(px(640.0), px(220.0)));
        cx.run_until_parked();

        cx
    }

    /// Opens one pane hosting the test widget.
    fn open_pane(
        cx: &mut TestAppContext,
    ) -> (TestWidgetHandles, Entity<Stage>, &mut VisualTestContext) {
        cx.update(install);
        let handles = cx.update(TestWidgetHandles::create);
        let widget = cx.update(|cx| handles.any_widget(cx));
        let (stage, cx) = open_pane_with(cx, widget);

        (handles, stage, cx)
    }

    #[gpui_kit::test]
    fn the_pane_should_show_its_header_and_the_widget_body(cx: &mut TestAppContext) {
        let (handles, _stage, cx) = open_pane(cx);
        handles.show_content(cx);

        assert!(cx.debug_bounds("PANE_HEADER").is_some());
        assert!(cx.debug_bounds("TEST_HEADER_RIGHT").is_some());
        assert!(cx.debug_bounds("TEST_WIDGET_BODY").is_some());
        assert!(cx.debug_bounds("WIDGET_BANNER").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER").is_none());
    }

    /// The pane insets its widget surface on all four sides, whether or not it draws
    /// a header, and the header keeps the same inset on the top and the sides: it is
    /// a rounded bar over the body's floor rather than a strip flush with the pane's
    /// edge. A drag preview has no header at all — but it still keeps the inset, so
    /// the body is never glued to the pane's edge.
    ///
    /// The assertions stay relative on purpose: the inset is the pane's own padding
    /// composed with its border, so this test compares the header's box with the
    /// body's instead of deriving pixels.
    ///
    /// Whether a close button is *visible* is a separate question — it says the
    /// pane can be closed at all, because it is one of several. Collapsing the two
    /// makes a lone pane (which shows no close button) pull its body up under a
    /// header that is still there, so the two are compared directly.
    #[gpui_kit::test]
    fn the_body_should_keep_its_inset_with_and_without_a_header(cx: &mut TestAppContext) {
        cx.update(install);

        let [bare, headered, closable] =
            [(true, false), (false, false), (false, true)].map(|flags| {
                let cx = open_flagged_panes(cx, &[flags]);

                (
                    cx.debug_bounds("PANE_BODY")
                        .expect("every pane draws a body"),
                    cx.debug_bounds("PANE_HEADER"),
                    cx.debug_bounds("WIDGET_SURFACE")
                        .expect("every pane draws its widget surface"),
                )
            });

        assert_eq!(bare.1, None, "a drag preview draws no header");
        assert_eq!(
            headered.1.map(|header| header.size.height),
            closable.1.map(|header| header.size.height),
            "a close button is chrome inside the header, not something that \
             changes the header's height"
        );
        assert_eq!(
            headered.0.top(),
            closable.0.top(),
            "showing a close button must not move the body: the pane does not \
             make room for a header because it happens to be closable"
        );

        let header = headered.1.expect("the headered pane draws a header");
        assert!(
            headered.0.top() > header.bottom(),
            "the body starts below the header's own bottom edge instead of being \
             glued to it: body top {:?}, header bottom {:?}",
            headered.0.top(),
            header.bottom()
        );
        assert_eq!(
            header.left(),
            headered.2.left(),
            "the header keeps the same inset on the left as the body"
        );
        assert_eq!(
            header.right(),
            headered.2.right(),
            "the header keeps the same inset on the right as the body"
        );
        // The pane sits at the window origin in this harness, so a body without a
        // header starts exactly where the header of a headered pane starts: both are
        // inset from the pane's own edge, and the header is not glued to it.
        assert_eq!(
            header.top(),
            bare.0.top(),
            "the header keeps the same inset from the pane's top edge as a body \
             without a header does"
        );
        assert_eq!(
            headered.2.top(),
            headered.0.top(),
            "the surface fills the body box from its top"
        );
        assert!(
            headered.0.top() > bare.0.top(),
            "a header pushes the body down: body top with a header {:?} exceeds \
             body top without one {:?}",
            headered.0.top(),
            bare.0.top()
        );

        // The pane sits at the window origin in this harness, so its own border is
        // the same on the top and the left; a drag preview keeps the inset on both.
        assert_eq!(
            bare.0.top(),
            bare.0.left(),
            "a drag preview draws no header, but the body keeps the same inset at \
             its top as at its left"
        );
    }

    #[gpui_kit::test]
    fn the_pane_should_ask_the_widget_for_its_header_once_per_frame(cx: &mut TestAppContext) {
        let (handles, _stage, cx) = open_pane(cx);
        let before = handles.header_calls.get();

        handles.show_content(cx);

        assert_eq!(
            handles.header_calls.get(),
            before + 1,
            "one frame must build the header exactly once"
        );
    }

    #[gpui_kit::test]
    fn a_widget_with_nothing_to_render_should_get_a_placeholder_instead_of_a_body(
        cx: &mut TestAppContext,
    ) {
        let (handles, _stage, cx) = open_pane(cx);
        handles.report(cx, |status, cx| status.report(WidgetState::Loading, cx));

        let placeholder = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("a widget without content should get a placeholder");
        let header = cx
            .debug_bounds("PANE_HEADER")
            .expect("the pane still owns the header");
        let body = cx
            .debug_bounds("PANE_BODY")
            .expect("the pane body should render");

        assert!(placeholder.bottom() <= body.bottom());
        assert!(header.bottom() <= body.top());
        assert!(cx.debug_bounds("TEST_WIDGET_BODY").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_some());
    }

    #[gpui_kit::test]
    fn a_message_with_content_should_use_a_banner_and_a_message_without_it_a_placeholder(
        cx: &mut TestAppContext,
    ) {
        let (handles, _stage, cx) = open_pane(cx);
        handles.show_content(cx);
        let runs = Rc::new(Cell::new(0));
        let recovery = handles.recovery(&runs);
        handles.report(cx, |status, cx| {
            status.report(
                WidgetState::failed(Arc::new(TestFailure), recovery.clone()),
                cx,
            );
        });

        let banner = cx
            .debug_bounds("WIDGET_BANNER")
            .expect("a failure with content belongs above the content");
        let header = cx
            .debug_bounds("PANE_HEADER")
            .expect("the pane still draws its header");
        let surface = cx
            .debug_bounds("WIDGET_SURFACE")
            .expect("the banner lives inside the widget surface");
        assert_eq!(
            banner.top(),
            surface.top(),
            "the banner is the surface's first child, so it starts at the surface's top"
        );
        assert!(
            banner.top() > header.bottom(),
            "the banner starts below the header's own bottom edge instead of being \
             glued to it: banner top {:?}, header bottom {:?}",
            banner.top(),
            header.bottom()
        );
        // A banner changes the body's box, and a changed box busts the cache on its
        // own; the refresh is kept so this read does not depend on that implicit
        // mechanism.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let body = cx
            .debug_bounds("TEST_WIDGET_BODY")
            .expect("the widget keeps rendering its content");
        // The exact separation is the surface's own: `WidgetSurface::frame` puts a
        // `gap_1` between the banner and the slot, and the slot draws its own border.
        // That geometry is the framework's, covered by its tests; the pane only owes
        // the order.
        assert!(
            body.top() > banner.bottom(),
            "the body starts below the banner: body top {:?}, banner bottom {:?}",
            body.top(),
            banner.bottom()
        );
        assert!(
            cx.debug_bounds("WIDGET_PLACEHOLDER").is_none(),
            "the same message must not be shown twice"
        );

        handles.hide_content(cx);

        assert!(cx.debug_bounds("WIDGET_BANNER").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER").is_some());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_some());
        assert_eq!(runs.get(), 0, "showing a recovery must not run it");
    }

    #[gpui_kit::test]
    fn the_recovery_in_a_placeholder_should_run_the_widgets_action(cx: &mut TestAppContext) {
        let (handles, _stage, cx) = open_pane(cx);
        let runs = Rc::new(Cell::new(0));
        let recovery = handles.recovery(&runs);
        handles.report(cx, |status, cx| {
            status.report(WidgetState::failed(Arc::new(TestFailure), recovery), cx);
        });

        let action = cx
            .debug_bounds("WIDGET_PLACEHOLDER_ACTION")
            .expect("a failure without content should offer its recovery");
        cx.simulate_click(action.center(), Modifiers::none());

        assert_eq!(runs.get(), 1);
    }

    /// A pane nobody has chosen a widget for shows the widget a pane falls back to,
    /// and that widget's body reaches the pane's slot like any other: the hint is
    /// what a fresh pane opens on.
    #[gpui_kit::test]
    fn a_pane_should_show_the_widget_it_falls_back_to(cx: &mut TestAppContext) {
        cx.update(install);
        let widget = cx.update(default_widget);
        let (_stage, cx) = open_pane_with(cx, widget);

        assert!(cx.debug_bounds("PANE_HEADER").is_some());
        assert!(cx.debug_bounds("TEST_EMPTY_WIDGET").is_some());
        assert!(cx.debug_bounds("BASE_UI_MARKDOWN").is_some());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER").is_none());
        assert!(cx.debug_bounds("WIDGET_BANNER").is_none());
    }

    #[gpui_kit::test]
    fn a_drag_preview_should_drop_the_header_and_keep_the_widget(cx: &mut TestAppContext) {
        cx.update(install);
        cx.update(|cx| {
            let handles = TestWidgetHandles::create(cx);
            let widget = handles.any_widget(cx);
            let stage = cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx));
            let pane = Pane::build(
                ElementId::Name("test-pane".into()),
                widget.clone(),
                stage.clone(),
                InstrumentLinkMode::Unlinked,
                false,
                true,
                cx,
            );
            let preview = pane.detached_for_drag(stage, cx);

            assert!(!pane.dragging);
            assert!(pane.close_button_visible);
            assert!(preview.dragging);
            assert!(!preview.close_button_visible);
            assert_eq!(preview.widget.entity_id(), widget.entity_id());
        });
    }

    /// A strongly typed failure the test can report.
    struct TestFailure;

    impl LocalizedError for TestFailure {
        fn localized_message(&self, _cx: &App) -> gpui_kit::SharedString {
            "route unavailable".into()
        }
    }

    /// A pane stores what its widget reports now, not the envelope the widget was
    /// created from.
    #[gpui_kit::test]
    fn a_snapshot_should_carry_the_config_the_widget_reports_now(cx: &mut TestAppContext) {
        cx.update(install);
        cx.update(|cx| {
            let (handles, widget) = ConfiguredHandles::create(TestConfig { current: 1 }, cx);
            let stage = cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx));
            let pane = Pane::build(
                ElementId::Name("test-pane".into()),
                widget,
                stage,
                InstrumentLinkMode::Unlinked,
                false,
                false,
                cx,
            );

            assert_eq!(
                pane.snapshot(cx).config,
                VersionedWidgetConfig::encode(&TestConfig { current: 1 }).ok(),
                "a pane is written back with the config it was restored from until the \
                 widget changes it"
            );

            // The widget starts reporting something else, which is what the user
            // choosing in a header does.
            handles.report(2, cx);

            assert_eq!(
                pane.snapshot(cx).config,
                VersionedWidgetConfig::encode(&TestConfig { current: 2 }).ok(),
                "the snapshot is the widget's own answer, not the envelope it arrived with"
            );
        });
    }

    /// A config this build cannot read comes back as the widget's defaults, and the stage
    /// reports that the stored one was replaced so the workspaces write the repair out.
    #[gpui_kit::test]
    fn restoring_an_unreadable_config_should_replace_it_and_report_the_repair(
        cx: &mut TestAppContext,
    ) {
        cx.update(install);
        cx.update(|cx| {
            let mut registry = widget_registry::registry::WidgetRegistry::default();
            registry
                .register(widget_registry::registry::WidgetRegistration::for_view::<
                    ConfiguredWidget,
                >())
                .expect("the configured widget should register");
            cx.set_global(registry);

            let unreadable: VersionedWidgetConfig = serde_json::from_value(serde_json::json!({
                "schema_version": TestConfig::SCHEMA_VERSION.get(),
                "payload": "{",
            }))
            .expect("the envelope itself is well formed");
            let layout = WorkspaceLayout::Pane {
                widget: WorkspaceWidget {
                    id: SmolStr::new_static("kytrix.test.configured"),
                    config: Some(unreadable),
                },
            };
            let stage = cx.new(|cx| Stage::from_layout(1, &layout, cx));

            assert!(
                stage.read(cx).has_repaired_widget_config(),
                "the stage reports the repair so the workspaces write it out"
            );
            assert_eq!(
                stored_config(&stage.read(cx).layout(cx)),
                VersionedWidgetConfig::encode(&TestConfig { current: 0 }).ok(),
                "the widget's defaults are what the pane now stores"
            );
        });
    }

    /// The config envelope the one pane of `layout` stores.
    fn stored_config(layout: &WorkspaceLayout) -> Option<VersionedWidgetConfig> {
        let WorkspaceLayout::Pane { widget } = layout else {
            panic!("the test layout is a single pane");
        };

        widget.config.clone()
    }
}
