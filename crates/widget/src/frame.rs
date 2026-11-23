use std::{
    cell::RefCell,
    fmt,
    rc::{Rc, Weak},
};

use ui_core::{app::App, error::EngineError};

pub type RouteResult = Result<(), EngineError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RouteId(u64);

impl RouteId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

struct Route {
    id: RouteId,
    sync: Rc<dyn Fn(&mut App) -> RouteResult>,
}

#[derive(Default)]
struct RouterState {
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

#[derive(Clone, Default)]
pub struct FrameRouter {
    state: Rc<RefCell<RouterState>>,
}

impl FrameRouter {
    pub fn new(app: &mut App) -> Result<Self, EngineError> {
        let router = Self::default();
        router.install(app)?;
        Ok(router)
    }

    pub fn install(&self, app: &mut App) -> Result<(), EngineError> {
        if self.state.borrow().installed {
            return Ok(());
        }
        if app.frame_hook_installed() {
            return Err(EngineError::FrameHookInstalled);
        }

        self.state.borrow_mut().installed = true;
        let ticker = self.clone();
        app.on_frame(move |app| {
            if let Err(error) = ticker.tick(app) {
                log::error!("a widget frame route failed: {error}");
            }
        });

        Ok(())
    }

    pub fn register(&self, sync: impl Fn(&mut App) -> RouteResult + 'static) -> RouteId {
        self.state.borrow_mut().register(Rc::new(sync))
    }

    pub fn unregister(&self, id: RouteId) -> bool {
        self.state.borrow_mut().unregister(id)
    }

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

    #[must_use]
    pub fn len(&self) -> usize {
        self.state.borrow().routes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn installed(&self) -> bool {
        self.state.borrow().installed
    }

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

#[derive(Clone)]
pub struct WeakRouter(Weak<RefCell<RouterState>>);

impl WeakRouter {
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

    fn app() -> App {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        App::new(runtime.handle().clone())
    }

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
        let router = FrameRouter::new(&mut app).expect("the slot is free");

        assert!(router.installed());
        router
            .install(&mut app)
            .expect("installing twice is idempotent");
        assert!(router.installed());
    }

    #[test]
    fn a_second_router_cannot_install_itself() {
        let mut app = app();
        let first = FrameRouter::new(&mut app).expect("the slot is free");
        let calls = Rc::new(Cell::new(0));
        first.register(counted(&calls));

        let second = FrameRouter::default();
        let error = second
            .install(&mut app)
            .expect_err("the slot is already taken");

        assert!(matches!(error, EngineError::FrameHookInstalled));
        assert!(!second.installed(), "the refused router says so");
        assert!(second.is_empty());
        assert!(first.installed());
    }

    #[test]
    fn a_second_router_constructor_reports_the_occupied_slot() {
        let mut app = app();
        let _first = FrameRouter::new(&mut app).expect("the slot is free");

        let error = FrameRouter::new(&mut app).expect_err("the slot is already taken");

        assert!(matches!(error, EngineError::FrameHookInstalled));
    }

    #[test]
    fn an_empty_router_should_request_no_frame() {
        let mut app = app();
        let before = app.frame_requests();

        let router = FrameRouter::new(&mut app).expect("the slot is free");
        router.tick(&mut app).expect("an empty tick succeeds");

        assert!(router.is_empty());
        assert_eq!(
            app.frame_requests(),
            before,
            "an idle router is not a frame loop"
        );
    }

    #[test]
    fn a_tick_should_fan_out_to_every_route_once() {
        let mut app = app();
        let router = FrameRouter::new(&mut app).expect("the slot is free");
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
        let router = FrameRouter::new(&mut app).expect("the slot is free");
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
        let router = FrameRouter::new(&mut app).expect("the slot is free");
        let first: RouteId = router.register(|_| Ok(()));
        let second: RouteId = router.register(|_| Ok(()));

        assert_ne!(first, second);
        assert_eq!(router.len(), 2);
    }

    #[test]
    fn a_failing_route_should_be_reported_without_stopping_the_others() {
        let mut app = app();
        let router = FrameRouter::new(&mut app).expect("the slot is free");
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

    #[test]
    fn a_route_can_unregister_itself_through_a_weak_handle() {
        let mut app = app();
        let router = FrameRouter::new(&mut app).expect("the slot is free");
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
