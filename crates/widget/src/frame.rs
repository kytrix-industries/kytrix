//! The single frame hook's fan-out: one router, one `App::on_frame`, many widgets.
//!
//! The engine's [`App::on_frame`] is a **single slot** — the last registration
//! replaces the previous one — and kit components deliberately do not register
//! themselves. Without one owner of that slot every widget that wants per-frame
//! synchronisation would install its own hook, the first would work and the rest
//! would be silently dead. [`FrameRouter`] is that owner: the composition root
//! creates it once, installs it once, and hands it to every widget.
//!
//! # What a tick is
//!
//! The engine invokes the hook once per event-loop turn, before the frame it is
//! about to submit. A tick runs the registered routes **in registration order** and
//! reports the first error a route raised — every route still runs, because one
//! widget's failure must not starve the rest. A route is a `Fn(&mut App)` closure
//! that owns whatever it needs (`Rc`, `Weak`, `Signal` handles), so the router holds
//! no widget type and the widget holds no router entry beyond its [`RouteId`].
//!
//! # I3: an empty router requests no frame
//!
//! Neither [`FrameRouter::install`] nor [`FrameRouter::tick`] schedules a frame. A
//! tick with no routes is a no-op, and a route that finds nothing changed must not
//! write a signal — that is what keeps an idle application at zero frames. The
//! invariant is testable through [`App::frame_requests`], which is exactly what the
//! module's tests do.
//!
//! # The slot is not shared
//!
//! Installing the router is what the composition root does *instead of* calling
//! [`App::on_frame`] for anything else. A second installer would replace the
//! router's hook, which is the engine's documented single-slot semantics and the
//! reason this type exists at all: there is one place to look for "who runs on
//! every frame".

use std::{
    cell::RefCell,
    fmt,
    rc::{Rc, Weak},
};

use ui_core::{app::App, error::EngineError};

/// The result a frame route reports.
///
/// The sync of one widget is allowed to fail for the engine's reasons (a node it
/// kept was removed, a window it names is gone); the error is surfaced by
/// [`FrameRouter::tick`] instead of being swallowed by the frame hook.
pub type RouteResult = Result<(), EngineError>;

/// A registered frame route's identity, returned by [`FrameRouter::register`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RouteId(u64);

impl RouteId {
    /// The numeric identity, for diagnostics and test assertions.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A registered route: the callback and the router entry that can remove it.
struct Route {
    id: RouteId,
    sync: Rc<dyn Fn(&mut App) -> RouteResult>,
}

#[derive(Default)]
struct RouterState {
    /// Whether the one `App::on_frame` slot has been taken by this router.
    installed: bool,
    next_id: u64,
    routes: Vec<Route>,
}

impl RouterState {
    fn register(&mut self, sync: Rc<dyn Fn(&mut App) -> RouteResult>) -> RouteId {
        let id = RouteId(self.next_id);
        self.next_id += 1;
        self.routes.push(Route { id, sync });
        id
    }

    fn unregister(&mut self, id: RouteId) -> bool {
        let before = self.routes.len();
        self.routes.retain(|route| route.id != id);
        self.routes.len() != before
    }
}

/// Fans the single frame hook out to every registered widget route.
///
/// `Clone` shares the route table: the composition root keeps one handle, the
/// installed hook keeps another, and unregistering through either is visible to
/// both. The router is `!Send` by construction (`Rc`), like every other
/// application-layer handle.
#[derive(Clone, Default)]
pub struct FrameRouter {
    state: Rc<RefCell<RouterState>>,
}

impl FrameRouter {
    /// Creates a router and installs it as the application's one frame hook.
    ///
    /// This is the whole installation protocol: the composition root calls it once,
    /// after the window and the widget surfaces exist, and never registers another
    /// frame hook itself.
    pub fn new(app: &mut App) -> Self {
        let router = Self::default();
        router.install(app);
        router
    }

