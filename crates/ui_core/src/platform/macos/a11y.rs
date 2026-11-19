//! a11y 直连：`accesskit_macos::Adapter` + `NSView` 的 NSAccessibility 回调。
//!
//! # 坐标系（NSAccessibility 的屏幕逻辑点）
//!
//! `accesskit_macos` 的 `to_ns_rect`（`util.rs`）把节点 bounds **除以**
//! `window.backingScaleFactor()` 换成逻辑点，再经 `view.convertRect_toView` +
//! `window.convertRectToScreen` 得到屏幕绝对矩形（逐条核对过 `accesskit_macos-0.27.2`
//! 源码）。由此得到引擎侧的两条硬要求：
//!
//! 1. 节点 bounds 必须是 **`GuiView` 相对、物理像素、y 向下**（`GuiView` 是 flipped，
//!    accesskit 按 `isFlipped` 分支处理的正是这一点）；
//! 2. 不需要额外传窗口原点 —— macOS 的换算全在 adapter 内部，引擎给 view 相对坐标即可。
//!
//! 真机验证项（本仓 CI 断言不到）：VoiceOver 的 `AXFrame` 与窗口实际屏幕位置一致
//! （外接 Retina 屏 + 非整数缩放拖动窗口后再读一次）。
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
    a11y::adapter::A11yConsumerEpoch,
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
    /// `epoch` 是窗口的消费端代际句柄：NSAccessibility 重新激活时激活回调 `bump()` 它，
    /// 引擎据此把下一笔升成全树（E1）。macOS 当前没有 deactivation 路径，统一后即使未来
    /// 出现 AX 客户端重连也安全。
    ///
    /// # Errors
    /// `EngineError::A11yInit`：不在主线程或 view 指针无效。
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
        // SAFETY: `pointer` 指向本窗口持有的 `GuiView`（`NativeWindow` 的生命周期覆盖 adapter），
        // 且已确认在主线程 —— 这正是 `Adapter::new` 文档要求的两条前提。
        let adapter = unsafe { Adapter::new(pointer, false, actions) };
        Ok(Self {
            adapter,
            activation,
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
///
/// E1：`bump()` 消费端代际并唤醒主线程，保证"占位根之后的第一笔"是全树。回调在主线程的
/// AX 请求里（`accessibilityChildren` / `hitTest` 的 `&mut self.activation`），但不能同步构造
/// `TreeUpdate`（那要碰 `App`，而 AX 回调可能重入），所以仍是异步交付。
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
