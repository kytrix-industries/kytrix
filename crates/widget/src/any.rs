use std::{
    any::Any,
    cell::RefCell,
    rc::{Rc, Weak},
};

use app_config::widget::{VersionedWidgetConfig, WidgetConfig};
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    node::{id::NodeId, signal::SignalScope},
    window::WindowId,
};

use super::{
    config::WidgetConfigChange,
    cx::WidgetCx,
    descriptor::WidgetDescriptor,
    frame::RouteId,
    identity::{WidgetId, WidgetInstanceId},
    instrument_link::InstrumentLink,
    services::WidgetServices,
    status::{WidgetAction, WidgetStatus},
    surface::WidgetSurface,
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

const RECOVERY_KEY: &str = "common.retry";

type ErasedBody = Rc<RefCell<Option<Box<dyn Any>>>>;

struct WidgetVTable {
    body: fn(&ErasedBody, NodeId, &mut WidgetCx<'_>) -> Result<(), EngineError>,
    sync: fn(&ErasedBody, &mut WidgetCx<'_>) -> Result<(), EngineError>,
    header: fn(&ErasedBody, HeaderGroup, NodeId, &mut WidgetCx<'_>) -> Result<(), EngineError>,
    dispatch: fn(&ErasedBody, &Event, &mut WidgetCx<'_>) -> bool,
    release: fn(&ErasedBody, &mut WidgetCx<'_>),
    has_content: fn(&ErasedBody) -> bool,
    config: fn(&ErasedBody) -> Option<VersionedWidgetConfig>,
    instrument_link: fn(&ErasedBody) -> Option<InstrumentLink>,
}

impl WidgetVTable {
    fn of<W: WidgetView>() -> Self {
        Self {
            body: body_of::<W>,
            sync: sync_of::<W>,
            header: header_of::<W>,
            dispatch: dispatch_of::<W>,
            release: release_of::<W>,
            has_content: has_content_of::<W>,
            config: config_of::<W>,
            instrument_link: instrument_link_of::<W>,
        }
    }
}

fn with_view<W: WidgetView, R>(body: &ErasedBody, read: impl FnOnce(&mut W) -> R) -> R {
    let mut borrow = body.borrow_mut();
    let view = borrow
        .as_mut()
        .and_then(|erased| erased.downcast_mut::<W>())
        .expect(
            "the widget vtable matches the widget it was mounted with, and a released widget's \
             body is never reached: entry points check `released` first",
        );

    read(view)
}

fn body_of<W: WidgetView>(
    body: &ErasedBody,
    parent: NodeId,
    cx: &mut WidgetCx<'_>,
) -> Result<(), EngineError> {
    with_view::<W, _>(body, |view| view.body(parent, cx))
}

fn sync_of<W: WidgetView>(body: &ErasedBody, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
    with_view::<W, _>(body, |view| view.sync(cx))
}

fn header_of<W: WidgetView>(
    body: &ErasedBody,
    group: HeaderGroup,
    parent: NodeId,
    cx: &mut WidgetCx<'_>,
) -> Result<(), EngineError> {
    with_view::<W, _>(body, |view| view.header(group, parent, cx))
}

fn dispatch_of<W: WidgetView>(body: &ErasedBody, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
    with_view::<W, _>(body, |view| view.dispatch(event, cx))
}

fn release_of<W: WidgetView>(body: &ErasedBody, cx: &mut WidgetCx<'_>) {
    with_view::<W, _>(body, |view| view.release(cx));
}

fn has_content_of<W: WidgetView>(body: &ErasedBody) -> bool {
    let borrow = body.borrow();
    borrow
        .as_ref()
        .and_then(|erased| erased.downcast_ref::<W>())
        .is_some_and(WidgetView::has_content)
}

fn config_of<W: WidgetView>(body: &ErasedBody) -> Option<VersionedWidgetConfig> {
    let borrow = body.borrow();
    let view = borrow
        .as_ref()
        .and_then(|erased| erased.downcast_ref::<W>())?;
    let config = view.config()?;

    match VersionedWidgetConfig::encode(config) {
        Ok(envelope) => Some(envelope),
        Err(error) => {
            log::error!(
                "failed to encode the config of `{}`: {error}",
                W::ID.as_str()
            );
            None
        }
    }
}

fn instrument_link_of<W: WidgetView>(body: &ErasedBody) -> Option<InstrumentLink> {
    let borrow = body.borrow();
    borrow
        .as_ref()
        .and_then(|erased| erased.downcast_ref::<W>())
        .and_then(WidgetView::instrument_link)
}

struct WidgetSlot {
    descriptor: WidgetDescriptor,
    instance_id: WidgetInstanceId,
    status: WidgetStatus,
    surface: WidgetSurface,
    body: ErasedBody,
    vtable: WidgetVTable,
    services: Rc<WidgetServices>,
    window: WindowId,
    scope: SignalScope,
    route: Option<RouteId>,
    body_built: bool,
    released: bool,
}

impl WidgetSlot {
    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }

        let Self {
            surface,
            body,
            vtable,
            services,
            window,
            scope,
            body_built,
            ..
        } = self;

        let has_content = (vtable.has_content)(body);
        if has_content && !*body_built {
            let mut cx = WidgetCx::new(app, *window, services, *scope);
            (vtable.body)(body, surface.content_slot(), &mut cx)?;
            *body_built = true;
        }

        let mut cx = WidgetCx::new(app, *window, services, *scope);
        (vtable.sync)(body, &mut cx)?;
        surface.sync(&mut cx, has_content)?;

        Ok(())
    }

    fn header(&self, app: &mut App, group: HeaderGroup, parent: NodeId) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }

        let mut cx = WidgetCx::new(app, self.window, &self.services, self.scope);
        (self.vtable.header)(&self.body, group, parent, &mut cx)
    }

    fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        if self.released {
            return false;
        }

        let surface = self.surface.dispatch(app, event);
        let view = {
            let mut cx = WidgetCx::new(app, self.window, &self.services, self.scope);
            (self.vtable.dispatch)(&self.body, event, &mut cx)
        };

        surface || view
    }

    fn release(&mut self, app: &mut App) -> usize {
        if self.released {
            return 0;
        }
        self.released = true;

        if let Some(route) = self.route.take() {
            self.services.frame_router.unregister(route);
        }

        {
            let mut cx = WidgetCx::new(app, self.window, &self.services, self.scope);
            (self.vtable.release)(&self.body, &mut cx);
        }

        let mut released = self.surface.release(app);
        drop(self.body.borrow_mut().take());

        if let Ok(outcome) =
            app.remove_node_with_scope(self.window, self.surface.root(), self.scope)
        {
            released += outcome.released_values;
        }

        released
    }
}

impl Drop for WidgetSlot {
    fn drop(&mut self) {
        if let Some(route) = self.route.take() {
            self.services.frame_router.unregister(route);
        }
    }
}

#[derive(Clone)]
pub struct AnyWidget {
    slot: Rc<RefCell<WidgetSlot>>,
}

impl AnyWidget {
    #[allow(clippy::too_many_arguments)]
    pub fn mount<W: WidgetView>(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
        instance_id: WidgetInstanceId,
        config: W::Config,
        config_change: WidgetConfigChange,
    ) -> Result<Self, EngineError> {
        let scope = app.signal_scope();
        Self::mount_in_scope::<W>(
            app,
            window,
            parent,
            services,
            instance_id,
            config,
            config_change,
            scope,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mount_in_scope<W: WidgetView>(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
        instance_id: WidgetInstanceId,
        config: W::Config,
        config_change: WidgetConfigChange,
        scope: SignalScope,
    ) -> Result<Self, EngineError> {
        let status = WidgetStatus::new(app, scope);

        let surface = {
            let mut cx = WidgetCx::new(app, window, services, scope);
            match WidgetSurface::build(&mut cx, parent, status) {
                Ok(surface) => surface,
                Err(error) => {
                    app.release_scope(scope);
                    return Err(error);
                }
            }
        };

        let body: ErasedBody = Rc::new(RefCell::new(None));
        let reload = reload_handle::<W>(Rc::downgrade(&body), window, services, scope);
        let recovery = WidgetAction::new(RECOVERY_KEY, {
            let reload = reload.clone();
            move |app| reload.reload(app)
        });

        let constructed = {
            let mut cx = WidgetCx::new(app, window, services, scope);
            W::new(
                instance_id,
                config,
                status,
                reload,
                config_change,
                recovery,
                &mut cx,
            )
        };
        let widget = match constructed {
            Ok(widget) => widget,
            Err(error) => {
                let mut released = surface.release(app);
                if let Ok(outcome) = app.remove_node_with_scope(window, surface.root(), scope) {
                    released += outcome.released_values;
                }
                let _ = released;

                return Err(error);
            }
        };
        *body.borrow_mut() = Some(Box::new(widget));

        {
            let mut cx = WidgetCx::new(app, window, services, scope);
            let mut borrow = body.borrow_mut();
            let view = borrow
                .as_mut()
                .and_then(|erased| erased.downcast_mut::<W>())
                .expect("the body was just filled with this widget's type");
            view.load(&mut cx);
        }

        let descriptor = descriptor_of::<W>();
        let slot = Rc::new(RefCell::new(WidgetSlot {
            descriptor,
            instance_id,
            status,
            surface,
            body,
            vtable: WidgetVTable::of::<W>(),
            services: Rc::clone(services),
            window,
            scope,
            route: None,
            body_built: false,
            released: false,
        }));

        let route = {
            let slot: Weak<RefCell<WidgetSlot>> = Rc::downgrade(&slot);
            services.frame_router.register(move |app| {
                let Some(slot) = slot.upgrade() else {
                    return Ok(());
                };
                slot.borrow_mut().sync(app)
            })
        };
        slot.borrow_mut().route = Some(route);

        Ok(Self { slot })
    }

    pub fn descriptor(&self) -> WidgetDescriptor {
        self.slot.borrow().descriptor.clone()
    }

    pub fn widget_id(&self) -> WidgetId {
        self.slot.borrow().descriptor.id().clone()
    }

    pub fn instance_id(&self) -> WidgetInstanceId {
        self.slot.borrow().instance_id
    }

    pub fn surface(&self) -> NodeId {
        self.slot.borrow().surface.root()
    }

    pub fn status(&self) -> WidgetStatus {
        self.slot.borrow().status
    }

    pub fn config(&self) -> Option<VersionedWidgetConfig> {
        let slot = self.slot.borrow();
        if slot.released {
            return None;
        }
        (slot.vtable.config)(&slot.body)
    }

    pub fn instrument_link(&self) -> Option<InstrumentLink> {
        let slot = self.slot.borrow();
        if slot.released {
            return None;
        }
        (slot.vtable.instrument_link)(&slot.body)
    }

    pub fn header(
        &self,
        app: &mut App,
        group: HeaderGroup,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        self.slot.borrow().header(app, group, parent)
    }

    pub fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        self.slot.borrow_mut().sync(app)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.slot.borrow().dispatch(app, event)
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.slot.borrow_mut().release(app)
    }

    pub fn scope(&self) -> SignalScope {
        self.slot.borrow().scope
    }

    pub fn window(&self) -> WindowId {
        self.slot.borrow().window
    }

    pub fn is_released(&self) -> bool {
        self.slot.borrow().released
    }

    pub fn display_name(&self) -> &'static str {
        self.slot.borrow().descriptor.display_name()
    }
}

impl std::fmt::Debug for AnyWidget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slot = self.slot.borrow();
        formatter
            .debug_struct("AnyWidget")
            .field("descriptor", &slot.descriptor)
            .field("instance_id", &slot.instance_id)
            .field("surface", &slot.surface.root())
            .field("released", &slot.released)
            .finish_non_exhaustive()
    }
}

