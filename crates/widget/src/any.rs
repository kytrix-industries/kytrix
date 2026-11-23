//! The type-erased widget instance a pane hosts.
//!
//! [`WidgetView`] is not object safe — its `Config` is an associated type, its
//! constructors are generic, and a widget's state belongs to its own type — so the
//! presentation cannot ask a `dyn WidgetView` anything. The old contract solved that
//! with `AnyView` + `Entity` + downcast; the engine's answer is different: the
//! **node id is the erased handle**. A pane hosts an [`AnyWidget`] and embeds the
//! node [`AnyWidget::surface`] returns, and everything else the framework needs to
//! ask of the concrete type is a function pointer built once at mount.
//!
//! # The vtable, and the one downcast
//!
//! [`AnyWidget::mount::<W>`] monomorphizes a [`WidgetVTable`] for `W`. The widget
//! itself is stored as `Box<dyn Any>` inside the slot; the vtable functions know
//! `W`, so the downcast happens exactly once per call, **inside** the framework, and
//! is infallible by construction: the slot was filled with that same `W` in
//! [`AnyWidget::mount`]. The public surface has no `downcast`, no `AnyView`, and no
//! `Entity` — a caller cannot reach past the vtable to the widget's type, which is
//! what keeps the 20 widgets replaceable one at a time.
//!
//! # The slot is shared, the calls are exclusive
//!
//! The slot lives in an `Rc<RefCell<…>>` because the frame router's route must be
//! able to reach the widget: the route holds a `Weak` to the slot, not a strong
//! reference, so a released widget is collectable. The methods take `&self` (they
//! borrow the slot internally), which is what lets a pane keep one handle and the
//! router keep another. Calling back into the same widget while a call is in flight
//! (a recovery that reloads during `sync`, say) panics on the `RefCell` borrow —
//! that is a programming error, not a state the engine produces.
//!
//! # Teardown order
//!
//! [`AnyWidget::release`] follows the engine's documented order: unregister the
//! frame route, run [`WidgetView::release`] while the nodes still exist, release the
//! surface's components, drop the widget, then
//! [`App::remove_node_with_scope`](ui_core::app::App::remove_node_with_scope) — which
//! removes the subtree before releasing the scope, so no live `bind_*` closure ever
//! reads a released signal. The returned count is the number of signal values the
//! removal released.

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

/// The recovery control every widget offers: the framework's reload path.
const RECOVERY_KEY: &str = "common.retry";

/// The erased widget body: filled at mount, emptied at release.
///
/// The outer `RefCell` is the "the widget exists" cell; the inner `dyn Any` is the
/// concrete view type. The vtable functions are monomorphized for that type.
type ErasedBody = Rc<RefCell<Option<Box<dyn Any>>>>;

/// The type-erased operations a concrete [`WidgetView`] provides.
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

/// Borrows the concrete view for one call.
///
/// The panic is an engine-invariant violation (the slot was filled with another
/// type, or the widget was released and its body taken), not a state a caller can
/// reach: the vtable is constructed by the same `mount` call that fills the cell, and
/// every public entry point guards on [`WidgetSlot::released`] before reaching a
/// vtable function.
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
        // Every field of a widget config is a type serde can write, so this is
        // unreachable in practice. Storing nothing is the least wrong answer: the
        // pane keeps the rest of its layout, and the widget is rebuilt from defaults
        // next time instead of from a half-written config.
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

/// Everything one mounted widget owns, reachable through the shared handle.
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
    /// Runs one frame's widget-side synchronization and applies the presentation.
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

    /// Builds this widget's controls into one of a pane's header groups.
    fn header(&self, app: &mut App, group: HeaderGroup, parent: NodeId) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }

        let mut cx = WidgetCx::new(app, self.window, &self.services, self.scope);
        (self.vtable.header)(&self.body, group, parent, &mut cx)
    }

    /// Forwards one platform event: the surface's controls first, then the widget.
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

    /// Tears the widget down in the engine's documented order.
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

/// One widget instance, with its concrete view type erased.
///
/// This is a structural and lifecycle container: it holds the parts a pane needs
/// without knowing which widget it is hosting. A pane embeds
/// [`AnyWidget::surface`]'s node, calls [`AnyWidget::sync`] from the
/// [`FrameRouter`](crate::frame::FrameRouter) (mount registers that route itself),
/// forwards events to [`AnyWidget::dispatch`], and calls [`AnyWidget::release`] when the
/// pane closes.
#[derive(Clone)]
pub struct AnyWidget {
    slot: Rc<RefCell<WidgetSlot>>,
}

impl AnyWidget {
    /// Creates the widget, builds its surface under `parent`, and starts its first load.
    ///
    /// The order is the contract: the state cell and the surface exist before
    /// [`WidgetView::new`], the widget is stored before [`WidgetView::load`] runs,
    /// and the frame route is registered last — so a load that schedules a frame
    /// always finds a complete widget. The route is registered with the router the
    /// services carry ([`WidgetServices::frame_router`]), which is the composition
    /// root's one router.
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

