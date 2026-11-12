//! a11y 直连：`accesskit_macos::Adapter` + `NSView` 的 NSAccessibility 回调
//! （`design/phase-12-macos-linux.md` §7.7）。
//!
//! 三条纪律（probe 验证）：
//! 1. `Adapter::new` 是 `unsafe` 且**必须在主线程**（内部 `MainThreadMarker::new().unwrap()`）；
//! 2. `hit_test` / `view_children` / `focus` 返回**裸指针**（autoreleased），点是 **objc2 0.2 线**的
//!    `NSPoint` ⇒ 用 `Retained::retain(raw.cast())` 桥到 0.6 线；
//! 3. `raise()` 必须在借用释放之后调用（raise 期间可能重入 NSAccessibility）。

use std::{ffi::c_void, sync::Arc};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use accesskit_macos::Adapter;
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_foundation::NSArray;

use crate::{
    error::EngineError,
    platform::{
        dispatcher::{Dispatcher, UserTask},
        macos::view::GuiView,
    },
    window::WindowId,
};

/// 窗口的 a11y 状态（`GuiView` 的 ivars 持有；主线程独占）。
pub(crate) struct MacA11y {
    adapter: Adapter,
    activation: MacActivation,
}

impl MacA11y {
    /// 在窗口**首次显示之前**调用（主线程）。
    ///
    /// # Errors
    /// `EngineError::A11yInit`：不在主线程或 view 指针无效。
    pub(crate) fn new(
        view: &GuiView,
        dispatcher: Arc<Dispatcher>,
        window: WindowId,
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
        let actions = MacActions { dispatcher, window };
        // SAFETY: `pointer` 指向本窗口持有的 `GuiView`（`NativeWindow` 的生命周期覆盖 adapter）；
        // 已确认在主线程，符合 `Adapter::new` 的文档要求。
        let adapter = unsafe { Adapter::new(pointer, false, actions) };
        Ok(Self {
            adapter,
            activation: MacActivation,
        })
    }

    /// 交付一笔增量更新（闭包只在 adapter 活跃时执行 ⇒ 懒激活零开销）。
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

    /// `accessibilityHitTest:` 的载荷（屏幕无关的 view 局部点，`isFlipped` 已对齐引擎坐标）。
    pub(crate) fn hit_test(&mut self, x: f64, y: f64) -> Option<Retained<AnyObject>> {
        let point = accesskit_macos::NSPoint { x, y };
        let raw = self.adapter.hit_test(point, &mut self.activation);
        if raw.is_null() {
            None
        } else {
            // SAFETY: 返回的是 autoreleased 的 ObjC 对象；retain 后由 `Retained` 管理。
            unsafe { Retained::retain(raw.cast::<AnyObject>()) }
        }
    }

    /// `accessibilityChildren` 的载荷（子元素数组）。
    pub(crate) fn children(&mut self) -> Option<Retained<NSArray>> {
        let raw = self.adapter.view_children(&mut self.activation);
        if raw.is_null() {
            None
        } else {
            // SAFETY: 同上；`NSArray<NSObject>`（objc2 0.2 线）与 `NSArray`（0.6 线）指针布局相同。
            unsafe { Retained::retain(raw.cast::<NSArray>()) }
        }
    }

    /// `accessibilityFocusedUIElement` 的载荷；空 = 没有焦点元素。
    pub(crate) fn focused(&mut self) -> Option<Retained<AnyObject>> {
        let raw = self.adapter.focus(&mut self.activation);
        if raw.is_null() {
            None
        } else {
            // SAFETY: 同 `hit_test`。
            unsafe { Retained::retain(raw.cast::<AnyObject>()) }
        }
    }
}

impl std::fmt::Debug for MacA11y {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacA11y").finish_non_exhaustive()
    }
}

/// 激活：首棵全树由主线程在下一轮交付（`None` ⇒ AccessKit 先给 placeholder 根）。
pub(crate) struct MacActivation;

impl ActivationHandler for MacActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        None
    }
}

/// Action 回投：**只入队**（adapter 的回调线程 ⇒ 主线程的 `Dispatcher::push`）。
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
