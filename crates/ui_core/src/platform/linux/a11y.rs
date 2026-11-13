//! a11y 直连：`accesskit_unix::Adapter`（AT-SPI over D-Bus，`design/phase-12-macos-linux.md` §8.7）。
//!
//! 与 Windows / macOS 的差异（F-124）：`update_if_active` **无返回值**；adapter 自己起 tokio 任务
//! 与 D-Bus 注册（`accesskit_unix` 的 `tokio` feature），因此 `new` 必须在 runtime 内调用。

use std::sync::Arc;

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, TreeUpdate};
use accesskit_unix::Adapter;

use crate::{
    error::EngineError,
    platform::{
        dispatcher::{Dispatcher, UserTask},
        linux::dpi,
    },
    window::WindowId,
};

/// 窗口的 a11y 状态（`event_loop` 持有；主线程独占）。
pub(crate) struct UnixA11y {
    adapter: Adapter,
}

impl UnixA11y {
    /// 建 adapter（D-Bus 注册在 adapter 内部异步完成）。
    ///
    /// # Errors
    /// 当前不产生错误（`accesskit_unix` 的注册失败只记录诊断，不影响渲染）。
    pub(crate) fn new(dispatcher: Arc<Dispatcher>, window: WindowId) -> Result<Self, EngineError> {
        let actions = UnixActions { dispatcher, window };
        Ok(Self {
            adapter: Adapter::new(UnixActivation, actions, UnixDeactivation),
        })
    }

    /// 交付一笔增量更新（闭包只在 adapter 活跃时执行 ⇒ 懒激活零开销）。
    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate) {
        self.adapter.update_if_active(factory);
    }

    /// 窗口在屏幕上的位置（Wayland 拿不到 ⇒ outer == inner，文档允许的退化）。
    pub(crate) fn set_bounds(&mut self, bounds: accesskit::Rect) {
        self.adapter.set_root_window_bounds(bounds, bounds);
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) {
        self.adapter.update_window_focus_state(focused);
    }
}

/// 激活：首棵全树由主线程在下一轮交付（`None` ⇒ AccessKit 先给 placeholder 根）。
struct UnixActivation;

impl ActivationHandler for UnixActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        None
    }
}

/// 停用：adapter 被对端关闭时回调（引擎没有需要清理的 a11y 状态）。
struct UnixDeactivation;

impl DeactivationHandler for UnixDeactivation {
    fn deactivate_accessibility(&mut self) {}
}

/// Action 回投：**只入队**（adapter 的 D-Bus 回调线程 ⇒ 主线程的 `Dispatcher::push`）。
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

/// 供 `event_loop` 复用的窗口尺寸换算（逻辑 → 物理，供 `accesskit` 的 bounds 使用）。
pub(crate) fn window_bounds(size: crate::math::Size, scale: crate::math::Scale) -> accesskit::Rect {
    let (width, height) = dpi::physical_size(size, scale);
    accesskit::Rect {
        x0: 0.0,
        y0: 0.0,
        x1: f64::from(width),
        y1: f64::from(height),
    }
}
