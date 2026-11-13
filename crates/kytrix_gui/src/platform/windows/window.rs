//! 原生窗口：创建（初始隐藏）/ 显示 / 客户区尺寸 / 最小尺寸 / 销毁
//! （`design/phase-09-win32-demo.md` §6）。
//!
//! P09 使用**系统标题栏**（`WS_OVERLAPPEDWINDOW`，`WM_NCCALCSIZE` 交 `DefWindowProcW`）；
//! 自绘标题栏留 P12。

use std::sync::Arc;

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::HBRUSH,
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

use crate::{
    error::EngineError,
    math::{Rect, Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        windows::{
            dpi,
            event_loop::{WndProcState, wndproc},
        },
    },
    window::{WindowId, WindowOptions},
};

/// 窗口类名（每进程注册一次）。
const CLASS_NAME: &str = "KytrixGuiWindow";

/// 最小客户区（逻辑像素；`WM_GETMINMAXINFO` 换算成物理像素后生效）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// 类光标（`IDC_ARROW`）：probe 实测「类光标 = null + `HTCLIENT`」会得到**空光标**（不可见）。
fn load_arrow_cursor() -> HCURSOR {
    // SAFETY: 共享的系统光标句柄（进程不拥有）；`IDC_ARROW` 是静态常量。
    unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default()
}

/// 一个原生窗口（`Application::run` 持有；`Drop` 时销毁）。
pub(crate) struct NativeWindow {
    id: WindowId,
    hwnd: HWND,
    /// `GWLP_USERDATA` 指向它；`Drop` 时回收。
    state: *mut WndProcState,
    scale: Scale,
    logical_size: Size,
}

impl NativeWindow {
    /// 创建**隐藏**窗口并把客户区调到 `options.size`（逻辑像素 × 平台 DPI）。
    ///
    /// 失败一律 `EngineError::WindowCreate`（`CreateWindowExW` / `RegisterClassW` 的 HRESULT 与
    /// `GetLastError` 都写进 `detail`）。
    pub(crate) fn create(
        id: WindowId,
        options: &WindowOptions,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        // SAFETY: 取本进程模块句柄（`None` = 当前可执行文件）；只读，不转移所有权。
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|error| window_error("GetModuleHandleW", error))?;
        register_class(instance)?;

        let class = wide(CLASS_NAME);
        let title = wide(&options.title);
        // 先用**请求的 scale** 估算一次窗口尺寸（真实 DPI 要窗口存在后才能问）。
        let provisional = dpi::window_size_for_client(
            physical_client(options.size, options.scale),
            dpi::system_dpi(),
        );
        // SAFETY: `class` / `title` 是本函数栈上的 NUL 结尾 UTF-16 缓冲，`CreateWindowExW`
        // 同步拷贝；父窗口/菜单为空，实例句柄来自本进程。窗口此时**隐藏**（无 `WS_VISIBLE`）。
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                provisional.0,
                provisional.1,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(|error| window_error("CreateWindowExW", error))?;

        let state = match WndProcState::new(dispatcher, hwnd, id) {
            Ok(state) => Box::into_raw(Box::new(state)),
            Err(error) => {
                // a11y adapter 构造失败（`A11yInit`）：窗口还没显示，直接销毁，避免泄漏 HWND。
                // SAFETY: 窗口刚由本函数创建，尚未交给任何其它所有者。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        // SAFETY: `hwnd` 刚创建且本函数独占；`state` 的所有权转移给 `NativeWindow`（Drop 回收）。
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };

        let mut window = Self {
            id,
            hwnd,
            state,
            scale: Scale::ONE,
            logical_size: options.size,
        };
        window.apply_dpi_and_size(options.size)?;
        Ok(window)
    }

    /// 句柄的 `isize` 形式（引擎内部唯一允许的「无类型」传递）。
    pub(crate) fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// 创建者传入的窗口 id。
    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    /// 当前 DPI 比例。
    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    /// 当前客户区逻辑尺寸。
    pub(crate) fn logical_size(&self) -> Size {
        self.logical_size
    }

