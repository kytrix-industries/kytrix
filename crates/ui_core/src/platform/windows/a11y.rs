//! a11y 直连：`accesskit_windows::Adapter` + `WM_GETOBJECT` 接线
//! （`design/phase-11-a11y.md` §8）。
//!
//! 三条纪律：
//! 1. **首显前构造**：`Adapter::new` 必须在 `ShowWindow` 之前调用（它内部 `init_uia()`，
//!    `adapter.rs:411-441`）；`SubclassingAdapter` 不用——引擎自己的 `WndProc` 已经存在。
//! 2. **借用在 `UiaReturnRawElementProvider` 之前释放**：`handle_wm_getobject` 的返回值转
//!    `LRESULT` 时会同步派发嵌套 `WM_GETOBJECT`（`adapter.rs:530-533`），照
//!    `accesskit_windows::SubclassingAdapter` 的写法先释放借用再 `.into()`（probe ③ 编译验证）。
//! 3. **Action 只入队**：`ActionHandler` 可能在任意线程（`adapter.rs:413-414`）⇒ 只
//!    `Dispatcher::push`，主线程 drain 时由 `a11y::action::dispatch` 走标准失效闭环。

use std::{cell::Cell, sync::Arc};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use accesskit_windows::{Adapter, QueuedEvents};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::IsWindow,
};

use crate::{
    error::EngineError,
    platform::dispatcher::{Dispatcher, UserTask},
    window::WindowId,
};

/// 一个原生窗口的 a11y 状态（挂在 `WndProcState` 上）。
pub(crate) struct WindowsA11y {
    adapter: Adapter,
    activation: WindowsActivation,
    /// 重入防御（`UiaReturnRawElementProvider` 期间的嵌套 `WM_GETOBJECT`）。
    in_getobject: Cell<bool>,
}

impl WindowsA11y {
    /// 在窗口**显示之前**构造（`NativeWindow::create` 里、`ShowWindow` 之前）。
    ///
    /// # Errors
    /// `EngineError::A11yInit`：句柄无效（adapter 只能在有效窗口上构造）。
    pub(crate) fn new(
        hwnd: HWND,
        dispatcher: Arc<Dispatcher>,
        window: WindowId,
    ) -> Result<Self, EngineError> {
        // SAFETY: 只查窗口有效性，不改任何状态。
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(EngineError::A11yInit {
                detail: smol_str::SmolStr::new_static("IsWindow(hwnd) == false"),
            });
        }
        let action_handler = WindowsActionHandler { dispatcher, window };
        Ok(Self {
            // `is_window_focused = false`：窗口尚未显示；随后由 `WM_SETFOCUS` 纠正。
            adapter: Adapter::new(hwnd, false, action_handler),
            activation: WindowsActivation,
            in_getobject: Cell::new(false),
        })
    }

    /// 进入 `WM_GETOBJECT` 处理：`false` = 重入（本次不接管）。
    ///
    /// 旗标由调用方在**释放 `&mut WndProcState` 之前**置位、在 `.into()` **之后**清除：
    /// `UiaReturnRawElementProvider` 会同步派发嵌套 `WM_GETOBJECT`（`adapter.rs:530-533`），
    /// 那个嵌套调用只能看到旗标、不得再进第二层。
    pub(crate) fn begin_getobject(&mut self) -> bool {
        if self.in_getobject.get() {
            return false;
        }
        self.in_getobject.set(true);
        true
    }

    /// 退出 `WM_GETOBJECT` 处理（在 `.into()` 之后调用）。
    pub(crate) fn end_getobject(&mut self) {
        self.in_getobject.set(false);
    }

    /// 处理 `WM_GETOBJECT` 的**未转换**结果。
    ///
    /// 返回值必须在 `&mut WndProcState` 释放之后才 `.into()`：`UiaReturnRawElementProvider`
    /// 会重新进入 `WndProc`，此时再持有 `&mut` 就是别名 UB（P11 审查 #5）。
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

    /// 窗口焦点态同步（`WM_SETFOCUS` / `WM_KILLFOCUS` / 菜单与缩放循环）。
    ///
    /// 返回待 raise 的事件：调用方必须**先释放 `&mut WndProcState`** 再 `raise()`
    /// （`raise` 期间可能派发嵌套 `WM_GETOBJECT`）。
    pub(crate) fn focus_changed(&mut self, focused: bool) -> Option<QueuedEvents> {
        self.adapter.update_window_focus_state(focused)
    }

    /// 交付一笔 a11y 更新（`factory` 只在 adapter 活跃时执行 ⇒ 懒激活零开销）。
    ///
    /// 同样返回待 raise 的事件（raise 由调用方在借用释放后做）。
    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate) -> Option<QueuedEvents> {
        self.adapter.update_if_active(factory)
    }
}

