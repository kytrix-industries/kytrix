use std::{rc::Rc, sync::Arc, time::Duration};

use bytes::Bytes;
use i18n::{language::codes::LanguageCode, translate::Translate, translator::types::I18n};
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use market_participants_connector::connector::MarketParticipantsConnector;
use smol_str::SmolStr;
use ui_core::{
    a11y::A11yPatch,
    app::App,
    error::EngineError,
    event::{Action, ActionSource, Event},
    math::{LogicalPx, LogicalRect},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::SignalScope,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::GeometryId,
        primitive::{ClipOp, GradientKind, Primitive, QuadPaint},
    },
    test_support::{
        app::TestApp,
        diagnostics::SignalSlotStats,
        record::{PrimitiveKind, PrimitiveRecord},
    },
    window::{FrameIndex, WindowId},
};
use ui_kit::theme::handle::ThemeHandle;

use crate::{
    any::AnyWidget,
    cx::WidgetCx,
    error::TranslatedError,
    frame::FrameRouter,
    identity::WidgetInstanceId,
    services::WidgetServices,
    status::{WidgetAction, WidgetState, WidgetStatus},
    surface::WidgetSurface,
    view::WidgetView,
};

const RUNTIME_TURNS: usize = 32;

pub const MESSAGES_EN: &str = r#"
common:
  connecting: Connecting
  loading: Loading
  retry: Retry
sample:
  failure: the fixture failed
test:
  waiting: Waiting for orders
  rich: '<p align="center">Click the <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> button to pick a widget.</p>'
"#;

pub const MESSAGES_ZH: &str = r#"
common:
  connecting: 正在连接
  loading: 正在加载
  retry: 重试
sample:
  failure: 示例失败
test:
  waiting: 正在等待订单
  rich: '<p align="center">点击 <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> 按钮选择一个部件</p>'
"#;

pub struct FixtureFailure;

impl TranslatedError for FixtureFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        i18n::t_smol!(translate, "sample.failure")
    }
}

pub fn fixture_translate(language: LanguageCode) -> Translate {
    let i18n = I18n::builder()
        .locale_yaml(LanguageCode::En, MESSAGES_EN)
        .expect("the English fixture parses")
        .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
        .expect("the Chinese fixture parses")
        .fallback(LanguageCode::En)
        .build()
        .expect("the fixture catalogue builds");

    Translate::new(Rc::new(i18n), language)
}

pub struct WidgetTestApp {
    app: TestApp,
    window: WindowId,
    runtime: tokio::runtime::Runtime,
    router: Rc<FrameRouter>,
    services: Rc<WidgetServices>,
    scope: SignalScope,
    status: WidgetStatus,
}

impl Default for WidgetTestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl WidgetTestApp {
    #[must_use]
    pub fn new() -> Self {
        Self::with_translate(fixture_translate(LanguageCode::En))
    }

