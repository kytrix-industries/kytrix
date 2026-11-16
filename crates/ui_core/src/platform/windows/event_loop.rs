//! Windows 事件循环：`WndProc`、消息泵、唤醒合并、窗口生命周期
//! （`design/phase-09-win32-demo.md` §5.2 / §5.4）。
//!
//! 两条实测约束（probe）：
//! 1. `PeekMessageW` 会**取出并消费** `WM_QUIT`，泵必须在取出后显式判 `WM_QUIT` 再退出，
//!    否则后续 `GetMessageW` 永久阻塞（DEVIATIONS F-90）。
//! 2. `WM_PAINT` 不验证会形成风暴（100 ms / 24,576 条，V-63）：本文件的 `WM_PAINT` 分支只做
//!    `BeginPaint`/`EndPaint`，不在这里绘制。
//!
//! `WndProc` **不做**任何 `App` 调用：它只把 [`PlatformEvent`] 压进 `WndProcState`，泵在
//! `DispatchMessageW` 返回后统一消费（Rust 借用规则 + 保持 `WndProc` 廉价）。

use std::sync::Arc;

use windows::Win32::{
    Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient},
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{
            GetDoubleClickTime, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT,
            TrackMouseEvent,
        },
        WindowsAndMessaging::*,
    },
};

use crate::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent},
    math::PhysicalRect,
    platform::{
        dispatcher::{Dispatcher, WM_APP_COMPOSITION_WAKE, WM_APP_FRAME_WAKE},
        event::{
            PlatformEvent, PointerKind, TranslateState, hiword_signed, loword_signed, translate,
        },
        windows::{composition, dpi, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{Window, WindowId, WindowOptions},
};

/// 连续帧定时器 id（动画 / P17 HUD 共用；`wndproc` 用它唤醒；P09 probe 实测 1 ms → 16.0 ms）。
const FRAME_TIMER_ID: usize = 1;

/// 自绘边框缩放的合并定时器 id（与 [`FRAME_TIMER_ID`] 分开）。
///
/// 1 ms 定时器在 P09 probe 里实测合并到 16.0 ms ⇒ ~60 Hz：鼠标以 1000 Hz 上报时，窗口矩形与
/// DComp 几何、`App::frame` 都只按这个节奏推进（P1-2b）。`WM_TIMER` 是低优先级消息，连续的
/// 鼠标消息可能让它延迟；抬起 / 失去捕获时的最终 apply 保证窗口不会停在旧位置（不忙等）。
const RESIZE_TIMER_ID: usize = 2;

/// 用系统设置初始化输入阈值（双击时间 + 拖拽/双击的距离容差，§5.4）。
fn install_input_thresholds(app: &mut App) {
    // SAFETY: 两个都是无副作用、无指针参数的系统查询。
    let (double_click, drag_x, drag_y) = unsafe {
        (
            GetDoubleClickTime(),
            GetSystemMetrics(SM_CXDRAG),
            GetSystemMetrics(SM_CYDRAG),
        )
    };
    let distance = drag_x.max(drag_y).max(1) as f32;
    app.set_input_thresholds(crate::interaction::InputThresholds {
        double_click_time: std::time::Duration::from_millis(u64::from(double_click.max(1))),
        double_click_distance: distance,
        drag_distance: distance,
    });
}

/// 引擎光标 → 系统光标。`Default` 交给类光标（`IDC_ARROW`），不调 `SetCursor`。
fn cursor_handle(cursor: crate::paint::style::Cursor) -> HCURSOR {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENS, IDC_SIZEWE,
    };

    use crate::paint::style::Cursor;

    let id = match cursor {
        Cursor::Default => IDC_ARROW,
        Cursor::Pointer => IDC_HAND,
        Cursor::Text => IDC_IBEAM,
        Cursor::NotAllowed => IDC_NO,
        Cursor::Grab | Cursor::Grabbing => IDC_SIZEALL,
        Cursor::ResizeHorizontal => IDC_SIZEWE,
        Cursor::ResizeVertical => IDC_SIZENS,
    };
    // SAFETY: `LoadCursorW` 取共享的系统光标句柄（不拥有、不释放）；`id` 是本函数的静态常量。
    unsafe { LoadCursorW(None, id) }.unwrap_or_default()
}

/// `WM_NCHITTEST` 的可缩放边框区域。
fn is_resize_hit(hit: u32) -> bool {
    matches!(
        hit,
        HTLEFT | HTRIGHT | HTTOP | HTBOTTOM | HTTOPLEFT | HTTOPRIGHT | HTBOTTOMLEFT | HTBOTTOMRIGHT
    )
}

/// 缩放边框期间的系统光标。
fn resize_cursor(hit: u32) -> HCURSOR {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    };

    let id = match hit {
        HTLEFT | HTRIGHT => IDC_SIZEWE,
        HTTOP | HTBOTTOM => IDC_SIZENS,
        HTTOPLEFT | HTBOTTOMRIGHT => IDC_SIZENWSE,
        HTTOPRIGHT | HTBOTTOMLEFT => IDC_SIZENESW,
        _ => IDC_ARROW,
    };
    // SAFETY: 共享的系统光标句柄；不拥有、不释放。
    unsafe { LoadCursorW(None, id) }.unwrap_or_default()
}

/// 从缩放起点与当前鼠标位置算出新的窗口矩形（物理像素，含最小尺寸钳制）。
fn resized_rect(session: ResizeSession, cursor: POINT, min: (u32, u32)) -> RECT {
    let dx = cursor.x - session.cursor.x;
    let dy = cursor.y - session.cursor.y;
    let mut rect = session.window;
    let left = matches!(session.hit, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT);
    let right = matches!(session.hit, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT);
    let top = matches!(session.hit, HTTOP | HTTOPLEFT | HTTOPRIGHT);
    let bottom = matches!(session.hit, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT);
    if left {
        rect.left += dx;
    }
    if right {
        rect.right += dx;
    }
    if top {
        rect.top += dy;
    }
    if bottom {
        rect.bottom += dy;
    }

    let min_width = min.0.max(1) as i32;
    let min_height = min.1.max(1) as i32;
    if (left || right) && rect.right - rect.left < min_width {
        if left {
            rect.left = rect.right - min_width;
        } else {
            rect.right = rect.left + min_width;
        }
    }
    if (top || bottom) && rect.bottom - rect.top < min_height {
        if top {
            rect.top = rect.bottom - min_height;
        } else {
            rect.bottom = rect.top + min_height;
        }
    }
    rect
}

