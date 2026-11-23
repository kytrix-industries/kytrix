use std::{cell::Cell, sync::Arc};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use accesskit_windows::{Adapter, QueuedEvents};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::IsWindow,
};

use crate::{
    a11y::adapter::A11yConsumerEpoch,
    error::EngineError,
    platform::dispatcher::{Dispatcher, UserTask},
    window::WindowId,
};

pub(crate) struct WindowsA11y {
    adapter: Adapter,
    activation: WindowsActivation,
    in_getobject: Cell<bool>,
}

impl WindowsA11y {
    pub(crate) fn new(
        hwnd: HWND,
        dispatcher: Arc<Dispatcher>,
        window: WindowId,
        epoch: A11yConsumerEpoch,
    ) -> Result<Self, EngineError> {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(EngineError::A11yInit {
                detail: smol_str::SmolStr::new_static("IsWindow(hwnd) == false"),
            });
        }
        let activation = WindowsActivation {
            epoch,
            dispatcher: Arc::clone(&dispatcher),
        };
        let action_handler = WindowsActionHandler { dispatcher, window };
        Ok(Self {
            adapter: Adapter::new(hwnd, false, action_handler),
            activation,
            in_getobject: Cell::new(false),
        })
    }

    pub(crate) fn begin_getobject(&mut self) -> bool {
        if self.in_getobject.get() {
            return false;
        }
        self.in_getobject.set(true);
        true
    }

    pub(crate) fn end_getobject(&mut self) {
        self.in_getobject.set(false);
    }

    pub(crate) fn wm_getobject(
        &mut self,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<impl Into<LRESULT>> {
        let Self {
            adapter,
            activation,
            ..
        } = self;
        adapter.handle_wm_getobject(wparam, lparam, activation)
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) -> Option<QueuedEvents> {
        self.adapter.update_window_focus_state(focused)
    }

    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate) -> Option<QueuedEvents> {
        self.adapter.update_if_active(factory)
    }

    #[cfg(test)]
    pub(crate) fn consumer_epoch(&self) -> &A11yConsumerEpoch {
        &self.activation.epoch
    }
}

impl std::fmt::Debug for WindowsA11y {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsA11y")
            .field("in_getobject", &self.in_getobject.get())
            .finish_non_exhaustive()
    }
}

struct WindowsActivation {
    epoch: A11yConsumerEpoch,
    dispatcher: Arc<Dispatcher>,
}

impl ActivationHandler for WindowsActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.epoch.bump();
        self.dispatcher.wake();
        None
    }
}

struct WindowsActionHandler {
    dispatcher: Arc<Dispatcher>,
    window: WindowId,
}

impl ActionHandler for WindowsActionHandler {
    fn do_action(&mut self, request: ActionRequest) {
        let window = self.window;
        self.dispatcher.push(UserTask::new(move |app| {
            crate::a11y::action::dispatch(app, window, request);
        }));
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::{
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindowVisible, RegisterClassW,
            WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        },
    };

    use super::{WindowsA11y, WindowsActivation};
    use crate::{
        a11y::adapter::A11yConsumerEpoch,
        app::App,
        platform::dispatcher::Dispatcher,
        window::{WindowId, WindowOptions},
    };

    unsafe extern "system" fn wndproc(
        hwnd: windows::Win32::Foundation::HWND,
        msg: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hidden_window(tag: &str) -> (windows::Win32::Foundation::HWND, Vec<u16>) {
        let class_name = wide(tag);
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class_name.as_ptr()),
                windows::core::PCWSTR(class_name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .expect("CreateWindowExW");
        (hwnd, class_name)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn test_epoch() -> A11yConsumerEpoch {
        crate::a11y::adapter::A11yBridge::new().consumer_epoch()
    }

    #[test]
    fn adapter_constructs_before_the_window_is_shown() {
        let _runtime = runtime();
        let (hwnd, _class) = hidden_window("KytrixP11A11yHidden");
        assert!(
            !unsafe { IsWindowVisible(hwnd) }.as_bool(),
            "本用例必须在首显之前构造 adapter"
        );
        let a11y = WindowsA11y::new(hwnd, Dispatcher::new(), WindowId::for_test(1), test_epoch());
        assert!(a11y.is_ok(), "隐藏窗口上必须能构造 adapter（首显前）");
        let mut a11y = a11y.expect("adapter");
        a11y.focus_changed(true);
        a11y.focus_changed(false);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn update_if_active_is_lazy_without_assistive_tech() {
        let _runtime = runtime();
        let (hwnd, _class) = hidden_window("KytrixP11A11yLazy");
        let mut a11y =
            WindowsA11y::new(hwnd, Dispatcher::new(), WindowId::for_test(2), test_epoch())
                .expect("adapter");
        let mut called = false;
        a11y.update(|| {
            called = true;
            empty_update()
        });
        assert!(!called, "Inactive 时不得构造 TreeUpdate");
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn wm_getobject_with_an_unrelated_objid_falls_through() {
        let _runtime = runtime();
        let (hwnd, _class) = hidden_window("KytrixP11A11yObjid");
        let mut a11y =
            WindowsA11y::new(hwnd, Dispatcher::new(), WindowId::for_test(3), test_epoch())
                .expect("adapter");
        let result = a11y.wm_getobject(
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(-1),
        );
        assert!(result.is_none(), "非 client/root 的 objid 必须返回 None");
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn activation_wakes_the_main_thread_and_forces_the_next_full_tree() {
        use accesskit::ActivationHandler;

        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions::default());
        let epoch = app.window_a11y_epoch(window).expect("窗口存在");
        let before = epoch.current();
        let dispatcher = Dispatcher::new();
        let _ = dispatcher.take_frame_wake();
        let mut activation = WindowsActivation {
            epoch: epoch.clone(),
            dispatcher: std::sync::Arc::clone(&dispatcher),
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
    fn the_adapter_shares_the_window_consumer_epoch() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions::default());
        let epoch = app.window_a11y_epoch(window).expect("窗口存在");
        let (hwnd, _class) = hidden_window("KytrixP11A11ySharedEpoch");
        let a11y =
            WindowsA11y::new(hwnd, Dispatcher::new(), window, epoch.clone()).expect("adapter");
        assert!(
            a11y.consumer_epoch().same_handle(&epoch),
            "adapter 必须持有窗口 bridge 的同一份 epoch（否则回调 bump 到不了引擎）"
        );
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    fn empty_update() -> accesskit::TreeUpdate {
        accesskit::TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: accesskit::TreeId::ROOT,
            focus: accesskit::NodeId(1u64 << 32),
        }
    }
}