    /// 客户区物理尺寸（创建渲染器 / resize 的唯一输入）。
    pub(crate) fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        // SAFETY: 窗口存活期内查询；`rect` 是栈上值。
        let ok = unsafe { GetClientRect(self.hwnd, &mut rect) };
        match ok {
            Ok(()) => (
                (rect.right - rect.left).max(1) as u32,
                (rect.bottom - rect.top).max(1) as u32,
            ),
            Err(_) => physical_client(self.logical_size, self.scale),
        }
    }

    /// 显示窗口（`ShowWindow(SW_SHOWNORMAL)`）。
    pub(crate) fn show(&self) {
        // SAFETY: 窗口存活；返回值表示「之前是否可见」，这里不需要。
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNORMAL);
        }
    }

    /// 应用 DPI / 尺寸：先问 DPI，再按 `AdjustWindowRectExForDpi` 调窗口，最后回读客户区。
    fn apply_dpi_and_size(&mut self, size: Size) -> Result<(), EngineError> {
        let dpi = dpi::window_dpi(self.hwnd);
        let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
            factor: dpi as f32 / dpi::BASE_DPI as f32,
        })?;
        let window_size = dpi::window_size_for_client(physical_client(size, scale), dpi);
        // SAFETY: 窗口存活；只改尺寸与位置，不重新创建。
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                100,
                100,
                window_size.0,
                window_size.1,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| window_error("SetWindowPos", error))?;
        // 回读：客户区由系统按真实边框算出，逻辑尺寸以它为准（round 之后可能与请求差 1 px）。
        let client = self.client_size();
        self.scale = scale;
        self.logical_size = Size::new(
            scale.to_logical(client.0 as f32),
            scale.to_logical(client.1 as f32),
        );
        let min = (
            (MIN_SIZE_LOGICAL.0 * scale.factor()).round() as u32,
            (MIN_SIZE_LOGICAL.1 * scale.factor()).round() as u32,
        );
        // SAFETY: `state` 由本窗口独占（`Drop` 之前一直有效）；WndProc 只在消息处理期读它。
        unsafe {
            (*self.state).set_min_size(min);
        }
        Ok(())
    }

    /// 用系统建议的矩形移动/缩放（`WM_DPICHANGED` 的 `lParam`）。
    pub(crate) fn place(&self, rect: (i32, i32, i32, i32)) -> Result<(), EngineError> {
        // SAFETY: 窗口存活；矩形来自系统建议，只改位置与尺寸。
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                rect.0,
                rect.1,
                rect.2 - rect.0,
                rect.3 - rect.1,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| window_error("SetWindowPos(WM_DPICHANGED)", error))?;
        Ok(())
    }

    /// `WndProc` 状态的独占访问（事件循环消费事件 / 读修饰键）。
    pub(crate) fn state_mut(&mut self) -> &mut WndProcState {
        // SAFETY: `state` 由 `create` 的 `Box::into_raw` 产生、`Drop` 回收；窗口存活期内唯一所有者。
        unsafe { &mut *self.state }
    }

    /// 交付一笔 a11y 更新（`pump` 每轮调用；`a11y_pending` 为 O(1) 过滤）。
    ///
    /// `build_update` 只在 `update_if_active` 的闭包里执行 ⇒ 没有辅助技术时**没有任何**
    /// `accesskit::Node` 构造（懒激活零开销）。
    pub(crate) fn update_a11y(&mut self, app: &mut crate::app::App) {
        let window = self.id;
        if !app.a11y_pending(window) {
            return;
        }
        // 短借用：`QueuedEvents::raise` 会**同步**派发嵌套 `WM_GETOBJECT`，所以只能在不持有
        // `&mut WndProcState` 时调用（同 P11 的 `WM_GETOBJECT` / `focus_changed` 纪律）。
        // 丢弃 `Option<QueuedEvents>` 等于只交付首棵树、后续事件全丢（P14 审查 #1）。
        let events = {
            let state = self.state_mut();
            state
                .a11y_mut()
                .update(|| crate::a11y::adapter::build_update(app, window))
        };
        if let Some(events) = events {
            events.raise();
        }
    }

    /// 窗口是否已被销毁（`close_requested_windows` 的 retain 判据）。
    pub(crate) fn is_destroyed(&self) -> bool {
        self.hwnd.is_invalid()
    }

    /// 应用新的 DPI 比例（`WM_DPICHANGED`）：更新最小尺寸，并按新 DPI 重算窗口尺寸。
    pub(crate) fn apply_scale(&mut self, scale: Scale, dpi: u32) -> Result<(), EngineError> {
        self.scale = scale;
        let min = (
            (MIN_SIZE_LOGICAL.0 * scale.factor()).round() as u32,
            (MIN_SIZE_LOGICAL.1 * scale.factor()).round() as u32,
        );
        // SAFETY: `state` 独占；只写 `min_size`。
        unsafe {
            (*self.state).set_min_size(min);
        }
        // 让窗口按新的 DPI 重新计算非客户区（`WM_NCCALCSIZE` 由系统标题栏处理）。
        let window_size =
            dpi::window_size_for_client(physical_client(self.logical_size, scale), dpi);
        // SAFETY: 窗口存活；只改尺寸，不重建。
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                100,
                100,
                window_size.0,
                window_size.1,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| window_error("SetWindowPos(WM_DPICHANGED)", error))?;
        Ok(())
    }

    /// 同步 `logical_size` 字段（客户区被用户拖动改变之后由事件循环调用）。
    pub(crate) fn note_client_resize(&mut self, client: (u32, u32)) {
        let scale = self.scale;
        self.logical_size = Size::new(
            scale.to_logical(client.0 as f32),
            scale.to_logical(client.1 as f32),
        );
    }

    /// 销毁窗口（幂等；`Drop` 也会调）。
    pub(crate) fn destroy(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        // SAFETY: 窗口存活；销毁后把句柄置空避免二次销毁。
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        self.hwnd = HWND::default();
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        self.destroy();
        if !self.state.is_null() {
            // SAFETY: 指针由 `create` 里的 `Box::into_raw` 产生，只在这里回收一次。
            unsafe {
                drop(Box::from_raw(self.state));
            }
            self.state = std::ptr::null_mut();
        }
    }
}