    /// Installs this router as the application's frame hook.
    ///
    /// Idempotent: installing the same router twice leaves the first hook in place,
    /// because the hook already points at this router's shared route table.
    pub fn install(&self, app: &mut App) {
        if self.state.borrow().installed {
            return;
        }

        self.state.borrow_mut().installed = true;
        let ticker = self.clone();
        app.on_frame(move |app| {
            if let Err(error) = ticker.tick(app) {
                log::error!("a widget frame route failed: {error}");
            }
        });
    }

    /// Registers one frame route.
    ///
    /// Registration order is execution order. The returned [`RouteId`] is what
    /// [`unregister`](Self::unregister) takes; a route that is never unregistered
    /// keeps running for as long as the application lives.
    pub fn register(&self, sync: impl Fn(&mut App) -> RouteResult + 'static) -> RouteId {
        self.state.borrow_mut().register(Rc::new(sync))
    }

    /// Removes a route. Returns whether it was registered.
    ///
    /// A route unregistered from inside a tick may still run once in that same
    /// tick: the tick iterates the routes it captured when it began, so removing
    /// one cannot invalidate the iteration. Removal takes effect on the next tick.
    pub fn unregister(&self, id: RouteId) -> bool {
        self.state.borrow_mut().unregister(id)
    }

    /// Runs every registered route once, in registration order.
    ///
    /// Returns the first error any route reported; later routes still ran. This is
    /// the engine hook's body and the function component tests call directly —
    /// [`ui_core::test_support::app::TestApp`] never invokes `App::on_frame`, so a
    /// test that wants a synchronized frame calls `tick` before draining.
    pub fn tick(&self, app: &mut App) -> RouteResult {
        let routes: Vec<Rc<dyn Fn(&mut App) -> RouteResult>> = {
            let state = self.state.borrow();
            state
                .routes
                .iter()
                .map(|route| route.sync.clone())
                .collect()
        };

        let mut first_error = None;
        for sync in routes {
            if let Err(error) = sync(app)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    /// The number of registered routes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.borrow().routes.len()
    }

    /// Whether no route is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this router took the application's frame slot.
    #[must_use]
    pub fn installed(&self) -> bool {
        self.state.borrow().installed
    }

    /// A weak handle to the route table, for a route that wants to reach the router.
    ///
    /// The installed hook owns the router strongly for as long as the application
    /// lives; a widget that wants to unregister itself from its own callback would
    /// otherwise grow a reference cycle (widget → router → route → widget).
    #[must_use]
    pub fn downgrade(&self) -> WeakRouter {
        WeakRouter(Rc::downgrade(&self.state))
    }
}

impl fmt::Debug for FrameRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.borrow();
        formatter
            .debug_struct("FrameRouter")
            .field("installed", &state.installed)
            .field("routes", &state.routes.len())
            .finish_non_exhaustive()
    }
}

/// A non-owning handle to a [`FrameRouter`]'s route table.
///
/// It exists for one pattern: a route that removes itself when the thing it
/// synchronizes is released. The callback then captures a weak handles instead of
/// the router, so the router (owned by the application's frame hook) does not keep
/// the callback — and through it the widget — alive.
#[derive(Clone)]
pub struct WeakRouter(Weak<RefCell<RouterState>>);

impl WeakRouter {
    /// Removes the route if the router is still alive.
    pub fn unregister(&self, id: RouteId) -> bool {
        self.0
            .upgrade()
            .is_some_and(|state| state.borrow_mut().unregister(id))
    }
}