/// `WndProc` 的每窗口状态（`GWLP_USERDATA` 指向它）。
pub(crate) struct WndProcState {
    dispatcher: Arc<Dispatcher>,
    events: Vec<PlatformEvent>,
    translate: TranslateState,
    /// `WM_GETMINMAXINFO` 的 `ptMinTrackSize`（物理像素）。
    min_size: (u32, u32),
    close_requested: bool,
    /// 是否已注册 `TrackMouseEvent(TME_LEAVE)`（每进入一次客户区注册一次）。
    mouse_tracking: bool,
    /// 正在进行的自绘边框缩放（拦截 `WM_NCLBUTTONDOWN` 后不再进入 Win32 模态循环，
    /// 这样消息泵才能继续处理 `WM_SIZE` 并及时出帧）。
    resize: Option<ResizeSession>,
    /// a11y（P11：首显前构造的 `accesskit_windows::Adapter` + `WM_GETOBJECT` 接线）。
    a11y: crate::platform::windows::a11y::WindowsA11y,
    /// 引擎当前生效的光标（`App::take_cursor_change` 写入）。
    ///
    /// Windows 每次鼠标移动都发 `WM_SETCURSOR`，`DefWindowProcW` 会把光标重置成**类光标**
    /// （恒为 `IDC_ARROW`）—— 只在光标变化时 `SetCursor` 的写法会被系统覆盖掉，自定义光标
    /// 永远不生效（P2-3）。所以这里记住当前值，在 `WM_SETCURSOR` 里重新应用。
    cursor: crate::paint::style::Cursor,
}

/// 一次自绘边框缩放的起点 + 最新光标。
#[derive(Clone, Copy)]
struct ResizeSession {
    /// `WM_NCHITTEST` 的 `HT*` 值。
    hit: u32,
    /// 按下时的屏幕坐标（`resized_rect` 的基准，不随移动变化）。
    cursor: POINT,
    /// 按下时的窗口矩形（物理像素）。
    window: RECT,
    /// 最新一次 `WM_MOUSEMOVE` 的屏幕坐标；定时器 tick / 抬起时按它算窗口矩形。
    latest: POINT,
}

impl WndProcState {
    pub(crate) fn new(
        dispatcher: Arc<Dispatcher>,
        hwnd: HWND,
        window: WindowId,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            dispatcher: dispatcher.clone(),
            events: Vec::with_capacity(8),
            translate: TranslateState::default(),
            min_size: (1, 1),
            close_requested: false,
            mouse_tracking: false,
            resize: None,
            // **必须在窗口显示之前构造**：`Adapter::new` 内部 `init_uia()`，
            // 且不得在处理 `WM_GETOBJECT` 时构造（adapter.rs:411-441）。
            a11y: crate::platform::windows::a11y::WindowsA11y::new(hwnd, dispatcher, window)?,
            cursor: crate::paint::style::Cursor::Default,
        })
    }

    /// a11y 状态（`WM_GETOBJECT` / 焦点消息 / 每轮交付的唯一入口）。
    pub(crate) fn a11y_mut(&mut self) -> &mut crate::platform::windows::a11y::WindowsA11y {
        &mut self.a11y
    }

    pub(crate) fn set_min_size(&mut self, min: (u32, u32)) {
        self.min_size = min;
    }

    fn wake(&self) {
        self.dispatcher.wake();
    }

    fn push(&mut self, event: PlatformEvent) {
        self.events.push(event);
    }

    pub(crate) fn take_events(&mut self) -> Vec<PlatformEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    /// 设置 IME caret 区域（客户区物理像素）；由 `NativeWindow::sync_ime_cursor_area` 调用。
    pub(crate) fn set_ime_cursor_area(&mut self, hwnd: HWND, area: PhysicalRect) {
        self.translate.ime_mut().set_cursor_area(hwnd, area);
    }

    /// 清除 IME caret hint（不改变 IME 的启用状态）。
    pub(crate) fn clear_ime_cursor_area(&mut self) {
        self.translate.ime_mut().clear_cursor_area();
    }

    /// 当前修饰键（事件投递时随按键一起给应用层）。
    pub(crate) fn modifiers(&self) -> crate::event::Modifiers {
        self.translate.modifiers()
    }

    /// 复位瞬时输入状态（修饰键 / 半高代理对）。失焦时由 WndProc 调用（P2-6）。
    pub(crate) fn reset_transient_input(&mut self) {
        self.translate.reset_transient();
    }

    /// 记录引擎当前光标（`WM_SETCURSOR` 据此重新应用，P2-3）。
    pub(crate) fn set_cursor(&mut self, cursor: crate::paint::style::Cursor) {
        self.cursor = cursor;
    }

    /// 开始自绘边框缩放：记录起点并抓鼠标（不再把消息交给 `DefWindowProcW` 的模态循环）。
    ///
    /// 同时启动 [`RESIZE_TIMER_ID`] 合并定时器：`WM_MOUSEMOVE` 只更新最新光标，真正的
    /// `SetWindowPos` 由定时器 tick 合并成 ~60 Hz（P1-2b）。
    fn begin_resize(&mut self, hwnd: HWND, hit: u32) {
        let mut cursor = POINT::default();
        let mut window = RECT::default();
        // SAFETY: 两个输出指针都指向本函数栈上的值；`hwnd` 由当前消息给出且窗口存活。
        let captured = unsafe {
            GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut window).is_ok()
        };
        if !captured {
            return;
        }
        // SAFETY: `hwnd` 是本进程创建的窗口；捕获在抬起 / 取消时释放。
        unsafe { SetCapture(hwnd) };
        self.resize = Some(ResizeSession {
            hit,
            cursor,
            window,
            latest: cursor,
        });
        // 1 ms 定时器（probe：实测合并到 16.0 ms）。失败不重试：抬起时的最终 apply 仍保证
        // 窗口落在最后的位置，只是拖动中没有中间几何更新。
        // SAFETY: `hwnd` 存活；`KillTimer` 用同一个 id（`end_resize`）。
        let _ = unsafe { SetTimer(Some(hwnd), RESIZE_TIMER_ID, 1, None) };
        // SAFETY: 系统光标句柄由 `LoadCursorW` 借用，进程不拥有；`SetCursor` 只影响本线程。
        unsafe { SetCursor(Some(resize_cursor(hit))) };
    }

    /// 记录最新光标位置并保持缩放光标。
    ///
    /// **不**移动窗口：`SetWindowPos` 只发生在定时器 tick、抬起与失去捕获时（P1-2b）。
    fn note_resize_cursor(&mut self) {
        let Some(session) = self.resize.as_mut() else {
            return;
        };
        let mut cursor = POINT::default();
        // SAFETY: `cursor` 是本函数栈上的输出值。
        if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
            session.latest = cursor;
        }
        // SAFETY: 同 `begin_resize`。
        unsafe { SetCursor(Some(resize_cursor(session.hit))) };
    }

    /// 按最新光标位置应用一次窗口矩形（定时器 tick / 抬起 / 失去捕获的唯一 `SetWindowPos` 点）。
    fn apply_resize(&mut self, hwnd: HWND) {
        let Some(session) = self.resize else {
            return;
        };
        let next = resized_rect(session, session.latest, self.min_size);
        let width = next.right - next.left;
        let height = next.bottom - next.top;
        if width <= 0 || height <= 0 {
            return;
        }
        // SAFETY: `hwnd` 存活；只改位置与尺寸，不改变 Z 序、不激活窗口。
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                next.left,
                next.top,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    /// 结束自绘边框缩放：杀掉合并定时器并释放鼠标捕获。
    fn end_resize(&mut self, hwnd: HWND) {
        if self.resize.take().is_some() {
            // SAFETY: `id` 由 `begin_resize` 在同一 hwnd 上创建；已过期 / 失败时是 no-op。
            let _ = unsafe { KillTimer(Some(hwnd), RESIZE_TIMER_ID) };
            // SAFETY: 只释放本线程的捕获；没有捕获时是 no-op。
            let _ = unsafe { ReleaseCapture() };
        }
    }
}

