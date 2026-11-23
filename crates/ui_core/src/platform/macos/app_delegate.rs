use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject,
};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol};

use crate::platform::{dispatcher::Dispatcher, macos::wake};

#[derive(Debug, Default)]
pub(crate) struct QuitHandshake {
    state: AtomicU8,
}

const IDLE: u8 = 0;
const PENDING: u8 = 1;
const ALLOWED: u8 = 2;

impl QuitHandshake {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn request(&self) -> bool {
        self.state
            .compare_exchange(IDLE, PENDING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(crate) fn pending(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PENDING
    }

    pub(crate) fn allowed(&self) -> bool {
        self.state.load(Ordering::SeqCst) == ALLOWED
    }

    pub(crate) fn allow(&self) {
        self.state.store(ALLOWED, Ordering::SeqCst);
    }

    pub(crate) fn veto(&self) {
        self.state.store(IDLE, Ordering::SeqCst);
    }
}

pub(crate) struct AppDelegateIvars {
    dispatcher: Arc<Dispatcher>,
    quit: Arc<QuitHandshake>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixAppDelegate"]
    #[ivars = AppDelegateIvars]
    pub(crate) struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            if self.ivars().quit.allowed() {
                NSApplicationTerminateReply::TerminateNow
            } else {
                self.ivars().quit.request();
                self.ivars().dispatcher.wake();
                NSApplicationTerminateReply::TerminateLater
            }
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            false
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            wake::uninstall();
        }
    }
);

impl AppDelegate {
    pub(crate) fn new(
        mtm: MainThreadMarker,
        dispatcher: Arc<Dispatcher>,
        quit: Arc<QuitHandshake>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars { dispatcher, quit });
        unsafe { msg_send![super(this), init] }
    }

    pub(crate) fn protocol(&self) -> &ProtocolObject<dyn NSApplicationDelegate> {
        ProtocolObject::from_ref(self)
    }
}