impl fmt::Debug for WeakRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WeakRouter")
            .field("alive", &self.0.strong_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use smol_str::SmolStr;
    use ui_core::{app::App, error::EngineError};

    use super::{FrameRouter, RouteId};

    /// A headless engine application; the frame hook is never run by the test
    /// support, so every test here drives [`FrameRouter::tick`] directly.
    fn app() -> App {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        App::new(runtime.handle().clone())
    }

    /// A route that counts how often it ran.
    fn counted(calls: &Rc<Cell<usize>>) -> impl Fn(&mut App) -> Result<(), EngineError> + 'static {
        let calls = calls.clone();
        move |_| {
            calls.set(calls.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn installing_the_router_takes_the_single_frame_slot() {
        let mut app = app();
        let router = FrameRouter::new(&mut app);

        assert!(router.installed());
        // A second installation is a no-op, not a replacement: the hook already
        // points at this router.
        router.install(&mut app);
        assert!(router.installed());
    }

    /// I3: an empty router never asks the application for a frame, and neither its
    /// installation nor its tick schedules one.
    #[test]
    fn an_empty_router_should_request_no_frame() {
        let mut app = app();
        let before = app.frame_requests();

        let router = FrameRouter::new(&mut app);
        router.tick(&mut app).expect("an empty tick succeeds");

        assert!(router.is_empty());
        assert_eq!(
            app.frame_requests(),
            before,
            "an idle router is not a frame loop"
        );
    }

    /// The whole reason the router exists: every widget's `sync` runs once per
    /// tick, in registration order.
    #[test]
    fn a_tick_should_fan_out_to_every_route_once() {
        let mut app = app();
        let router = FrameRouter::new(&mut app);
        let first = Rc::new(Cell::new(0));
        let second = Rc::new(Cell::new(0));
        router.register(counted(&first));
        router.register(counted(&second));

        router.tick(&mut app).expect("the tick succeeds");
        router.tick(&mut app).expect("the tick succeeds");

        assert_eq!(first.get(), 2);
        assert_eq!(second.get(), 2);
        assert_eq!(router.len(), 2);
    }

    #[test]
    fn unregistering_a_route_should_stop_its_delivery() {
        let mut app = app();
        let router = FrameRouter::new(&mut app);
        let calls = Rc::new(Cell::new(0));
        let id = router.register(counted(&calls));

        assert!(router.unregister(id));
        assert!(!router.unregister(id), "removing twice changes nothing");
        router.tick(&mut app).expect("the tick succeeds");

        assert_eq!(calls.get(), 0);
        assert!(router.is_empty());
    }

    #[test]
    fn route_ids_are_unique_and_returned_on_registration() {
        let mut app = app();
        let router = FrameRouter::new(&mut app);
        let first: RouteId = router.register(|_| Ok(()));
        let second: RouteId = router.register(|_| Ok(()));

        assert_ne!(first, second);
        assert_eq!(router.len(), 2);
    }

    /// One widget's failure must not starve the rest: the tick runs every route and
    /// reports the first error it saw.
    #[test]
    fn a_failing_route_should_be_reported_without_stopping_the_others() {
        let mut app = app();
        let router = FrameRouter::new(&mut app);
        let after = Rc::new(Cell::new(0));
        router.register(|_| {
            Err(EngineError::FramePipeline {
                detail: SmolStr::new_static("widget sync failed"),
            })
        });
        router.register(counted(&after));

        let error = router.tick(&mut app).expect_err("the failure is reported");

        assert!(matches!(error, EngineError::FramePipeline { .. }));
        assert_eq!(after.get(), 1, "the route after the failure still ran");
    }

    /// A route can remove itself through a weak handle without keeping the router
    /// (and therefore its own callback) alive.
    #[test]
    fn a_route_can_unregister_itself_through_a_weak_handle() {
        let mut app = app();
        let router = FrameRouter::new(&mut app);
        let weak = router.downgrade();
        let slot = Rc::new(Cell::new(None::<RouteId>));
        let captured = slot.clone();
        let calls = Rc::new(Cell::new(0));
        let id = router.register(move |_| {
            let calls = calls.clone();
            calls.set(calls.get() + 1);
            if let Some(id) = captured.get() {
                assert!(weak.unregister(id));
            }
            Ok(())
        });
        slot.set(Some(id));

        router.tick(&mut app).expect("the first tick succeeds");
        router.tick(&mut app).expect("the second tick succeeds");

        assert!(router.is_empty(), "the route removed itself after one run");
    }
}