    /// The mount itself, over a scope the caller minted.
    ///
    /// [`AnyWidget::mount`] mints the scope and delegates. The split is what makes
    /// the failure path observable: the host keeps the scope, so a test can assert
    /// that a refused mount released it instead of leaving the state cell behind.
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
                    // The state cell is everything this attempt minted before the
                    // surface refused; the scope belongs to the attempt, so it goes
                    // with it rather than being left orphaned.
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
                // The surface and the state cell belong to this mount attempt: a
                // widget that failed to construct leaves the pane with no widget, so
                // the shell must go with it. Removing the subtree through the
                // engine's scoped removal also releases the scope the state cell was
                // minted in — the same teardown a released widget gets.
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

    /// Returns this widget's immutable type descriptor.
    pub fn descriptor(&self) -> WidgetDescriptor {
        self.slot.borrow().descriptor.clone()
    }

    /// Returns the stable widget type ID.
    pub fn widget_id(&self) -> WidgetId {
        self.slot.borrow().descriptor.id().clone()
    }

    /// Returns this instance's process-local unique ID.
    pub fn instance_id(&self) -> WidgetInstanceId {
        self.slot.borrow().instance_id
    }

    /// Returns the presentation's root node: what a pane embeds.
    pub fn surface(&self) -> NodeId {
        self.slot.borrow().surface.root()
    }

    /// Returns the state cell the widget reports into.
    pub fn status(&self) -> WidgetStatus {
        self.slot.borrow().status
    }

    /// Returns the config envelope this instance should now be stored as.
    ///
    /// The widget answers with the config it currently holds, so this reflects every
    /// change the widget reported. `None` means this widget persists no options, and
    /// the pane stores no config for it.
    pub fn config(&self) -> Option<VersionedWidgetConfig> {
        let slot = self.slot.borrow();
        if slot.released {
            return None;
        }
        (slot.vtable.config)(&slot.body)
    }

    /// Returns the widget's instrument synchronization capability, if it has one.
    pub fn instrument_link(&self) -> Option<InstrumentLink> {
        let slot = self.slot.borrow();
        if slot.released {
            return None;
        }
        (slot.vtable.instrument_link)(&slot.body)
    }

    /// Builds this widget's controls into one of the pane's header groups.
    ///
    /// A released widget builds nothing and reports success: the pane may rebuild its
    /// header row in the same turn it closed a pane, and asking a released widget is a
    /// no-op rather than a panic. The body cell is empty after release, so the vtable
    /// entry (`with_view`) would otherwise panic.
    pub fn header(
        &self,
        app: &mut App,
        group: HeaderGroup,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        self.slot.borrow().header(app, group, parent)
    }

    /// Runs one frame's synchronization: the widget's own hook, then the surface.
    ///
    /// A pane does not have to call this itself — [`AnyWidget::mount`] registered it
    /// with the frame router — but calling it directly is what a test does, because
    /// the engine's `on_frame` hook does not run in the headless test host.
    pub fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        self.slot.borrow_mut().sync(app)
    }

    /// Forwards one platform event to the widget and to the surface's controls.
    ///
    /// Returns whether the widget (or one of the framework's controls) consumed it.
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.slot.borrow().dispatch(app, event)
    }

    /// Releases the widget and returns the number of signal values released.
    ///
    /// Idempotent: a second call is a no-op that returns zero. See the module
    /// documentation for the order this follows.
    pub fn release(&self, app: &mut App) -> usize {
        self.slot.borrow_mut().release(app)
    }

    /// The widget's own signal scope: what the framework releases on teardown.
    pub fn scope(&self) -> SignalScope {
        self.slot.borrow().scope
    }

    /// The window the widget's nodes belong to.
    pub fn window(&self) -> WindowId {
        self.slot.borrow().window
    }

    /// Whether the widget has been released.
    pub fn is_released(&self) -> bool {
        self.slot.borrow().released
    }

    /// The display name key of the widget type, resolved through the translation
    /// context by the surface that lists it.
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

/// Builds the type descriptor of one widget type.
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

/// The reload handle a widget is constructed with.
///
/// It holds the widget's own weak handle, so a recovery that fires after the widget
/// was released is a no-op rather than a use-after-free — the same reason the previous
/// contract carried a weak handle.
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

    /// A widget whose construction refuses: the mount has nothing to hand a pane.
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

    /// A refused construction rolls the whole mount attempt back: the pane was
    /// handed no widget, so it must not keep the shell the mount built first.
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

    /// A surface refused by a stale parent releases the scope the mount minted: the
    /// state cell and the presentation signal are dropped, not orphaned. The host
    /// owns the scope, which is what makes the leak observable at all.
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

    /// A widget whose first load fails and offers the framework's recovery, and whose
    /// recovery's load settles into a waiting state: the state it reports is how the
    /// test sees which code path ran.
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

    /// The framework's recovery control is the reload path, end to end: the widget's
    /// own failure carries the action `mount` built, a click goes through the engine's
    /// real action dispatch, and the action runs the widget's `load` again — not a
    /// handler the widget (or the test) wired itself.
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

    /// A pane may rebuild its header row after the widget was released in the same
    /// turn; asking a released widget is a no-op, not a panic on the empty body.
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