/// 窗口过程。所有分支都先取 `GWLP_USERDATA`；创建期（尚未绑定状态）一律交 `DefWindowProcW`。
pub(crate) extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `hwnd` 由系统在消息分发时给出；`GWLP_USERDATA` 只由本模块写入（`NativeWindow::create`），
    // 并在 `NativeWindow::drop` 之前清空（销毁后不会再有消息）。
    unsafe {
        // 系统标题栏：非客户区完全交给默认过程（P09 不做自绘标题栏）。
        if msg == WM_NCCALCSIZE {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;

        if msg == WM_GETMINMAXINFO {
            if !state.is_null() {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: (*state).min_size.0 as i32,
                    y: (*state).min_size.1 as i32,
                };
            }
            return LRESULT(0);
        }
        // 客户区的光标由引擎决定（P2-3）：每次鼠标移动系统都会问一次，不接管就会被
        // `DefWindowProcW` 重置成类光标（恒 `IDC_ARROW`），自定义光标永不生效。
        // 返回 1 = 「已处理，别再改」；非客户区（边框）交给系统，缩放箭头才对。
        if msg == WM_SETCURSOR {
            let hit = (lparam.0 as u32) & 0xFFFF;
            if hit == HTCLIENT && !state.is_null() {
                // SAFETY: 系统光标句柄由 `LoadCursorW` 借用、进程不拥有；`SetCursor`
                // 只影响当前线程。
                SetCursor(Some(cursor_handle((*state).cursor)));
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 引擎自己画：抑制系统擦背景（否则拖动/重绘时闪白）。
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        if msg == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            if !state.is_null() {
                (*state).wake();
            }
            return LRESULT(0);
        }
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 应用自己持有的指针捕获被系统收回（Alt+Tab、系统弹窗、别的窗口 SetCapture）：
        // 此时**不会**收到 `WM_LBUTTONUP`，拖拽会话会永久停在「按住」。投递一次
        // `PointerCancel` 让交互层作废本次会话（结束拖拽 + 派发 `DragPhase::Cancel`）。
        // 自绘边框缩放有自己的收尾逻辑，走下面的分支，不重复投递。
        if msg == WM_CAPTURECHANGED && (*state).resize.is_none() {
            (*state).events.push(PlatformEvent::PointerCancel);
            return LRESULT(0);
        }

        // ── 自绘边框缩放 ────────────────────────────────────────────────────────
        // 非客户区的缩放不能交给 `DefWindowProcW`：`WM_NCLBUTTONDOWN` 会进入 Win32 的模态
        // sizing 循环，把我们的泵阻塞在里面，`WM_SIZE` 只能排队、画面在拖动期间不更新。
        // 这里接管边框的按下/移动/抬起，普通消息循环继续跑，下一轮泵就能按新尺寸出帧。
        {
            let state = &mut *state;
            let hit = wparam.0 as u32;
            if msg == WM_NCLBUTTONDOWN && is_resize_hit(hit) {
                state.begin_resize(hwnd, hit);
                return LRESULT(0);
            }
            if state.resize.is_some() {
                match msg {
                    // 只记录最新光标：`SetWindowPos` 由定时器 tick / 抬起 / 失去捕获合并（P1-2b）。
                    WM_MOUSEMOVE => {
                        state.note_resize_cursor();
                        return LRESULT(0);
                    }
                    WM_TIMER if wparam.0 == RESIZE_TIMER_ID => {
                        state.apply_resize(hwnd);
                        // 每个 tick 至多一帧：泵的 ②b′ 会按最终客户区重算几何。
                        state.wake();
                        return LRESULT(0);
                    }
                    WM_LBUTTONUP => {
                        state.note_resize_cursor();
                        state.apply_resize(hwnd);
                        state.end_resize(hwnd);
                        state.wake();
                        return LRESULT(0);
                    }
                    WM_CAPTURECHANGED => {
                        // 捕获被系统收回（Alt+Tab / 弹出菜单等）：用最新位置收尾，不留半移动的窗口。
                        state.note_resize_cursor();
                        state.apply_resize(hwnd);
                        state.end_resize(hwnd);
                        return LRESULT(0);
                    }
                    _ => {}
                }
            }
        }

        // ── a11y（P11）：借用纪律 ──────────────────────────────────────────────
        // `UiaReturnRawElementProvider`（转 `LRESULT` 时）与 `QueuedEvents::raise` 都会**同步**
        // 派发嵌套 `WM_GETOBJECT`。所以这两处只能在**不持有 `&mut WndProcState`** 的窗口里做：
        // 短借用块取出「未转换的值」，出了块再 `.into()` / `raise()`（P11 审查 #5）。
        if msg == WM_GETOBJECT {
            let pending = {
                // SAFETY: `state` 非空且由本线程独占（`GWLP_USERDATA` 的既有纪律）。
                let cell = &mut *state;
                if !cell.a11y_mut().begin_getobject() {
                    // 重入：本次不接管（不让 UIA 拿到半个 provider）。
                    return LRESULT(0);
                }
                cell.a11y_mut().wm_getobject(wparam, lparam)
            };
            let result = match pending {
                Some(value) => value.into(),
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            };
            // SAFETY: 转换已完成，此时没有其它 `&mut` 存活。
            (*state).a11y_mut().end_getobject();
            return result;
        }
        // 窗口焦点态同步（与 `TranslateState` 的 `PlatformEvent::Focus` 是两条独立的线：
        // 前者给 AT 看「窗口有没有焦点」，后者给引擎做键盘分发）。
        if matches!(msg, WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE) {
            // SAFETY: 短借用；`raise` 在借用释放之后（它可能派发嵌套消息）。
            let events = { (*state).a11y_mut().focus_changed(true) };
            if let Some(events) = events {
                events.raise();
            }
        }
        if matches!(msg, WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE) {
            // 丢失键盘焦点时必须**复位瞬时输入状态**（P2-6）：
            //
            // 修饰键是按消息顺序维护的（按下置位、抬起清位）。失焦期间抬起消息不再到达本窗口，
            // 于是 Alt+Tab / 打开菜单 / 进入模态缩放后，用户实际已经松开的 Shift/Ctrl 会永久
            // 保持"按下"，之后所有输入都被当成组合键。代理对半高也一并清掉（否则失焦前半个
            // 代理会和失焦后的字符拼成乱码）。
            (*state).reset_transient_input();
            // SAFETY: 同上。
            let events = { (*state).a11y_mut().focus_changed(false) };
            if let Some(events) = events {
                events.raise();
            }
        }

        let state = &mut *state;
        if msg == WM_APP_FRAME_WAKE {
            // `Dispatcher::wake` 已置位，这里只需要返回。
            return LRESULT(0);
        }
        if msg == WM_APP_COMPOSITION_WAKE {
            // GPU 线程在帧**完成**后通知「交换链里的内容尺寸变了」。这个通知**不请求新帧**：
            // 消息本身已经把 `GetMessageW` 唤醒，泵的 ②b′ 几何 pass 每轮都按
            // `presented_surface` / 客户区重算（同值几何是 no-op）。若在这里 `state.wake()`，
            // 每次完成 present 都会额外产生一次 `App::frame`（HUD 下是真实的 paint + present）。
            return LRESULT(0);
        }
        if msg == WM_TIMER {
            // 连续帧 tick：只唤醒；动画推进在 `App::frame` 的 `tick_animations`，
            // P17 HUD 的采样/请求 damage 在 `App::frame` 的 `prepare_debug_hud`。
            if wparam.0 == FRAME_TIMER_ID {
                state.wake();
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_MOUSEMOVE && !state.mouse_tracking {
            // `PointerLeave` 的生产者：鼠标离开客户区时系统发 `WM_MOUSELEAVE`（0x02A3）。
            let mut track = TRACKMOUSEEVENT {
                cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // `track` 在栈上，`hwnd` 是本窗口；失败时下一帧再试。
            if TrackMouseEvent(&mut track).is_ok() {
                state.mouse_tracking = true;
            }
        }

        // 滚轮：`lParam` 是屏幕坐标，翻译层要求客户区坐标。
        let (wparam_value, lparam_value) = (wparam.0, lparam.0);
        let lparam_value = if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
            let mut point = POINT {
                x: loword_signed(lparam_value),
                y: hiword_signed(lparam_value),
            };
            let _ = ScreenToClient(hwnd, &mut point);
            encode_point(point.x, point.y)
        } else {
            lparam_value
        };

        let event = if is_ime_message(msg) {
            state
                .translate
                .ime_mut()
                .handle(hwnd, msg, lparam_value)
                .map(PlatformEvent::Ime)
        } else if msg == WM_SETTINGCHANGE {
            appearance_change(lparam_value)
        } else {
            translate(msg, wparam_value, lparam_value, &mut state.translate)
        };

        if let Some(event) = event {
            if matches!(event, PlatformEvent::PointerLeave) {
                state.mouse_tracking = false;
            }
            let is_close = matches!(event, PlatformEvent::Close);
            state.push(event);
            state.wake();
            if is_close {
                // 不交给 `DefWindowProcW`：销毁由事件循环在关掉 `App` 里的窗口之后做。
                state.close_requested = true;
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

fn is_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION | WM_IME_ENDCOMPOSITION
    )
}

/// `WM_SETTINGCHANGE` 且是「沉浸式颜色集」变化 → 读注册表拿深浅色。
fn appearance_change(lparam: isize) -> Option<PlatformEvent> {
    if lparam == 0 {
        return None;
    }
    // SAFETY: `lParam` 是系统给的以 NUL 结尾的宽字符串，只在本次消息处理期有效。
    let changed = unsafe {
        let text = lparam as *const u16;
        if text.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *text.add(len) != 0 && len < 64 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(text, len);
        String::from_utf16_lossy(slice)
    };
    if changed != "ImmersiveColorSet" {
        return None;
    }
    dark_mode().map(PlatformEvent::Appearance)
}

/// 读 `HKCU\...\Themes\Personalize\AppsUseLightTheme`（1 = 浅色）。
fn dark_mode() -> Option<bool> {
    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: 输出缓冲是栈上的 `u32`，`size` 同步给出字节数；键路径是 NUL 结尾的宽字符串。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    Some(data == 0)
}

/// 启动时声明 DPI 感知：per-monitor v2（`WM_DPICHANGED` 才带每屏 DPI）。
fn install_dpi_awareness() {
    // 失败通常意味着清单/宿主已经设过 awareness（返回 `ERROR_ACCESS_DENIED`）：沿用现有设置。
    // SAFETY: 无参数指针的进程级设置；必须在创建任何窗口之前调用。
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// 引擎自有事件循环：建窗口 → 挂渲染器 → 显示 → 泵 → 关闭。
pub(crate) fn run(mut app: App) -> Result<(), EngineError> {
    install_dpi_awareness();
    install_input_thresholds(&mut app);
    let dispatcher = app.dispatcher();
    // 线程消息兜底的目标：首窗关闭后 `PostMessageW` 会失败，`wake()` 必须还能叫醒 `GetMessageW`。
    // SAFETY: 无参数，返回调用线程的 id（只用于 `PostThreadMessageW`）。
    dispatcher.bind_thread(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });

    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        let mut native = NativeWindow::create(id, &options, dispatcher.clone())?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        if let Some(window) = app.window_mut(id) {
            window.set_renderer(renderer);
            window.set_scale(native.scale());
            window.set_size(native.logical_size());
            // 渲染目标就是真实客户区：逻辑尺寸取整后的 covering 值可能比它大 1 px。
            window.set_surface(client);
        }
        // 首帧已经由 `new_for_hwnd` prime 并 present（内容层与背景层都已 attach；客户区在第一次
        // present 之前完全透明），这里把几何对齐成「内容 = 客户区」（内容 1:1、背景层铺满），
        // 再显示窗口。
        {
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(client, client)?;
        }
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
    }
    if let Some(first) = windows.first() {
        dispatcher.attach(first.hwnd_raw());
        crate::platform::windows::clipboard::set_owner(first.hwnd_raw());
    }
    // 首帧：显示之后请求一帧（I3 的「空闲」只发生在没有 dirty 时）。
    dispatcher.wake();
    pump(app, windows, dispatcher)
}

fn pump(
    mut app: App,
    mut windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let result = pump_loop(&mut app, &mut windows, &dispatcher);
    // 关闭顺序（`design/async-render-pipeline.md` §6.7）：先 drop `App`（`Window` →
    // `Renderer::drop` 里的 GPU 线程 join 握手，期间窗口必须仍然存活），再销毁原生窗口。
    // 反过来会让 GPU 线程在 HWND 已销毁之后还去 `Present`。
    drop(app);
    drop(windows);
    result
}

/// 消息泵主体（每轮：消息 → 平台事件 → GPU 反馈 → 关闭 → 应用钩子 + 提交帧 → 阻塞等待）。
fn pump_loop(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let mut message = MSG::default();
    // 连续帧定时器记 (宿主 hwnd, timer id)：定时器随窗口销毁而失效，宿主窗口关闭后必须
    // 在新首窗上重建，否则其余窗口的动画/连续帧永远不再推进（P2-4）。
    let mut frame_timer: Option<(isize, usize)> = None;
    loop {
        // ① 先 drain：`WM_QUIT` 必须显式识别（`PeekMessageW` 会把它取出来）。
        loop {
            // SAFETY: `message` 是栈上值；过滤范围 0..0 = 本线程全部消息。
            let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
            if !has_message.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                return Ok(());
            }
            // SAFETY: `message` 由 `PeekMessageW` 填充；`TranslateMessage` 可能产生 `WM_CHAR`。
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        // ② 平台事件 → App。
        app.sync_clock();
        // ②a 运行时窗口生命周期（P1-2.8）：`App` 里新增/移除的窗口要同步到原生窗口。
        // 旧实现只在 `run` 启动时枚举一次，运行时 `App::create_window` 得到的窗口永远没有
        // 原生窗口与渲染器（`App::frame` 对着无 renderer 的 Window 空转）；应用自己调
        // `App::close_window` 也只删 App 状态，留下幽灵原生窗口并继续收事件。
        if sync_native_windows(app, windows, dispatcher)? {
            retarget_wake(windows, dispatcher);
        }
        collect_events(app, windows, dispatcher)?;

        // ②b GPU 线程反馈：设备丢失 / 呈现失败是粘性的，在这里冒泡出去（三平台统一）。
        app.poll_renderers()?;

        // ②b′ DirectComposition 几何：内容尺寸 = 最近一次**完成 present** 的尺寸，客户区 = 当前
        // 客户区。内容 visual 恒 1:1：拖动中旧内容保持左上角，露出的新区域由背景层填充；静止时
        // 内容 = 客户区，背景层被完全盖住。GPU 线程在每帧完成后 post
        // `WM_APP_COMPOSITION_WAKE`（只唤醒、不请求帧），所以「最后一帧」的几何也会在下一次泵
        // 迭代里落地。这里是几何**唯一**的周期应用点：`WM_SIZE` / DPI 只记录尺寸，不逐条
        // `Commit`（P1-2a）。
        for native in windows.iter_mut() {
            // 最小化时 `GetClientRect` 是 0×0（会钳成 1×1）：不要拿它当几何。
            if native.is_minimized() {
                continue;
            }
            let client = native.client_size();
            let presented = app
                .window(native.id())
                .map(Window::presented_surface)
                .unwrap_or(client);
            // `presented` 是待处理状态本身：每次泵迭代都读它，消息丢了也只是晚一轮（同值 no-op）。
            // `pending` 是 GPU 线程两次唤醒都失败时的兜底标记；正常情况下靠与本窗口上次成功应用
            // 的 `(presented, client)` 比较，避免输入风暴下每轮都进一次互斥锁。
            let pending = app
                .window(native.id())
                .is_some_and(Window::take_composition_pending);
            if !pending && native.composition_geometry() == Some((presented, client)) {
                continue;
            }
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(presented, client)?;
            native.note_composition_geometry((presented, client));
        }

        // ②c 交互反馈：光标 / 指针捕获 / 连续帧定时器（P10 §5.4 / §11.3、P17 HUD）。
        // 光标是**每窗口**状态（`WM_SETCURSOR` 各自读自己的 `WndProcState`），而
        // `App::take_cursor_change` 不带窗口 id ⇒ 必须写到**所有**窗口，否则首窗之外的
        // 窗口永远显示箭头（P2-4）。
        if let Some(cursor) = app.take_cursor_change() {
            for native in windows.iter_mut() {
                native.state_mut().set_cursor(cursor);
            }
            // SAFETY: 系统光标句柄由 `LoadCursorW` 借用，进程不拥有；`SetCursor` 只影响本线程。
            unsafe { SetCursor(Some(cursor_handle(cursor))) };
        }
        if let Some(native) = windows.first_mut() {
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            if let Some(capture) = app.take_capture_request() {
                if capture {
                    // SAFETY: `hwnd` 是本进程创建的窗口；捕获在拖拽结束/失焦时释放。
                    unsafe { SetCapture(hwnd) };
                } else {
                    // SAFETY: 同上，释放本线程的捕获。
                    let _ = unsafe { ReleaseCapture() };
                }
            }
            // 定时器随宿主窗口销毁而失效：宿主已不是当前首窗 ⇒ 清掉记录，下面在新首窗重建
            // （否则首窗关闭后其余窗口的动画/连续帧永远不再推进，P2-4）。
            if frame_timer.is_some_and(|(owner, _)| owner != native.hwnd_raw()) {
                frame_timer = None;
            }
            match (app.needs_continuous_frames(), frame_timer) {
                (true, None) => {
                    // SAFETY: 1 ms 定时器（probe：实测 16.0 ms）；`KillTimer` 在同一线程。
                    let id = unsafe { SetTimer(Some(hwnd), FRAME_TIMER_ID, 1, None) };
                    if id != 0 {
                        frame_timer = Some((native.hwnd_raw(), id));
                    }
                }
                (false, Some((owner, id))) => {
                    // SAFETY: `id` 由本线程在 `owner` 上 `SetTimer` 产生。
                    let owner = HWND(owner as *mut core::ffi::c_void);
                    let _ = unsafe { KillTimer(Some(owner), id) };
                    frame_timer = None;
                }
                _ => {}
            }
        }

        // ③ 关闭：关掉 App 里的窗口与原生窗口；一个不剩就退出。
        if close_requested_windows(app, windows, dispatcher)? {
            return Ok(());
        }

        // ④ 应用钩子（读事件 / 写状态），再提交本帧；两段门禁在 `App::frame` 里，闲时是 `Idle`。
        app.run_frame_hook();
        if dispatcher.take_frame_wake() {
            frame_all(app)?;
        }

        // ④′ a11y：纯 `A11Y` 变化不会提交帧（`FrameOutcome::Idle`），所以**每轮**都要给 adapter
        // 一次机会；`App::a11y_pending` 是 O(1) 过滤，adapter 不活跃时闭包根本不执行（懒激活）。
        for native in windows.iter_mut() {
            native.update_a11y(app);
            native.sync_ime_cursor_area(app);
        }

        // ⑤ 跨线程回投的任务。
        for task in dispatcher.drain_tasks() {
            task.run(app);
        }

        // ⑥ 阻塞等待（I3）：没有消息时停在这里。
        // SAFETY: 同 ①。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == 0 {
            return Ok(());
        }
        if result.0 == -1 {
            // SAFETY: 无参数、无指针的线程局部查询；`GetMessageW == -1` 之后调用才有效。
            let last_error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
            return Err(EngineError::WindowCreate {
                detail: smol_str::format_smolstr!("GetMessageW: GetLastError={last_error}"),
            });
        }
        // SAFETY: `message` 已由 `GetMessageW` 填充。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// 把 `App` 的窗口集合同步到原生窗口（运行时创建 / 应用主动关闭，P1-2.8）。
///
/// 返回 `true` 表示原生窗口集合变了（调用方需要重新指向唤醒目标与剪贴板属主）。
///
/// 两件事：
/// - **创建**：`App` 里存在但还没有原生窗口的 id ⇒ 建 `NativeWindow` + `Renderer`，并把
///   尺寸 / DPI / surface 写回 `Window`（与启动路径同一套写入顺序）。
/// - **销毁**：原生窗口对应的 `App` 窗口已经不存在 ⇒ `destroy()` + 从列表移除。
///   （用户点 X 的路径在 `close_requested_windows` 里，已经先 `app.close_window` 再 `destroy`，
///   这里只兜「应用自己关掉」的那一半。）
fn sync_native_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    let mut changed = false;
    // 先销毁：避免同一个 id 在窗口关闭又被复用时留下两份原生窗口。
    windows.retain(|native| {
        if app.window(native.id()).is_some() {
            true
        } else {
            changed = true;
            false
        }
    });
    // `NativeWindow::drop` 负责 `DestroyWindow`（GPU 线程的 join 在 `Renderer::drop` 里，
    // `App::close_window` 已经丢掉了那个 `Window`，因此顺序天然正确）。
    let existing: Vec<WindowId> = windows.iter().map(NativeWindow::id).collect();
    let pending: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .filter(|(id, _)| !existing.contains(id))
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    for (id, options) in pending {
        let mut native = NativeWindow::create(id, &options, dispatcher.clone())?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        if let Some(window) = app.window_mut(id) {
            // `set_renderer` 先清空图集页记账（新渲染器 = 一套空纹理 ⇒ 两张页都要整页重传），
            // `set_surface` 会把 `full_damage_pending` 置位（新交换链/持久目标必须整帧）。
            window.set_renderer(renderer);
            window.set_scale(native.scale());
            window.set_size(native.logical_size());
            window.set_surface(client);
        }
        {
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(client, client)?;
        }
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
        changed = true;
    }
    Ok(changed)
}

/// 把 `WndProc` 收集的事件应用到 `App`（几何/DPI 先落地，再投递事件）。
fn collect_events(
    app: &mut App,
    windows: &mut [NativeWindow],
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    for native in windows.iter_mut() {
        let id = native.id();
        let events = native.state_mut().take_events();
        for event in events {
            apply_event(app, native, dispatcher, id, event)?;
        }
    }
    Ok(())
}

fn apply_event(
    app: &mut App,
    native: &mut NativeWindow,
    dispatcher: &Arc<Dispatcher>,
    id: WindowId,
    event: PlatformEvent,
) -> Result<(), EngineError> {
    let scale = native.scale();
    let to_logical = |x: i32, y: i32| {
        crate::math::Point::new(scale.to_logical(x as f32), scale.to_logical(y as f32))
    };
    let modifiers = native.state_mut().modifiers();
    let app_event = match event {
        PlatformEvent::Pointer { x, y, kind } => {
            let position = to_logical(x, y);
            let input = match kind {
                PointerKind::Move => InputEvent::PointerMove {
                    position,
                    modifiers,
                },
                PointerKind::Down(button) => InputEvent::PointerDown {
                    position,
                    button,
                    modifiers,
                },
                PointerKind::Up(button) => InputEvent::PointerUp {
                    position,
                    button,
                    modifiers,
                },
            };
            Event::Input(input)
        }
        PlatformEvent::Scroll {
            x,
            y,
            dx_ticks,
            dy_ticks,
        } => Event::Input(InputEvent::Scroll {
            position: to_logical(x, y),
            delta_x: dx_ticks,
            delta_y: dy_ticks,
            modifiers,
        }),
        PlatformEvent::PointerLeave => Event::Input(InputEvent::PointerLeave),
        PlatformEvent::PointerCancel => Event::Input(InputEvent::PointerCancel),
        PlatformEvent::Key { key, state } => Event::Input(InputEvent::Key {
            key,
            state,
            modifiers: native.state_mut().modifiers(),
        }),
        PlatformEvent::Text(text) => Event::Input(InputEvent::Text { text }),
        PlatformEvent::Ime(ime) => Event::Ime(ime),
        PlatformEvent::Resize { width, height } => {
            // 最小化时 `WM_SIZE` 是 0×0：**保留上一次真实逻辑尺寸**，不写 `set_size` /
            // `set_surface`（否则布局被压成 0、恢复时先渲染一整面单色，P2-3）；恢复窗口的
            // `WM_SIZE` 会给出真实客户区。几何由 ②b′ 统一应用（这里不逐条 `Commit`，P1-2a）。
            let size = if native.is_minimized() {
                native.logical_size()
            } else {
                // 只记录尺寸并唤醒（`design/async-render-pipeline.md` §6.7）：内容重画发生在 GPU
                // 线程按下一帧的 `FrameRequest.surface` 做，UI 线程不等待。
                native.note_client_resize((width, height));
                let size = native.logical_size();
                if let Some(window) = app.window_mut(id) {
                    window.set_size(size);
                    // 目标尺寸 = 真实客户区（`WM_SIZE` 的 lParam 就是新的客户区宽高）。
                    window.set_surface((width, height));
                }
                size
            };
            dispatcher.wake();
            Event::Resized { size }
        }
        PlatformEvent::Dpi { dpi, suggested } => {
            // DPI 同理：只更新窗口尺寸/比例并唤醒；内容重画由 GPU 线程消费新的 surface，
            // 几何由 ②b′ 统一应用（P1-2a）。
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            // 有系统建议矩形时**不要**先按旧尺寸重排（P2-2）：那会先跳一次再套建议矩形，
            // 可见抖动 + 一次多余布局。只记账新 DPI，位置与尺寸完全交给建议矩形。
            let resizable = native.note_scale(scale, dpi);
            match suggested {
                Some(rect) if resizable && !native.is_minimized() => native.place(rect)?,
                // 没有可用建议矩形（系统没给 / 窗口最小化）：保持逻辑尺寸，按新 DPI 重排一次。
                _ => native.apply_scale(scale, dpi)?,
            }
            if let Some(window) = app.window_mut(id) {
                window.set_scale(scale);
                // `apply_scale` / `place` 已按新 DPI 重排非客户区：回读真实客户区。
                // 最小化时 `GetClientRect` 是 0×0（`client_size()` 会钳成 1×1）：跳过 `set_surface`，
                // 目标保持上一次平台写入的真实客户区；恢复窗口的 `WM_SIZE` 会用新客户区覆盖它。
                if !native.is_minimized() {
                    window.set_surface(native.client_size());
                }
            }
            dispatcher.wake();
            Event::DpiChanged { dpi, scale }
        }
        PlatformEvent::Focus(focused) => Event::FocusChanged { focused },
        PlatformEvent::Appearance(dark) => Event::AppearanceChanged { dark },
        PlatformEvent::Close => Event::CloseRequested,
    };
    app.dispatch(id, app_event);
    Ok(())
}

/// 关闭被要求关闭的窗口；返回 `true` = 没有窗口了（泵退出）。
fn close_requested_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    let mut closed = false;
    for native in windows.iter_mut() {
        if native.state_mut().take_close_request() {
            let id = native.id();
            app.close_window(id);
            native.destroy();
            closed = true;
        }
    }
    if closed {
        windows.retain(|native| !native.is_destroyed());
        retarget_wake(windows, dispatcher);
    }
    Ok(windows.is_empty() || app.windows().next().is_none())
}

/// 唤醒目标 / 剪贴板属主跟随存活的第一个窗口；一个不剩就清空（`wake` 改走线程消息）。
///
/// 目标窗口销毁后 `PostMessageW` 会失败，若不改指，`GetMessageW` 会永久阻塞（P14 审查 #2）。
fn retarget_wake(windows: &[NativeWindow], dispatcher: &Arc<Dispatcher>) {
    match windows.first() {
        Some(native) => {
            dispatcher.attach(native.hwnd_raw());
            crate::platform::windows::clipboard::set_owner(native.hwnd_raw());
        }
        None => dispatcher.detach(),
    }
}

/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
fn frame_all(app: &mut App) -> Result<(), EngineError> {
    let ids: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
    for id in ids {
        match app.frame(id) {
            None | Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

fn encode_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod resize_tests {
    use windows::Win32::{
        Foundation::{POINT, RECT},
        UI::WindowsAndMessaging::{HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT, HTTOPLEFT},
    };

    use super::{ResizeSession, is_resize_hit, resized_rect};

    /// 缩放起点与抓取点必须一致：`HTLEFT`/`HTTOPLEFT`/`HTBOTTOMLEFT` 的按下点在**左边缘**，
    /// 右键/上边/下边同理。抓取点由调用方显式给出（旧的固定 `(500, 300)` 让左边缘的 `dx`
    /// 从窗口右边缘起算，算术上不可能触发最小宽度钳制）。
    fn session(hit: u32, cursor: POINT) -> ResizeSession {
        ResizeSession {
            hit,
            cursor,
            window: RECT {
                left: 100,
                top: 100,
                right: 500,
                bottom: 400,
            },
            latest: cursor,
        }
    }

    #[test]
    fn border_hits_are_the_eight_resize_edges() {
        assert!(is_resize_hit(HTLEFT));
        assert!(is_resize_hit(HTBOTTOMRIGHT));
        assert!(!is_resize_hit(HTCAPTION), "标题栏拖动仍交给 DefWindowProcW");
    }

    #[test]
    fn dragging_the_right_edge_changes_only_the_width() {
        let rect = resized_rect(
            session(HTRIGHT, POINT { x: 500, y: 300 }),
            POINT { x: 540, y: 280 },
            (320, 200),
        );

        assert_eq!(rect.left, 100);
        assert_eq!(rect.right, 540);
        assert_eq!(rect.top, 100);
        assert_eq!(rect.bottom, 400);
    }

    #[test]
    fn dragging_the_top_left_corner_moves_the_origin() {
        let rect = resized_rect(
            session(HTTOPLEFT, POINT { x: 100, y: 100 }),
            POINT { x: 80, y: 80 },
            (320, 200),
        );

        assert_eq!(rect.left, 80);
        assert_eq!(rect.top, 80);
        assert_eq!(rect.right, 500);
        assert_eq!(rect.bottom, 400);
    }

    #[test]
    fn minimum_size_clamps_the_dragged_edge() {
        // 左边缘抓取点在 (100, 300)：拖动到 (490, 300) 时 dx = 390，左边越过右边界，
        // 钳制到 right - min_width = 500 - 320 = 180。
        let rect = resized_rect(
            session(HTLEFT, POINT { x: 100, y: 300 }),
            POINT { x: 490, y: 300 },
            (320, 200),
        );
        assert_eq!(
            rect.right - rect.left,
            320,
            "左边不能把窗口压到最小宽度以下"
        );
        assert_eq!(rect.right, 500);
        assert_eq!(rect.left, 180);
    }
}

#[cfg(test)]
mod wndproc_tests {
    use std::sync::Arc;

    use windows::Win32::{
        Foundation::{HWND, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetWindowRect, HTRIGHT, RegisterClassW,
            SendMessageW, SetWindowLongPtrW, WINDOW_EX_STYLE, WM_LBUTTONUP, WM_MOUSEMOVE,
            WM_NCLBUTTONDOWN, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        },
    };

    use super::{WndProcState, wndproc};
    use crate::{
        platform::dispatcher::{Dispatcher, WM_APP_COMPOSITION_WAKE},
        window::WindowId,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 建一个**隐藏**窗口并挂上生产 `WndProc` + `WndProcState`（`GWLP_USERDATA` 指向它）。
    fn hidden_window(
        tag: &str,
        dispatcher: Arc<Dispatcher>,
    ) -> (HWND, *mut WndProcState, Vec<u16>) {
        let class_name = wide(tag);
        // SAFETY: 进程级模块句柄；类名是 NUL 结尾的宽字符串，`RegisterClassW` 同步拷贝。
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // SAFETY: 同上；每个测试用唯一的类名，只注册一次。
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
        let state = match WndProcState::new(dispatcher, hwnd, WindowId::for_test(1)) {
            Ok(state) => Box::into_raw(Box::new(state)),
            Err(error) => {
                // SAFETY: 窗口由本函数创建、尚未交给其它所有者。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                panic!("WndProcState::new: {error}");
            }
        };
        // SAFETY: `hwnd` 由本函数创建；`state` 的所有权由测试通过 `destroy_hidden_window` 回收。
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
        (hwnd, state, class_name)
    }

    /// 先销毁窗口（此后不会再有消息），再回收 `WndProcState`（与 `NativeWindow::drop` 同序）。
    fn destroy_hidden_window(hwnd: HWND, state: *mut WndProcState) {
        // SAFETY: 窗口与状态都由本测试独占，且只回收一次。
        unsafe {
            let _ = DestroyWindow(hwnd);
            drop(Box::from_raw(state));
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// P1-1 回归：`WM_APP_COMPOSITION_WAKE` 只唤醒泵，**不**置帧唤醒位。
    ///
    /// 旧实现调用 `state.wake()`：每次帧完成都会让泵多跑一次 `App::frame`（HUD 下是真实的
    /// paint + present），完成通知因此变成了额外的帧源。
    #[test]
    fn the_composition_wake_does_not_request_a_frame() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) =
            hidden_window("KytrixWndProcCompositionWake", Arc::clone(&dispatcher));
        // 窗口创建 / a11y 构造不应留下待处理帧；清一次让断言只测被发送的消息。
        let _ = dispatcher.take_frame_wake();

        // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入 `wndproc`。
        unsafe {
            SendMessageW(hwnd, WM_APP_COMPOSITION_WAKE, None, None);
        }

        assert!(
            !dispatcher.take_frame_wake(),
            "合成通知必须只唤醒 GetMessageW，不得请求 App::frame"
        );
        destroy_hidden_window(hwnd, state);
    }

    /// P1-2b 回归：缩放期间的 `WM_MOUSEMOVE` 只记录光标，不移动窗口（`SetWindowPos` 在定时器
    /// tick / 抬起时合并）。旧实现每个鼠标消息都 `SetWindowPos` ⇒ 348 Hz 上报时 FPS 193.8。
    #[test]
    fn mouse_move_during_a_resize_does_not_move_the_window() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) =
            hidden_window("KytrixWndProcResizePacing", Arc::clone(&dispatcher));

        let mut before = RECT::default();
        // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入 `wndproc`。
        unsafe {
            // 按下右边框：进入自绘缩放（定时器启动），不交给 DefWindowProcW 的模态循环。
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTRIGHT as usize)), None);
            GetWindowRect(hwnd, &mut before).expect("GetWindowRect");
            SendMessageW(hwnd, WM_MOUSEMOVE, None, None);
        }
        let mut after = RECT::default();
        // SAFETY: 同上。
        unsafe {
            GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
        }
        assert_eq!(
            (after.left, after.top, after.right, after.bottom),
            (before.left, before.top, before.right, before.bottom),
            "WM_MOUSEMOVE 不得直接 SetWindowPos"
        );

        // 抬起：最终 apply（可能按真实光标位置移动窗口）+ 杀定时器 + 释放捕获。
        // SAFETY: 同上。
        unsafe {
            SendMessageW(hwnd, WM_LBUTTONUP, None, None);
        }
        destroy_hidden_window(hwnd, state);
    }
}
