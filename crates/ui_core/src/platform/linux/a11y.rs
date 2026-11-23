use std::sync::Arc;

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, TreeUpdate};
use accesskit_unix::Adapter;

use crate::{
    a11y::adapter::A11yConsumerEpoch,
    error::EngineError,
    math::{Point, Scale, Size},
    platform::dispatcher::{Dispatcher, UserTask},
    window::WindowId,
};

pub(crate) struct UnixA11y {
    adapter: Adapter,
}

impl UnixA11y {
    pub(crate) fn new(
        dispatcher: Arc<Dispatcher>,
        window: WindowId,
        epoch: A11yConsumerEpoch,
    ) -> Result<Self, EngineError> {
        let actions = UnixActions {
            dispatcher: Arc::clone(&dispatcher),
            window,
        };
        let activation = UnixActivation {
            epoch: epoch.clone(),
            dispatcher,
        };
        Ok(Self {
            adapter: Adapter::new(activation, actions, UnixDeactivation { epoch }),
        })
    }

    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate) {
        self.adapter.update_if_active(factory);
    }

    pub(crate) fn set_bounds(&mut self, origin: Point, size: Size, scale: Scale) {
        let (outer, inner) = window_bounds(origin, size, scale);
        self.adapter.set_root_window_bounds(outer, inner);
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) {
        self.adapter.update_window_focus_state(focused);
    }
}

struct UnixActivation {
    epoch: A11yConsumerEpoch,
    dispatcher: Arc<Dispatcher>,
}

impl ActivationHandler for UnixActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.epoch.bump();
        self.dispatcher.wake();
        None
    }
}

struct UnixDeactivation {
    epoch: A11yConsumerEpoch,
}

impl DeactivationHandler for UnixDeactivation {
    fn deactivate_accessibility(&mut self) {
        self.epoch.bump();
    }
}

struct UnixActions {
    dispatcher: Arc<Dispatcher>,
    window: WindowId,
}

impl ActionHandler for UnixActions {
    fn do_action(&mut self, request: ActionRequest) {
        let window = self.window;
        self.dispatcher.push(UserTask::new(move |app| {
            crate::a11y::action::dispatch(app, window, request);
        }));
    }
}

#[must_use]
pub(crate) fn window_bounds(
    origin: Point,
    size: Size,
    scale: Scale,
) -> (accesskit::Rect, accesskit::Rect) {
    let scale_factor = f64::from(scale.factor());
    let width = (f64::from(size.width) * scale_factor).ceil().max(1.0);
    let height = (f64::from(size.height) * scale_factor).ceil().max(1.0);
    let x = f64::from(origin.x);
    let y = f64::from(origin.y);
    let rect = accesskit::Rect {
        x0: x,
        y0: y,
        x1: x + width,
        y1: y + height,
    };
    (rect, rect)
}

#[cfg(test)]
mod tests {
    use accesskit::{ActivationHandler, DeactivationHandler};

    use super::{UnixActivation, UnixDeactivation, window_bounds};
    use crate::{
        app::App,
        math::{Point, Scale, Size},
        platform::dispatcher::Dispatcher,
        window::WindowOptions,
    };

    fn scale(factor: f32) -> Scale {
        Scale::new(factor).expect("scale")
    }

    #[test]
    fn activation_wakes_the_main_thread_and_forces_the_next_full_tree() {
        use std::sync::Arc;

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
        let mut activation = UnixActivation {
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
            dispatcher.take_frame_wake(),
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

    #[test]
    fn deactivation_bumps_the_consumer_epoch() {
        let epoch = crate::a11y::adapter::A11yBridge::new().consumer_epoch();
        let before = epoch.current();
        let mut deactivation = UnixDeactivation {
            epoch: epoch.clone(),
        };
        deactivation.deactivate_accessibility();
        assert_eq!(epoch.current(), before + 1);
    }

    #[test]
    fn window_bounds_are_physical_pixels() {
        let (outer, inner) = window_bounds(Point::ZERO, Size::new(400.0, 300.0), scale(2.0));
        assert_eq!(
            (inner.x0, inner.y0, inner.x1, inner.y1),
            (0.0, 0.0, 800.0, 600.0)
        );
        assert_eq!(outer, inner, "无系统装饰时 outer == inner");
    }

    #[test]
    fn fractional_scale_is_rounded_up() {
        let (_, inner) = window_bounds(Point::ZERO, Size::new(400.0, 300.0), scale(1.25));
        assert_eq!((inner.x1, inner.y1), (500.0, 375.0));
        let (_, odd) = window_bounds(Point::ZERO, Size::new(401.0, 301.0), scale(1.25));
        assert_eq!(
            (odd.x1, odd.y1),
            (502.0, 377.0),
            "401×1.25 = 501.25 ⇒ 覆盖取整 502"
        );
    }

    #[test]
    fn the_screen_origin_is_passed_through() {
        let (_, inner) = window_bounds(Point::new(120.0, 48.0), Size::new(100.0, 50.0), Scale::ONE);
        assert_eq!((inner.x0, inner.y0), (120.0, 48.0));
        assert_eq!((inner.x1, inner.y1), (220.0, 98.0));
    }
}