impl std::fmt::Debug for WindowsA11y {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsA11y")
            .field("in_getobject", &self.in_getobject.get())
            .finish_non_exhaustive()
    }
}

/// 激活：首棵全树由主线程在下一轮交付（`AccessKit` 先用 `PLACEHOLDER_ROOT_ID` 顶上）。
///
/// 这里恒返回 `None`——`WndProc` 不碰 `App`（P09 §5.2 的纪律），要同步返回全树就得在平台层缓存
/// 一整棵 `TreeUpdate`（第二份真值）。占位一帧是 AccessKit 明确支持的用法（`lib.rs:3351-3358`）。
struct WindowsActivation;

impl ActivationHandler for WindowsActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        None
    }
}

/// Action 回投：**只入队**（UIA 线程 ⇒ 主线程的 `Dispatcher::push`）。
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

    use super::WindowsA11y;
    use crate::{platform::dispatcher::Dispatcher, window::WindowId};

    unsafe extern "system" fn wndproc(
        hwnd: windows::Win32::Foundation::HWND,
        msg: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        // SAFETY: 本测试的窗口只用于 a11y 构造，默认过程不依赖额外状态。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 建一个**隐藏**窗口（不 `ShowWindow`）并返回句柄。
    fn hidden_window(tag: &str) -> (windows::Win32::Foundation::HWND, Vec<u16>) {
        let class_name = wide(tag);
        // SAFETY: 进程级模块句柄；类名是 NUL 结尾的宽字符串，`RegisterClassW` 同步拷贝。
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // SAFETY: 同上；重复注册返回 0，本测试每个 tag 只注册一次。
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
        // SAFETY: 类已注册；`WS_OVERLAPPEDWINDOW` 不含 `WS_VISIBLE` ⇒ 窗口保持隐藏。
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

    #[test]
    fn adapter_constructs_before_the_window_is_shown() {
        let _runtime = runtime();
        let (hwnd, _class) = hidden_window("KytrixP11A11yHidden");
        // SAFETY: 只读窗口可见性。
        assert!(
            !unsafe { IsWindowVisible(hwnd) }.as_bool(),
            "本用例必须在首显之前构造 adapter"
        );
        let a11y = WindowsA11y::new(hwnd, Dispatcher::new(), WindowId::for_test(1));
        assert!(a11y.is_ok(), "隐藏窗口上必须能构造 adapter（首显前）");
        let mut a11y = a11y.expect("adapter");
        a11y.focus_changed(true);
        a11y.focus_changed(false);
        // SAFETY: 窗口由本用例创建；销毁后不再访问。
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn update_if_active_is_lazy_without_assistive_tech() {
        let _runtime = runtime();
        let (hwnd, _class) = hidden_window("KytrixP11A11yLazy");
        let mut a11y =
            WindowsA11y::new(hwnd, Dispatcher::new(), WindowId::for_test(2)).expect("adapter");
        // 无辅助技术连接时 adapter 恒 Inactive ⇒ 闭包**一次都不执行**（懒激活零开销）。
        // 注：本断言在「有 AT 正在运行的机器」上不成立，CI 无桌面环境恒成立（V-75 的已知边界）。
        let mut called = false;
        a11y.update(|| {
            called = true;
            empty_update()
        });
        assert!(!called, "Inactive 时不得构造 TreeUpdate");
        // SAFETY: 同上。
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn wm_getobject_with_an_unrelated_objid_falls_through() {
        let _runtime = runtime();
        let (hwnd, _class) = hidden_window("KytrixP11A11yObjid");
        let mut a11y =
            WindowsA11y::new(hwnd, Dispatcher::new(), WindowId::for_test(3)).expect("adapter");
        // OBJID_WINDOW / 其它负数 objid 不归 AccessKit 管 ⇒ 交回 DefWindowProcW。
        let result = a11y.wm_getobject(
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(-1),
        );
        assert!(result.is_none(), "非 client/root 的 objid 必须返回 None");
        // SAFETY: 同上。
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