    #[must_use]
    pub fn with_translate(translate: Translate) -> Self {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), ui_kit::theme::appearance::Appearance::Light);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the host's runtime builds");
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the host owns the frame hook slot"));
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime.handle().clone(),
                translate,
                theme,
                Rc::clone(&router),
            )
            .build(),
        );
        let scope = app.app_mut().signal_scope();
        let status = WidgetStatus::new(app.app_mut(), scope);

        Self {
            app,
            window,
            runtime,
            router,
            services,
            scope,
            status,
        }
    }

    #[must_use]
    pub fn with_market_participants(
        mut self,
        market_participants: Option<MarketParticipantsConnector>,
    ) -> Self {
        self.services_mut().market_participants = market_participants;
        self
    }

    #[must_use]
    pub fn with_kytrix_service(mut self, kytrix_service: Option<KytrixServiceConnector>) -> Self {
        self.services_mut().kytrix_service = kytrix_service;
        self
    }

    #[must_use]
    pub fn with_open_url(mut self, open_url: impl Fn(&str) + 'static) -> Self {
        self.services_mut().open_url = Rc::new(open_url);
        self
    }

    #[must_use]
    pub fn with_sound(mut self, sound: impl Fn(&'static str) -> Option<Bytes> + 'static) -> Self {
        self.services_mut().sound = Rc::new(sound);
        self
    }

    fn services_mut(&mut self) -> &mut WidgetServices {
        Rc::get_mut(&mut self.services).expect("the host is the only owner of its services")
    }

    #[must_use]
    pub fn app(&self) -> &App {
        self.app.app()
    }

    #[must_use]
    pub fn app_mut(&mut self) -> &mut App {
        self.app.app_mut()
    }

    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    pub fn dispatch_event(&mut self, event: Event) {
        self.app.dispatch_event(event);
    }

    #[must_use]
    pub fn services_rc(&self) -> &Rc<WidgetServices> {
        &self.services
    }

    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.router
    }

    #[must_use]
    pub const fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub const fn status(&self) -> WidgetStatus {
        self.status
    }

    #[must_use]
    pub const fn scope(&self) -> SignalScope {
        self.scope
    }

    pub fn with_cx<R>(&mut self, body: impl FnOnce(&mut WidgetCx<'_>) -> R) -> R {
        let services = Rc::clone(&self.services);
        let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, self.scope);
        body(&mut cx)
    }

    pub fn tick(&mut self) {
        self.router
            .tick(self.app.app_mut())
            .expect("the widget frame routes succeed");
    }

    pub fn drain(&mut self) {
        self.tick();
        self.app.drain().expect("the frame commits");
    }

    pub fn advance(&mut self, by: Duration) -> bool {
        self.app.advance(by);
        self.drain();
        self.app.frame_record().rendered
    }

    #[must_use]
    pub fn frame_index(&self) -> FrameIndex {
        self.app
            .app()
            .window(self.window)
            .expect("the host's window exists")
            .frame_index()
    }

    #[must_use]
    pub fn frame_requests(&self) -> u64 {
        self.app.app().frame_requests()
    }

    #[must_use]
    pub fn signal_slot_stats(&self) -> SignalSlotStats {
        ui_core::test_support::diagnostics::signal_slot_stats(self.app.app())
    }

    #[must_use]
    pub fn rendered(&self) -> bool {
        self.app.frame_record().rendered
    }

    pub fn drain_runtime(&mut self) {
        self.runtime.block_on(tokio::task::yield_now());
    }

    pub fn run_until_idle(&mut self) -> usize {
        let mut turns = 0;
        let mut settled = (self.app.app().frame_requests(), self.app.tasks_run());
        while turns < RUNTIME_TURNS {
            self.drain_runtime();
            self.drain();
            turns += 1;
            let after = (self.app.app().frame_requests(), self.app.tasks_run());
            if turns >= 2 && after == settled {
                break;
            }
            settled = after;
        }
        turns
    }

    #[must_use]
    pub fn tree(&self) -> &NodeTree {
        self.app
            .app()
            .window(self.window)
            .expect("the host's window exists")
            .tree()
    }

    #[must_use]
    pub fn root(&self) -> NodeId {
        self.tree().root()
    }

    pub fn report(&mut self, state: WidgetState) {
        assert!(
            self.status.report(self.app.app_mut(), state),
            "the test's report is a change"
        );
    }

    pub fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        assert!(
            self.app.app_mut().write(&self.services.language, language),
            "the switch is a change"
        );
    }

    pub fn build_surface(&mut self) -> WidgetSurface {
        let status = self.status;
        let services = Rc::clone(&self.services);
        let scope = self.scope;
        let root = self.root();
        let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, scope);
        WidgetSurface::build(&mut cx, root, status).expect("the surface builds")
    }

    pub fn sync_surface(&mut self, surface: &mut WidgetSurface, has_content: bool) {
        let services = Rc::clone(&self.services);
        let scope = self.scope;
        {
            let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, scope);
            surface
                .sync(&mut cx, has_content)
                .expect("the surface syncs");
        }
        self.drain();
    }

    pub fn build_body(&mut self, parent: NodeId) -> NodeId {
        self.with_cx(|cx| {
            NodeBuilder::boxed()
                .id("TEST_BODY")
                .build(cx.tree_mut(), parent)
                .expect("the body parent is alive")
        })
    }

    pub fn placeholder_parent(&mut self) -> NodeId {
        self.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_PLACEHOLDER_HOST")
                .build(cx.tree_mut(), root)
                .expect("the root is alive")
        })
    }

    pub fn mount<W: WidgetView>(&mut self, config: W::Config) -> AnyWidget {
        self.try_mount::<W>(config).expect("the widget mounts")
    }

    pub fn try_mount<W: WidgetView>(
        &mut self,
        config: W::Config,
    ) -> Result<AnyWidget, EngineError> {
        let services = Rc::clone(&self.services);
        let parent = self.root();
        AnyWidget::mount::<W>(
            self.app.app_mut(),
            self.window,
            parent,
            &services,
            WidgetInstanceId::new(),
            config,
            crate::config::WidgetConfigChange::default(),
        )
    }

    pub fn try_mount_in_scope<W: WidgetView>(
        &mut self,
        parent: NodeId,
        scope: SignalScope,
        config: W::Config,
    ) -> Result<AnyWidget, EngineError> {
        let services = Rc::clone(&self.services);
        AnyWidget::mount_in_scope::<W>(
            self.app.app_mut(),
            self.window,
            parent,
            &services,
            WidgetInstanceId::new(),
            config,
            crate::config::WidgetConfigChange::default(),
            scope,
        )
    }

    #[must_use]
    pub fn visible(&self, node: NodeId) -> bool {
        self.tree()
            .get(node)
            .expect("the node exists")
            .style_patch()
            .visible
            .unwrap_or(true)
    }

    #[must_use]
    pub fn text_of(&self, node: NodeId) -> String {
        let mut texts = Vec::new();
        self.collect_text(node, &mut texts);
        texts.join("\n")
    }

    #[must_use]
    pub fn has_text(&self, node: NodeId, text: &str) -> bool {
        let mut texts = Vec::new();
        self.collect_text(node, &mut texts);
        texts.iter().any(|drawn| drawn == text)
    }

    #[must_use]
    pub fn has_canvas(&self, node: NodeId) -> bool {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if matches!(tree.get(current).expect("node").kind(), NodeKind::Canvas(_)) {
                return true;
            }
            stack.extend(tree.get(current).expect("node").children().iter().copied());
        }
        false
    }

    #[must_use]
    pub fn paint_entries(&self, node: NodeId) -> Vec<PrimitiveRecord> {
        self.app
            .rendered()
            .map(|frame| {
                frame
                    .primitives
                    .iter()
                    .filter(|entry| entry.node == node)
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn canvas_primitives(&self, node: NodeId) -> Vec<CanvasPrimitive> {
        self.app
            .rendered()
            .map(|frame| {
                frame
                    .canvases
                    .iter()
                    .filter(|entry| entry.node == node)
                    .flat_map(|entry| entry.canvas.primitives().iter().map(canvas_primitive))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn canvas_handle(&self, node: NodeId) -> Option<Arc<Canvas>> {
        let NodeKind::Canvas(canvas) = self.tree().get(node).ok()?.kind() else {
            return None;
        };
        Some(Arc::clone(canvas))
    }

    pub fn click_recovery(&mut self, parent: NodeId) {
        let node = self
            .find_by_debug_prefix(parent, "widget.placeholder.action.")
            .or_else(|| self.find_by_debug_prefix(parent, "widget.banner.action."))
            .expect("the framework built a recovery control");
        let debug_id = self
            .tree()
            .debug_id(node)
            .expect("the control carries its debug id")
            .to_owned();
        let key = debug_id
            .strip_suffix(":interactive")
            .expect("a kit control's node id ends with its interactive suffix");
        let action = Action::Custom(smol_str::format_smolstr!("{key}.click"));
        let window = self.window;

        assert!(
            self.app
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the recovery control is registered"
        );
    }

    #[must_use]
    pub fn find_by_debug_prefix(&self, parent: NodeId, prefix: &str) -> Option<NodeId> {
        let tree = self.tree();
        let mut stack = vec![parent];
        while let Some(current) = stack.pop() {
            if tree
                .debug_id(current)
                .is_some_and(|id| id.starts_with(prefix))
            {
                return Some(current);
            }
            stack.extend(
                tree.get(current)
                    .expect("node")
                    .children()
                    .iter()
                    .rev()
                    .copied(),
            );
        }
        None
    }

    pub fn set_a11y_patch(&mut self, node: NodeId, patch: A11yPatch) {
        self.app
            .app_mut()
            .set_a11y_patch(self.window, node, patch)
            .expect("the node is alive");
    }

    pub fn enable_a11y_capture(&mut self) {
        self.app.enable_a11y_capture();
    }

    #[must_use]
    pub fn a11y_label(&mut self, node: NodeId) -> Option<String> {
        let _ = self.app.a11y_oracle_diff();
        self.app.assert_a11y_contract();
        self.app
            .a11y()?
            .node_for(node)
            .and_then(|entry| entry.label.as_ref().map(|label| label.text().to_owned()))
    }

    #[must_use]
    pub fn a11y_hidden(&self, node: NodeId) -> bool {
        let snapshot = self
            .app
            .a11y()
            .expect("a11y capture is on and a frame was drained");
        snapshot
            .node_for(node)
            .unwrap_or_else(|| panic!("{node:?} was not delivered in this a11y frame"))
            .hidden
    }

    #[must_use]
    pub fn has_inline_boxes(&self, node: NodeId) -> bool {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("node");
            if let NodeKind::Text(content) = target.kind()
                && !content.inline_boxes.is_empty()
            {
                return true;
            }
            stack.extend(target.children().iter().copied());
        }
        false
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        let tree = self.tree();
        let mut count = 0;
        let mut stack = vec![tree.root()];
        while let Some(current) = stack.pop() {
            count += 1;
            stack.extend(tree.get(current).expect("node").children().iter().copied());
        }
        count
    }

    fn collect_text(&self, node: NodeId, texts: &mut Vec<String>) {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("node");
            if let NodeKind::Text(content) = target.kind() {
                texts.push(content.text.to_string());
            }
            stack.extend(target.children().iter().rev().copied());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanvasPrimitive {
    pub kind: PrimitiveKind,
    pub bounds: Option<LogicalRect>,
    pub corner_radius: Option<[LogicalPx; 4]>,
    pub color: Option<Color>,
    pub border: Option<(LogicalPx, Color)>,
    pub gradient: Option<(Color, GradientKind)>,
    pub glyphs: usize,
    pub geometry: Option<GeometryId>,
}

fn canvas_primitive(primitive: &Primitive) -> CanvasPrimitive {
    match primitive {
        Primitive::Quad(quad) => {
            let (color, border, gradient) = match &quad.paint {
                QuadPaint::Solid { fill, border } => (
                    Some(*fill),
                    border.map(|painted| (painted.width, painted.color)),
                    None,
                ),
                QuadPaint::Gradient(painted) => {
                    (Some(painted.from), None, Some((painted.to, painted.kind)))
                }
            };
            CanvasPrimitive {
                kind: PrimitiveKind::Quad,
                bounds: Some(quad.bounds),
                corner_radius: Some(quad.corner_radius),
                color,
                border,
                gradient,
                glyphs: 0,
                geometry: None,
            }
        }
        Primitive::GlyphRun(run) => {
            let mut bounds = LogicalRect::default();
            for glyph in &run.glyphs {
                bounds = bounds.union(LogicalRect::from_origin_size(glyph.position, glyph.size));
            }
            CanvasPrimitive {
                kind: PrimitiveKind::GlyphRun,
                bounds: Some(bounds),
                corner_radius: None,
                color: Some(run.color),
                border: None,
                gradient: None,
                glyphs: run.glyphs.len(),
                geometry: None,
            }
        }
        Primitive::Image(image) => CanvasPrimitive {
            kind: PrimitiveKind::Image,
            bounds: Some(image.bounds),
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Path(path) => CanvasPrimitive {
            kind: PrimitiveKind::Path,
            bounds: Some(path.bounds),
            corner_radius: None,
            color: Some(path.fill),
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: Some(path.geometry),
        },
        Primitive::Shadow(shadow) => CanvasPrimitive {
            kind: PrimitiveKind::Shadow,
            bounds: Some(shadow.rect),
            corner_radius: Some(shadow.corner_radius),
            color: Some(shadow.color),
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Clip(ClipOp::Push(clip)) => CanvasPrimitive {
            kind: PrimitiveKind::ClipPush,
            bounds: Some(clip.rect),
            corner_radius: Some(clip.corner_radius),
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Clip(ClipOp::Pop) => CanvasPrimitive {
            kind: PrimitiveKind::ClipPop,
            bounds: None,
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Geometry(draw) => CanvasPrimitive {
            kind: PrimitiveKind::Geometry,
            bounds: None,
            corner_radius: None,
            color: Some(draw.color),
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: Some(draw.id),
        },
        Primitive::Canvas(draw) => CanvasPrimitive {
            kind: PrimitiveKind::Canvas,
            bounds: Some(draw.bounds().translate(draw.offset)),
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Text(_) => CanvasPrimitive {
            kind: PrimitiveKind::Text,
            bounds: None,
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
    }
}

pub struct WaitingWidget {
    status: WidgetStatus,
    writes: Rc<std::cell::Cell<usize>>,
    loads: Rc<std::cell::Cell<usize>>,
    released: Rc<std::cell::Cell<bool>>,
}

impl WaitingWidget {
    #[must_use]
    pub fn writes(&self) -> usize {
        self.writes.get()
    }

    #[must_use]
    pub fn loads(&self) -> usize {
        self.loads.get()
    }

    #[must_use]
    pub fn released(&self) -> bool {
        self.released.get()
    }
}

impl WidgetView for WaitingWidget {
    type Config = ();

    const ID: crate::identity::WidgetId = crate::identity::WidgetId::new_static("test.waiting");
    const DISPLAY_NAME: &'static str = "test.waiting";
    const CATEGORY: crate::category::WidgetCategory = crate::category::WidgetCategory::Market;

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: crate::view::ReloadHandle,
        _config_change: crate::config::WidgetConfigChange,
        _recovery: WidgetAction,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<Self, ui_core::error::EngineError> {
        Ok(Self {
            status,
            writes: Rc::new(std::cell::Cell::new(0)),
            loads: Rc::new(std::cell::Cell::new(0)),
            released: Rc::new(std::cell::Cell::new(false)),
        })
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        self.loads.set(self.loads.get() + 1);
        self.writes.set(self.writes.get() + 1);
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(crate::status::Notice::message("test.waiting")),
        );
    }

    fn has_content(&self) -> bool {
        false
    }

    fn body(
        &mut self,
        _parent: NodeId,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<(), ui_core::error::EngineError> {
        Ok(())
    }

    fn release(&mut self, _cx: &mut WidgetCx<'_>) {
        self.released.set(true);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config_provider::notification::ConfigNotification;
    use bytes::Bytes;
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
        runtime::MarketParticipantNetworkSnapshot,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        app::App,
        error::EngineError,
        math::{LogicalPoint, LogicalRect, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
            primitive::{ClipRect, Path, Quad, QuadPaint},
            style::StylePatch,
        },
        test_support::record::PrimitiveKind,
        text::{TextContent, TextStyle},
    };
    use ui_kit::theme::{appearance::Appearance, role::Role};

    use super::{CanvasPrimitive, FixtureFailure, WaitingWidget, WidgetTestApp};
    use crate::{
        any::AnyWidget,
        category::WidgetCategory,
        config::WidgetConfigChange,
        cx::WidgetCx,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetState, WidgetStatus},
        view::{ReloadHandle, WidgetView},
    };

    #[test]
    fn a_minimal_widget_runs_the_whole_lifecycle() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<WaitingWidget>(());

        assert_eq!(
            harness.tree().debug_id(widget.surface()),
            Some("WIDGET_SURFACE")
        );
        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(crate::status::Notice::message("test.waiting"))
        );

        harness.drain();
        let surface_node = widget.surface();
        let placeholder = harness
            .find_by_debug_prefix(surface_node, "WIDGET_PLACEHOLDER")
            .expect("the placeholder exists");
        assert!(harness.visible(placeholder));
        assert!(harness.text_of(placeholder).contains("Waiting for orders"));

        assert!(
            widget
                .status()
                .report(harness.app_mut(), WidgetState::Loading)
        );
        harness.drain();
        assert!(harness.has_canvas(placeholder), "a spinner stands in");
        assert!(
            widget
                .status()
                .report(harness.app_mut(), WidgetState::Ready)
        );

        harness.switch_language(LanguageCode::ZhCn);
        assert!(widget.status().report(
            harness.app_mut(),
            WidgetState::waiting(crate::status::Notice::message("test.waiting"))
        ));
        harness.drain();
        assert!(
            harness.text_of(placeholder).contains("正在等待订单"),
            "the stand-in follows the language: {}",
            harness.text_of(placeholder)
        );

        assert!(widget.status().report(
            harness.app_mut(),
            WidgetState::failed(
                std::sync::Arc::new(FixtureFailure),
                WidgetAction::new("common.retry", |_| {}),
            )
        ));
        harness.drain();
        assert!(
            harness.text_of(placeholder).contains("示例失败"),
            "the failure is drawn in the language showing now: {}",
            harness.text_of(placeholder)
        );
        assert!(
            !harness.visible(
                harness
                    .find_by_debug_prefix(surface_node, "WIDGET_CONTENT")
                    .expect("the content slot exists")
            ),
            "a failed widget with no content draws the failure, not the content"
        );

        assert_eq!(harness.router().len(), 1);
        let released = widget.release(harness.app_mut());
        assert!(released > 0, "the scope owned the widget's signals");
        assert_eq!(harness.router().len(), 0, "the frame route is unregistered");
        assert!(widget.is_released());
        assert!(
            harness.tree().get(widget.surface()).is_err(),
            "the nodes are gone"
        );
        assert!(widget.config().is_none());

        assert_eq!(widget.release(harness.app_mut()), 0);
    }

    #[test]
    fn a_released_widget_leaves_no_frame_route() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<WaitingWidget>(());
        assert_eq!(harness.router().len(), 1);

        widget.release(harness.app_mut());

        assert!(harness.router().is_empty());
        harness.drain();
    }

    #[test]
    fn releasing_a_widget_reclaims_every_value_slot() {
        let mut harness = WidgetTestApp::new();
        let baseline = harness.signal_slot_stats();

        let widget = harness.mount::<WaitingWidget>(());
        let mounted = harness.signal_slot_stats();
        assert!(
            mounted.live > baseline.live,
            "the mounted widget mints its own values: {mounted:?}"
        );

        widget.release(harness.app_mut());
        let released = harness.signal_slot_stats();
        assert_eq!(
            released.live, baseline.live,
            "release returns every value the mounted widget minted: {released:?}"
        );

        for _ in 0..8 {
            let widget = harness.mount::<WaitingWidget>(());
            widget.release(harness.app_mut());
        }
        assert_eq!(
            harness.signal_slot_stats(),
            released,
            "eight more mount/release rounds reuse the slots instead of growing the table"
        );
    }

    #[test]
    fn two_widget_instances_live_side_by_side() {
        let mut harness = WidgetTestApp::new();
        let first = harness.mount::<WaitingWidget>(());
        let second = harness.mount::<WaitingWidget>(());
        harness.drain();

        assert_ne!(
            first.scope().index(),
            second.scope().index(),
            "each mount mints its own signal scope"
        );
        assert_ne!(first.surface(), second.surface());
        assert_eq!(harness.router().len(), 2);
        assert!(harness.tree().get(first.surface()).is_ok());
        assert!(harness.tree().get(second.surface()).is_ok());

        let released = first.release(harness.app_mut());
        assert!(released > 0);
        assert!(
            harness.tree().get(first.surface()).is_err(),
            "the closed pane's nodes are gone"
        );
        assert!(
            harness.tree().get(second.surface()).is_ok(),
            "the other pane is untouched"
        );
        assert_eq!(harness.router().len(), 1);
        assert!(!second.is_released());

        second.release(harness.app_mut());
        assert!(harness.tree().get(second.surface()).is_err());
    }

    #[test]
    fn a_recovery_action_offers_the_reload_path() {
        let mut harness = WidgetTestApp::new();
        let retries = Rc::new(std::cell::Cell::new(0));
        let counted = retries.clone();
        let action = WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1));
        let state = WidgetState::failed(Arc::new(FixtureFailure), action);

        harness.report(state);
        let mut surface = harness.build_surface();
        harness.sync_surface(&mut surface, false);
        let placeholder = harness
            .find_by_debug_prefix(surface.root(), "WIDGET_PLACEHOLDER")
            .expect("the placeholder stands in for the missing content");
        harness.click_recovery(placeholder);

        assert_eq!(retries.get(), 1);
    }

    #[test]
    fn any_widget_answers_through_the_vtable_only() {
        let mut harness = WidgetTestApp::new();
        let widget: AnyWidget = harness.mount::<WaitingWidget>(());

        assert_eq!(widget.display_name(), "test.waiting");
        assert!(!widget.status().is_failed(harness.app()));
        assert!(widget.config().is_none());
        assert!(widget.instrument_link().is_none());
        assert!(!widget.is_released());
    }

    #[test]
    fn the_default_host_has_no_directory_service_or_audio() {
        let harness = WidgetTestApp::new();

        assert!(harness.services().market_participants().is_none());
        assert!(harness.services().kytrix_service().is_none());
        assert_eq!(
            (harness.services().sound)("sounds/news_notification.wav"),
            None,
            "a host without audio answers None instead of failing"
        );
    }

    #[test]
    fn a_test_injects_the_application_collaborators_it_exercises() {
        let opened: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&opened);
        let harness = WidgetTestApp::new()
            .with_open_url(move |url| recorded.borrow_mut().push(url.to_owned()))
            .with_sound(|_| Some(Bytes::from_static(b"clip")));

        (harness.services().open_url)("https://example.com/news");
        assert_eq!(
            opened.borrow().as_slice(),
            ["https://example.com/news"],
            "the injected command is the one the widget calls"
        );
        assert_eq!(
            (harness.services().sound)("sounds/news_notification.wav"),
            Some(Bytes::from_static(b"clip")),
            "the injected audio source is the one the widget hears"
        );
    }

    #[test]
    fn a_headless_market_participants_directory_can_be_injected() {
        let harness = WidgetTestApp::new();
        let runtime = harness.services().runtime.clone();
        let notification =
            ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::default()));
        let participants = MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(runtime),
        )
        .expect("the headless directory builds");

        let harness = harness
            .with_market_participants(Some(participants))
            .with_kytrix_service(None);

        assert!(
            harness.services().market_participants().is_some(),
            "the widget sees the directory the test handed in"
        );
        assert!(harness.services().kytrix_service().is_none());
    }

    #[test]
    fn drain_runtime_advances_the_services_runtime() {
        let mut harness = WidgetTestApp::new();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        harness.services().runtime.spawn(async move {
            flag.store(true, Ordering::Relaxed);
        });

        assert!(
            !ran.load(Ordering::Relaxed),
            "a current-thread runtime runs nothing until it is driven"
        );
        harness.drain_runtime();
        assert!(
            ran.load(Ordering::Relaxed),
            "one turn polled the ready task"
        );
    }

    #[test]
    fn run_until_idle_drives_a_chain_of_service_work() {
        let mut harness = WidgetTestApp::new();
        let received = Arc::new(AtomicUsize::new(0));
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u32>();
        harness.services().runtime.spawn(async move {
            let _ = sender.send(1);
        });
        let count = Arc::clone(&received);
        harness.services().runtime.spawn(async move {
            while receiver.recv().await.is_some() {
                count.fetch_add(1, Ordering::Relaxed);
            }
        });

        let turns = harness.run_until_idle();

        assert_eq!(
            received.load(Ordering::Relaxed),
            1,
            "the second task received the message the first sent"
        );
        assert!(
            turns >= 2,
            "the host settled instead of returning at once: {turns}"
        );
        assert!(
            turns < 32,
            "the host did not spend its whole budget: {turns}"
        );
    }

    #[test]
    fn run_until_idle_presents_the_frame_a_service_asked_for() {
        let mut harness = WidgetTestApp::new();
        let before = harness.frame_requests();
        let handle = harness.app().handle();
        let window = harness.window();
        harness.services().runtime.spawn(async move {
            handle.send(move |app| {
                app.request_repaint(window)
                    .expect("the host's window is alive");
            });
        });

        let turns = harness.run_until_idle();

        assert!(
            harness.frame_requests() > before,
            "the service's frame request was pumped and ran"
        );
        assert!(
            turns >= 2,
            "the request is pumped after a frame and presented on the next turn: {turns}"
        );
    }

    #[test]
    fn advance_dispatches_a_due_timer_without_asking_for_a_frame() {
        let mut harness = WidgetTestApp::new();
        harness.drain();
        let fired = Rc::new(Cell::new(0));
        let counter = Rc::clone(&fired);
        let window = harness.window();
        harness
            .app_mut()
            .after(window, Duration::from_millis(250), move |_| {
                counter.set(counter.get() + 1);
            })
            .expect("the host's window takes the timer");
        let requests = harness.frame_requests();
        let index = harness.frame_index();

        assert!(
            !harness.advance(Duration::from_millis(250)),
            "a callback that writes no render state commits nothing"
        );
        assert_eq!(fired.get(), 1, "the timer fired at its deadline");
        assert_eq!(
            harness.frame_requests(),
            requests,
            "dispatching a due timer asks for no frame (I3)"
        );
        assert_eq!(harness.frame_index(), index, "no frame was committed");
        assert!(!harness.rendered());
    }

    #[test]
    fn advance_does_not_fire_a_timer_before_its_delay() {
        let mut harness = WidgetTestApp::new();
        harness.drain();
        let fired = Rc::new(Cell::new(0));
        let counter = Rc::clone(&fired);
        let window = harness.window();
        harness
            .app_mut()
            .after(window, Duration::from_millis(250), move |_| {
                counter.set(counter.get() + 1);
            })
            .expect("the host's window takes the timer");
        let requests = harness.frame_requests();

        assert!(!harness.advance(Duration::from_millis(125)));
        assert_eq!(fired.get(), 0, "half the delay is not enough");
        assert_eq!(harness.frame_requests(), requests);

        assert!(!harness.advance(Duration::from_millis(125)));
        assert_eq!(
            fired.get(),
            1,
            "the second half reaches the deadline exactly"
        );
        assert_eq!(
            harness.frame_requests(),
            requests,
            "the callback writes no render state"
        );
    }

    #[test]
    fn a_timer_callback_s_write_is_presented_by_the_same_advance() {
        let mut harness = WidgetTestApp::new();
        let countdown = harness.app_mut().signal(0_u32);
        let label = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::text(SmolStr::default(), TextStyle::new(px(12.0)))
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(80.0), px(20.0)))
                .bind_text(move |read| {
                    let value = *read.read(&countdown);
                    format_smolstr!("{value}")
                })
                .build(cx.tree_mut(), root)
                .expect("the countdown label builds")
        });
        harness.drain();
        assert!(
            harness.has_text(label, "0"),
            "the label starts at the value the test wrote"
        );
        let window = harness.window();
        harness
            .app_mut()
            .after(window, Duration::from_millis(250), move |app| {
                assert!(app.write(&countdown, 1), "the countdown moves");
            })
            .expect("the host's window takes the timer");
        let requests = harness.frame_requests();
        let index = harness.frame_index();

        assert!(
            harness.advance(Duration::from_millis(250)),
            "the callback's write asks for a frame and this round presents it"
        );
        assert!(
            harness.has_text(label, "1"),
            "the label draws the state the callback wrote: {}",
            harness.text_of(label)
        );
        assert_eq!(
            harness.frame_requests(),
            requests + 1,
            "the callback's write is the only frame request"
        );
        assert_eq!(harness.frame_index().get(), index.get() + 1);
    }

    const CANVAS_TICKS: u8 = 3;

    const CANVAS_BOX: LogicalRect = LogicalRect::new(px(0.0), px(0.0), px(120.0), px(40.0));

    struct CanvasWidget {
        progress: u8,
        painted: Option<(u8, Appearance)>,
        node: Option<NodeId>,
        mesh: Option<GeometryId>,
    }

    impl WidgetView for CanvasWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("test.canvas");
        const DISPLAY_NAME: &'static str = "test.canvas";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self {
                progress: 0,
                painted: None,
                node: None,
                mesh: None,
            })
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            true
        }

        fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            let theme = cx.theme().clone();
            NodeBuilder::boxed()
                .id("TEST_PLATE")
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(0.0),
                    CANVAS_BOX.size.width,
                    px(10.0),
                ))
                .bind_style(move |read| StylePatch {
                    background: Some(theme.role(read, Role::MarketUp)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), parent)?;
            let node = NodeBuilder::canvas(Canvas::new(CANVAS_BOX))
                .id("TEST_CANVAS")
                .layout_override(CANVAS_BOX)
                .build(cx.tree_mut(), parent)?;
            self.node = Some(node);
            self.mesh = Some(register_triangle(cx.app_mut())?);
            Ok(())
        }

        fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            let appearance = cx.theme().current(cx.app());
            let key = (self.progress, appearance);
            if self.painted == Some(key) {
                return Ok(());
            }
            let (Some(node), Some(mesh)) = (self.node, self.mesh) else {
                return Ok(());
            };
            let theme = cx.theme().clone();
            let fill = theme.theme_for(appearance).role(Role::MarketUp);
            let text_color = theme.theme_for(appearance).role(Role::FgDefault);
            let mut canvas = Canvas::new(CANVAS_BOX);
            assert!(
                canvas.push_clip(ClipRect {
                    rect: CANVAS_BOX,
                    corner_radius: [px(4.0); 4],
                }),
                "the fixture's clip is one rounded layer, under the depth limit"
            );
            canvas.push_quad(Quad {
                bounds: LogicalRect::new(
                    px(0.0),
                    px(0.0),
                    px(10.0 * f32::from(self.progress + 1)),
                    px(20.0),
                ),
                corner_radius: [px(2.0); 4],
                paint: QuadPaint::Solid { fill, border: None },
            });
            canvas.push_path(Path {
                bounds: LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)),
                geometry: mesh,
                fill,
            });
            let mut style = TextStyle::new(px(12.0));
            style.color = text_color;
            assert!(
                canvas.push_text(
                    TextContent::new("42", style),
                    LogicalPoint::new(px(4.0), px(4.0))
                ),
                "the fixture records its text recipe"
            );
            assert!(canvas.pop_clip(), "the fixture closes its clip");
            cx.set_canvas(node, canvas)?;
            self.painted = Some(key);
            if self.progress < CANVAS_TICKS {
                self.progress += 1;
                cx.request_frame()?;
            }
            Ok(())
        }

        fn release(&mut self, cx: &mut WidgetCx<'_>) {
            if let Some(mesh) = self.mesh.take() {
                assert!(
                    cx.app_mut().geometry_mut().release(mesh),
                    "the fixture releases the mesh it registered"
                );
            }
        }
    }

    fn register_triangle(app: &mut App) -> Result<GeometryId, EngineError> {
        let mut vertex_bytes = Vec::with_capacity(36);
        for vertex in [[0.0_f32, 0.0, -1.0], [40.0, 0.0, -1.0], [0.0, 20.0, -1.0]] {
            for component in vertex {
                vertex_bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        app.geometry_mut().insert(GeometryDesc {
            vertex_bytes: Bytes::from(vertex_bytes),
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)),
            pipeline: GeometryPipeline::Ui,
        })
    }

    fn canvas_node(harness: &WidgetTestApp, widget: &AnyWidget) -> NodeId {
        harness
            .find_by_debug_prefix(widget.surface(), "TEST_CANVAS")
            .expect("the fixture built its canvas")
    }

    fn role_color(harness: &WidgetTestApp, appearance: Appearance, role: Role) -> Color {
        harness.services().theme.theme_for(appearance).role(role)
    }

    fn kinds(primitives: &[CanvasPrimitive]) -> Vec<PrimitiveKind> {
        primitives.iter().map(|primitive| primitive.kind).collect()
    }

    #[test]
    fn a_canvas_widget_s_draws_are_readable_primitive_by_primitive() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        assert!(harness.rendered(), "the first frame paints the widget");
        let node = canvas_node(&harness, &widget);

        let own = harness.paint_entries(node);
        assert_eq!(
            own.iter()
                .filter(|entry| entry.kind == PrimitiveKind::Canvas)
                .count(),
            1,
            "the node's own paint is one opaque canvas entry: {own:?}"
        );

        let primitives = harness.canvas_primitives(node);
        assert_eq!(
            kinds(&primitives),
            [
                PrimitiveKind::ClipPush,
                PrimitiveKind::Quad,
                PrimitiveKind::Path,
                PrimitiveKind::GlyphRun,
                PrimitiveKind::ClipPop,
            ],
            "the canvas contents in push order: {primitives:?}"
        );
        let clip = &primitives[0];
        assert_eq!(clip.bounds, Some(CANVAS_BOX));
        assert_eq!(clip.corner_radius, Some([px(4.0); 4]));
        let quad = &primitives[1];
        assert_eq!(
            quad.bounds,
            Some(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(20.0))),
            "the first paint is progress 0"
        );
        assert_eq!(quad.corner_radius, Some([px(2.0); 4]));
        assert_eq!(
            quad.color,
            Some(role_color(&harness, Appearance::Light, Role::MarketUp)),
            "the quad carries the colour the widget wrote"
        );
        let path = &primitives[2];
        assert!(
            path.geometry.is_some(),
            "the path names the geometry it draws: {path:?}"
        );
        let run = &primitives[3];
        assert!(
            run.glyphs > 0,
            "one GlyphRun entry per text run, with its placed glyphs: {run:?}"
        );
        assert_eq!(
            run.color,
            Some(role_color(&harness, Appearance::Light, Role::FgDefault))
        );

        harness.drain();
        let grown = harness.canvas_primitives(node);
        assert_eq!(
            grown[1].bounds,
            Some(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0))),
            "the next frame repainted the quad at the next progress"
        );
    }

    #[test]
    fn a_settled_canvas_is_not_rewritten_and_asks_for_no_more_frames() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<CanvasWidget>(());
        for _ in 0..=CANVAS_TICKS {
            harness.drain();
        }
        let node = canvas_node(&harness, &widget);
        let settled_requests = harness.frame_requests();
        let before = harness
            .canvas_handle(node)
            .expect("the node holds a canvas");
        let painted = harness.canvas_primitives(node);
        assert_eq!(
            painted[1].bounds,
            Some(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0))),
            "the animation reached its last progress"
        );

        harness.drain();
        assert!(!harness.rendered(), "a settled canvas paints nothing new");
        assert_eq!(
            harness.frame_requests(),
            settled_requests,
            "the sync must not ask for another frame (I3)"
        );
        let after = harness
            .canvas_handle(node)
            .expect("the node holds a canvas");
        assert!(
            Arc::ptr_eq(&before, &after),
            "a steady state leaves the same canvas value in place (set_canvas was not called)"
        );
        assert!(
            harness.canvas_primitives(node).is_empty(),
            "an idle round has no paint to read"
        );
    }

    #[test]
    fn an_animating_canvas_keeps_asking_for_frames() {
        let mut harness = WidgetTestApp::new();
        let _widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        let first = harness.frame_requests();
        assert!(
            first >= 1,
            "the fixture asks for the frame its next progress needs"
        );
        harness.drain();
        assert!(
            harness.frame_requests() > first,
            "each animated round requests the next one"
        );
    }

    #[test]
    fn switching_the_appearance_repaints_the_canvas_in_the_new_roles() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        let node = canvas_node(&harness, &widget);
        let light = harness.canvas_primitives(node);
        let light_fill = light[1].color.expect("the quad is filled");
        assert_eq!(
            light_fill,
            role_color(&harness, Appearance::Light, Role::MarketUp)
        );

        let before = harness.frame_requests();
        let theme = harness.services().theme.clone();
        assert!(
            theme.set_appearance(harness.app_mut(), Appearance::Dark),
            "the appearance moves"
        );
        assert!(
            harness.frame_requests() > before,
            "the plate reads the theme, so the switch asks for the frame that repaints"
        );
        harness.drain();
        let dark = harness.canvas_primitives(node);
        let dark_fill = dark[1].color.expect("the quad is filled");
        assert_eq!(
            dark_fill,
            role_color(&harness, Appearance::Dark, Role::MarketUp)
        );
        assert_ne!(
            dark_fill, light_fill,
            "the dark appearance resolves another colour"
        );
        assert_eq!(
            dark[3].color,
            Some(role_color(&harness, Appearance::Dark, Role::FgDefault)),
            "the text run follows the theme too"
        );
    }

    #[test]
    fn a_canvas_node_that_was_never_written_paints_no_primitives() {
        let mut harness = WidgetTestApp::new();
        let node = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::canvas(Canvas::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(16.0),
                px(16.0),
            )))
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)))
            .id("TEST_EMPTY_CANVAS")
            .build(cx.tree_mut(), root)
            .expect("the root is alive")
        });
        harness.drain();
        assert!(harness.rendered(), "the node itself paints");
        assert!(
            harness.canvas_primitives(node).is_empty(),
            "no set_canvas means nothing inside the canvas"
        );
        assert_eq!(
            harness
                .paint_entries(node)
                .iter()
                .filter(|entry| entry.kind == PrimitiveKind::Canvas)
                .count(),
            1,
            "the node is still one opaque canvas in the frame's paint"
        );
    }

    #[test]
    fn releasing_a_canvas_widget_hands_its_geometry_back() {
        let mut harness = WidgetTestApp::new();
        let baseline = harness.app().geometry().len();
        let widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        let node = canvas_node(&harness, &widget);
        let primitives = harness.canvas_primitives(node);
        let path = primitives
            .iter()
            .find(|primitive| primitive.kind == PrimitiveKind::Path)
            .expect("the canvas draws its mesh");
        assert!(
            path.geometry.is_some(),
            "the path names the geometry it draws: {path:?}"
        );
        assert_eq!(
            harness.app().geometry().len(),
            baseline + 1,
            "one mesh is registered while the widget lives"
        );

        widget.release(harness.app_mut());
        assert_eq!(
            harness.app().geometry().len(),
            baseline,
            "releasing the widget returns its geometry"
        );
        assert!(
            harness.canvas_handle(node).is_none(),
            "the released widget's canvas node is gone"
        );
    }
}