/// 逻辑尺寸 × scale → 物理客户区（足够大，至少 1 px）。
fn physical_client(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

/// 注册窗口类；`ERROR_CLASS_ALREADY_EXISTS`（1410）视为成功。
fn register_class(instance: windows::Win32::Foundation::HMODULE) -> Result<(), EngineError> {
    let class = wide(CLASS_NAME);
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance.into(),
        hIcon: HICON::default(),
        // 类光标必须是箭头：probe 实测「类光标 = null + HTCLIENT」会得到**空光标**（不可见），
        // 而 `DefWindowProcW` 会用类光标兜底客户区与非客户区（P10 F-96）。
        hCursor: load_arrow_cursor(),
        // 引擎自己画每一帧：不擦背景，避免 `WM_ERASEBKGND` 闪白。
        hbrBackground: HBRUSH::default(),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: windows::core::PCWSTR(class.as_ptr()),
    };
    // SAFETY: `wc` 的字符串指针指向本函数栈上的 `class`，`RegisterClassW` 同步拷贝。
    let atom = unsafe { RegisterClassW(&wc) };
    if atom != 0 {
        return Ok(());
    }
    let error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
    if error == 1410 {
        return Ok(());
    }
    Err(EngineError::WindowCreate {
        detail: smol_str::format_smolstr!("RegisterClassW: GetLastError={error}"),
    })
}

fn window_error(stage: &'static str, error: windows::core::Error) -> EngineError {
    EngineError::WindowCreate {
        detail: smol_str::format_smolstr!("{stage}: {:#010X}", error.code().0 as u32),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
