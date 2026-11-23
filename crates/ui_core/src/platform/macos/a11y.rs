use std::{ffi::c_void, sync::Arc};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use accesskit_macos::Adapter;
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_foundation::NSArray;

use crate::{
    a11y::adapter::A11yConsumerEpoch,
    error::EngineError,
    platform::{
        dispatcher::{Dispatcher, UserTask},
        macos::view::GuiView,
    },
    window::WindowId,
};

pub(crate) struct MacA11y {
    adapter: Adapter,
    activation: MacActivation,
}

impl MacA11y {
    pub(crate) fn new(
        view: &GuiView,
        dispatcher: Arc<Dispatcher>,
        window: WindowId,
        epoch: A11yConsumerEpoch,
    ) -> Result<Self, EngineError> {
        if objc2::MainThreadMarker::new().is_none() {
            return Err(EngineError::A11yInit {
                detail: smol_str::SmolStr::new_static("accesskit_macos requires the main thread"),
            });
        }
        let pointer = view as *const GuiView as *mut c_void;
        if pointer.is_null() {
            return Err(EngineError::A11yInit {
                detail: smol_str::SmolStr::new_static("null NSView"),
            });
        }
        let activation = MacActivation {
            epoch,
            dispatcher: Arc::clone(&dispatcher),
        };
        let actions = MacActions { dispatcher, window };
        let adapter = unsafe { Adapter::new(pointer, false, actions) };
        Ok(Self {
            adapter,
            activation,
        })
    }

    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate) {
        if let Some(events) = self.adapter.update_if_active(factory) {
            events.raise();
        }
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) {
        if let Some(events) = self.adapter.update_view_focus_state(focused) {
            events.raise();
        }
    }

    pub(crate) fn hit_test(&mut self, x: f64, y: f64) -> Option<Retained<AnyObject>> {
        let point = accesskit_macos::NSPoint { x, y };
        let raw = self.adapter.hit_test(point, &mut self.activation);
        if raw.is_null() {
            None
        } else {
            unsafe { Retained::retain(raw.cast::<AnyObject>()) }
        }
    }

    pub(crate) fn children(&mut self) -> Option<Retained<NSArray>> {
        let raw = self.adapter.view_children(&mut self.activation);
        if raw.is_null() {
            None
        } else {
            unsafe { Retained::retain(raw.cast::<NSArray>()) }
        }
    }

    pub(crate) fn focused(&mut self) -> Option<Retained<AnyObject>> {
        let raw = self.adapter.focus(&mut self.activation);
        if raw.is_null() {
            None
        } else {
            unsafe { Retained::retain(raw.cast::<AnyObject>()) }
        }
    }
}

impl std::fmt::Debug for MacA11y {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacA11y").finish_non_exhaustive()
    }
}

pub(crate) struct MacActivation {
    epoch: A11yConsumerEpoch,
    dispatcher: Arc<Dispatcher>,
}

impl ActivationHandler for MacActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.epoch.bump();
        self.dispatcher.wake();
        None
    }
}

struct MacActions {
    dispatcher: Arc<Dispatcher>,
    window: WindowId,
}

impl ActionHandler for MacActions {
    fn do_action(&mut self, request: ActionRequest) {
        let window = self.window;
        self.dispatcher.push(UserTask::new(move |app| {
            crate::a11y::action::dispatch(app, window, request);
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use accesskit::ActivationHandler;

    use super::MacActivation;
    use crate::{app::App, platform::dispatcher::Dispatcher, window::WindowOptions};

    #[test]
    fn activation_wakes_the_main_thread_and_forces_the_next_full_tree() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions::default());
        let epoch = app.window_a11y_epoch(window).expect("窗口存在");
        let before = epoch.current();
        let dispatcher = Dispatcher::new();
        let _ = dispatcher.take_frame_wake();
        let mut activation = MacActivation {
            epoch: epoch.clone(),
            dispatcher: Arc::clone(&dispatcher),
        };
        assert!(
            activation.request_initial_tree().is_none(),
            "引擎异步交付首树：激活回调只标记，不同步构造 TreeUpdate"
        );
        assert_eq!(
            epoch.current(),
            before + 1,
            "激活回调必须标记消费端已重置（否则下一笔仍是增量）"
        );
        assert!(
            dispatcher.has_pending_wake(),
            "激活必须 wake 主线程（空闲重激活否则要等下一次输入才有树）"
        );
        assert!(
            app.window(window)
                .expect("window")
                .a11y_consumer_epoch_pending(),
            "窗口 bridge 必须观察到同一个 epoch 变化（共享同一份句柄）"
        );
        assert!(
            app.a11y_pending(window),
            "激活后 App 门禁必须立即放行（不节流旁路）"
        );
        let snapshot = app.a11y_take(window).expect("重激活后的第一笔必须产出");
        assert!(
            snapshot.full,
            "重激活后的第一笔必须是全树（否则 consumer panic / 永久空树）"
        );
    }
}