fn descriptor_of<W: WidgetView>() -> WidgetDescriptor {
    let descriptor = WidgetDescriptor::new(
        W::ID,
        W::DISPLAY_NAME,
        W::CATEGORY,
        <<W as WidgetView>::Config as WidgetConfig>::SCHEMA_VERSION,
    );
    let descriptor = match W::DESCRIPTION {
        Some(description) => descriptor.with_description(description),
        None => descriptor,
    };
    let descriptor = match W::ICON {
        Some(icon) => descriptor.with_icon(icon),
        None => descriptor,
    };

    descriptor.with_selectable(W::SELECTABLE)
}

fn reload_handle<W: WidgetView>(
    body: Weak<RefCell<Option<Box<dyn Any>>>>,
    window: WindowId,
    services: &Rc<WidgetServices>,
    scope: SignalScope,
) -> ReloadHandle {
    let services = Rc::clone(services);

    ReloadHandle::new(move |app| {
        let Some(cell) = body.upgrade() else {
            return;
        };
        let mut borrow = cell.borrow_mut();
        let Some(view) = borrow
            .as_mut()
            .and_then(|erased| erased.downcast_mut::<W>())
        else {
            return;
        };
        let mut cx = WidgetCx::new(app, window, &services, scope);
        view.load(&mut cx);
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smol_str::SmolStr;
    use ui_core::{error::EngineError, node::id::NodeId};

    use crate::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        cx::WidgetCx,
        identity::{WidgetId, WidgetInstanceId},
        status::{Notice, WidgetAction, WidgetState, WidgetStatus},
        test_support::{FixtureFailure, WaitingWidget, WidgetTestApp},
        view::{HeaderGroup, ReloadHandle, WidgetView},
    };

    struct FailingWidget;

    impl WidgetView for FailingWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("test.failing");
        const DISPLAY_NAME: &'static str = "test.failing";
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
            Err(EngineError::FramePipeline {
                detail: SmolStr::new_static("the fixture refuses construction"),
            })
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[test]
    fn a_refused_construction_removes_the_shell_it_built() {
        let mut harness = WidgetTestApp::new();
        let before = harness.node_count();

        let error = harness
            .try_mount::<FailingWidget>(())
            .expect_err("the fixture refuses construction");

        assert!(matches!(error, EngineError::FramePipeline { .. }));
        assert_eq!(
            harness.node_count(),
            before,
            "the surface nodes are removed with the mount attempt"
        );
        assert!(
            harness.router().is_empty(),
            "no frame route is registered for a widget that does not exist"
        );
    }

    #[test]
    fn a_refused_surface_construction_releases_the_scope_it_minted() {
        let mut harness = WidgetTestApp::new();
        let parent = harness.placeholder_parent();
        let window = harness.window();
        harness
            .app_mut()
            .remove_node(window, parent)
            .expect("the placeholder host is removable");
        let scope = harness.app_mut().signal_scope();

        let error = match harness.try_mount_in_scope::<WaitingWidget>(parent, scope, ()) {
            Ok(_) => panic!("a stale parent refuses the surface"),
            Err(error) => error,
        };

        assert!(
            matches!(error, EngineError::StaleNode { .. }),
            "the surface build reports the stale parent: {error:?}"
        );
        assert_eq!(
            harness.app_mut().release_scope(scope),
            0,
            "the refused mount released the scope it minted"
        );
    }

    struct RecoveringWidget {
        status: WidgetStatus,
        recovery: WidgetAction,
        failed: bool,
    }

    impl WidgetView for RecoveringWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("test.recovering");
        const DISPLAY_NAME: &'static str = "test.recovering";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self {
                status,
                recovery,
                failed: false,
            })
        }

        fn load(&mut self, cx: &mut WidgetCx<'_>) {
            if self.failed {
                self.status.report(
                    cx.app_mut(),
                    WidgetState::waiting(Notice::message("test.waiting")),
                );
                return;
            }
            self.failed = true;
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(FixtureFailure), self.recovery.clone()),
            );
        }

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[test]
    fn the_recovery_control_runs_the_widget_load_again() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<RecoveringWidget>(());
        assert!(widget.status().is_failed(harness.app()));
        harness.drain();

        let placeholder = harness
            .find_by_debug_prefix(widget.surface(), "WIDGET_PLACEHOLDER")
            .expect("the failure stands in for the missing content");
        harness.click_recovery(placeholder);

        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(Notice::message("test.waiting")),
            "the click ran the widget's load again, and what it reported is the state"
        );
        harness.drain();
        assert!(
            harness
                .text_of(widget.surface())
                .contains("Waiting for orders"),
            "the frame after the recovery draws what the second load reported"
        );
    }

    #[test]
    fn a_released_widget_answers_no_header_controls() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<WaitingWidget>(());
        widget.release(harness.app_mut());
        let parent = harness.root();

        widget
            .header(harness.app_mut(), HeaderGroup::Left, parent)
            .expect("a released widget builds nothing");
        assert!(widget.is_released());
    }
}
