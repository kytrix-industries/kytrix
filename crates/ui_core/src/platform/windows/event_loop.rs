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
    Graphics::Gdi::{
        BeginPaint, EndPaint, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO,
        MonitorFromPoint, PAINTSTRUCT, ScreenToClient,
    },
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{
            GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT,
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

/// 自绘标题栏拖动的合并定时器 id（与缩放分开：两种会话互斥，但 id 各自独立便于收尾）。
///
/// 与缩放同一策略：`WM_MOUSEMOVE` 只表示「光标动了」，真正的 `SetWindowPos` 由这个 1 ms
/// 定时器合并到约 60 Hz（Chrome 的 `MoveLoopMouseWatcher` 也是定时轮询光标、按 tick 移动，
/// 而不是每个鼠标消息移一次窗口）。
const CAPTION_TIMER_ID: usize = 3;

/// 窗口被拖到屏幕边缘时至少保留在工作区内的可见尺寸（物理像素）。
///
/// 水平方向保证窗口至少有一段可见、垂直方向保证标题栏不会被拖出工作区（Windows 原生
/// move 循环同样不允许把标题栏拖到顶边之上）。
const MIN_VISIBLE_EDGE: i32 = 64;

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

/// 会被引擎接管为「拖动窗口」的标题栏区域：标题文字（`HTCAPTION`）+ 左上角图标
/// （`HTSYSMENU`）。
///
/// 系统标题栏仍由 `DefWindowProcW` 绘制与命中测试（本文件不接管 `WM_NCCALCSIZE` /
/// `WM_NCHITTEST`），所以最小化/最大化/关闭按钮继续是 `HTMINBUTTON` 等、行为不变；这里只把
/// 「按下后原本会让 `DefWindowProcW` 进入模态循环」的两个区域接管。`HTSYSMENU` 必须一起接管：
/// 不接管时点标题栏图标会打开系统菜单的模态循环（且坐标上它就是标题栏的一部分）。
fn is_caption_hit(hit: u32) -> bool {
    matches!(hit, HTCAPTION | HTSYSMENU)
}

/// 标题栏拖动的新窗口位置：把 `window` 按光标增量平移，并钳制到 `work` 工作区。
///
/// 约束与 Windows 原生 move 循环一致（多显示器时 `work` 是**光标所在**显示器的工作区）：
/// - 标题栏不能越过工作区顶边（`top >= work.top`），也不能被拖到下边界之外；
/// - 水平方向允许窗口大部分移出屏幕，但至少留 [`MIN_VISIBLE_EDGE`] 可见。
///
/// 纯函数：多显示器 / 负坐标工作区都能在单测里钉死，不需要真的接第二个显示器。
fn caption_position(window: RECT, delta: (i32, i32), work: RECT) -> (i32, i32) {
    let width = (window.right - window.left).max(1);
    clamp_to_work(window.left + delta.0, window.top + delta.1, width, work)
}

/// 把窗口左上角钳制到工作区内（标题栏不出顶边、底部/左右至少留 [`MIN_VISIBLE_EDGE`]）。
fn clamp_to_work(left: i32, top: i32, width: i32, work: RECT) -> (i32, i32) {
    let width = width.max(1);
    let min_top = work.top;
    let max_top = (work.bottom - MIN_VISIBLE_EDGE).max(min_top);
    let min_left = work.left - width + MIN_VISIBLE_EDGE;
    let max_left = (work.right - MIN_VISIBLE_EDGE).max(min_left);
    (left.clamp(min_left, max_left), top.clamp(min_top, max_top))
}

/// 最大化窗口被拖动还原后的窗口左上角。
///
/// 与系统/Chrome 一致：水平方向按光标在**最大化标题栏**中的相对位置，让光标落到还原窗口
/// 标题栏的同一比例处；垂直方向保持光标在标题栏内的偏移（还原顶边 = 最大化顶边）。
/// 结果再钳制到光标所在显示器的工作区。
fn restored_drag_origin(maximized: RECT, restored: RECT, cursor: POINT, work: RECT) -> (i32, i32) {
    let maximized_width = (maximized.right - maximized.left).max(1);
    let ratio = ((cursor.x - maximized.left) as f32 / maximized_width as f32).clamp(0.0, 1.0);
    let restored_width = (restored.right - restored.left).max(1);
    let left = cursor.x - (ratio * restored_width as f32) as i32;
    clamp_to_work(left, maximized.top, restored_width, work)
}

/// 最大化窗口在拖动超过系统阈值后还原到上次的还原矩形，并按光标比例摆回光标下。
///
/// 返回 `false` = 还没超过拖拽阈值（单击最大化窗口的标题栏**不得**还原，系统行为），
/// 或查询失败；调用方保持会话不动，等下一次 tick 再试。
fn restore_maximized_for_drag(hwnd: HWND, press: POINT, cursor: POINT, work: RECT) -> bool {
    // SAFETY: `GetSystemMetrics` 是无副作用、无指针参数的只读查询。
    let (threshold_x, threshold_y) = unsafe {
        (
            GetSystemMetrics(SM_CXDRAG).max(1),
            GetSystemMetrics(SM_CYDRAG).max(1),
        )
    };
    if (cursor.x - press.x).abs() <= threshold_x && (cursor.y - press.y).abs() <= threshold_y {
        return false;
    }
    let mut maximized = RECT::default();
    // SAFETY: `hwnd` 存活；`maximized` 是栈上输出值。
    if unsafe { GetWindowRect(hwnd, &mut maximized) }.is_err() {
        return false;
    }
    // SAFETY: `hwnd` 存活；`SW_RESTORE` 只改窗口显示状态。
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    let mut restored = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut restored) }.is_err() {
        // 已经还原：摆位失败不能阻止拖动继续。
        return true;
    }
    let (left, top) = restored_drag_origin(maximized, restored, cursor, work);
    let width = (restored.right - restored.left).max(1);
    let height = (restored.bottom - restored.top).max(1);
    // SAFETY: `hwnd` 存活；沿用还原尺寸，只改位置（不激活窗口、不改 Z 序）。
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            left,
            top,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    true
}

/// 光标所在显示器的工作区（物理像素）。
///
/// 用**光标**而不是窗口矩形来选显示器：拖动跨屏时窗口横跨两个显示器，`MonitorFromWindow`
/// 只在窗口中心越过边界时才切换，会让跨屏拖动在边界上卡住；光标才是拖动的目标点。
/// 查询失败返回 `None`：调用方跳过本次移动（宁可不移动，也不把窗口拖出屏幕）。
fn cursor_work_area(cursor: POINT) -> Option<RECT> {
    // SAFETY: 两个调用都只读系统状态；`info` 是栈上输出，`cbSize` 按 API 要求先填好。
    unsafe {
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info.rcWork)
    }
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
    /// 正在进行的自绘标题栏拖动（拦截 `WM_NCLBUTTONDOWN` 的 `HTCAPTION` 后不再进入
    /// Win32 的 move 模态循环，拖动期间泵继续跑、动画/HUD 不断帧）。
    caption_drag: Option<CaptionDrag>,
    /// a11y（P11：首显前构造的 `accesskit_windows::Adapter` + `WM_GETOBJECT` 接线）。
    a11y: crate::platform::windows::a11y::WindowsA11y,
    /// 引擎**自己**即将释放指针捕获：下一个 `WM_CAPTURECHANGED` 不能当成
    /// 「系统夺走捕获」，否则每次正常拖拽结束都会误投一次 `PointerCancel`。
    ///（MSDN 明确：自己调 `ReleaseCapture` 也会收到该消息。）
    suppress_capture_cancel: bool,
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

/// 一次自绘标题栏拖动的增量状态。
///
/// 只记「上一次**应用移动**时的光标屏幕坐标」，每个 tick 重新读窗口矩形再按增量平移：
/// 拖动期间 `WM_DPICHANGED`（跨不同缩放显示器）会由系统建议矩形改写窗口几何，缓存起始
/// 矩形会让后续移动跳变；增量式没有这个基线问题。
#[derive(Clone, Copy)]
struct CaptionDrag {
    /// 上一次应用移动（或按下）时的光标屏幕坐标（物理像素）。
    cursor: POINT,
    /// 按下时窗口是最大化：第一次拖动超过阈值后才还原（单击标题栏不还原，与系统一致）。
    restore: bool,
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
            caption_drag: None,
            // **必须在窗口显示之前构造**：`Adapter::new` 内部 `init_uia()`，
            // 且不得在处理 `WM_GETOBJECT` 时构造（adapter.rs:411-441）。
            a11y: crate::platform::windows::a11y::WindowsA11y::new(hwnd, dispatcher, window)?,
            suppress_capture_cancel: false,
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

    /// 本窗口的 IME **已应用**状态（泵每轮与 `App` 的目标状态比较）。
    pub(crate) fn ime_enabled(&mut self) -> bool {
        self.translate.ime_mut().enabled()
    }

    /// 应用 IME 启用/禁用（禁用时 `ImmAssociateContext(hwnd, None)` 并保存旧上下文）。
    pub(crate) fn set_ime_enabled(&mut self, hwnd: HWND, enabled: bool) {
        self.translate.ime_mut().set_enabled(hwnd, enabled);
    }

    /// 窗口销毁前把 IME 上下文还回去（避免 `HIMC` 泄漏，见 `NativeWindow::destroy`）。
    pub(crate) fn release_ime_before_destroy(&mut self, hwnd: HWND) {
        self.translate.ime_mut().release_before_destroy(hwnd);
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
            // 只有确实持有捕获时才置 suppress 并释放（见 `release_capture` 的说明）。
            release_capture(hwnd, self);
        }
    }

    // ── 自绘标题栏拖动（P2-9） ──────────────────────────────────────────────

    /// 开始标题栏拖动：记录光标基准、抓鼠标、启动合并定时器。
    ///
    /// 最大化窗口**不在这里还原**：系统行为是「拖动超过阈值才还原，单击不还原」。还原延迟到
    /// [`Self::apply_caption_drag`] 第一次真正移动时（见 [`restore_maximized_for_drag`]）。
    fn begin_caption_drag(&mut self, hwnd: HWND) {
        let mut cursor = POINT::default();
        // SAFETY: `cursor` 是本函数栈上的输出值。
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        // SAFETY: `hwnd` 存活；`IsZoomed` 只读窗口状态。
        let restore = unsafe { IsZoomed(hwnd) }.as_bool();
        // SAFETY: `hwnd` 是本进程创建的窗口；捕获在抬起 / 失去捕获时释放。
        unsafe { SetCapture(hwnd) };
        self.caption_drag = Some(CaptionDrag { cursor, restore });
        // 1 ms 定时器（probe：实测合并到 16.0 ms，同缩放）。失败不重试：抬起时的最终
        // apply 仍保证窗口落到最后位置，只是拖动中没有中间几何更新。
        // SAFETY: `hwnd` 存活；`KillTimer` 用同一个 id（`end_caption_drag`）。
        let _ = unsafe { SetTimer(Some(hwnd), CAPTION_TIMER_ID, 1, None) };
    }

    /// 按最新光标位置应用一次窗口平移（定时器 tick / 抬起 / 失去捕获的唯一 `SetWindowPos` 点）。
    ///
    /// 增量式：读**当前**窗口矩形 + 光标距上次应用位置的增量，再按光标所在显示器的工作区
    /// 钳制。`SetWindowPos` 失败时不推进基准（下一次 tick 用同一增量重试）。
    fn apply_caption_drag(&mut self, hwnd: HWND) {
        let Some(mut session) = self.caption_drag else {
            return;
        };
        let mut cursor = POINT::default();
        // SAFETY: `cursor` 是本函数栈上的输出值。
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        let delta = (cursor.x - session.cursor.x, cursor.y - session.cursor.y);
        if delta == (0, 0) {
            return;
        }
        let Some(work) = cursor_work_area(cursor) else {
            return;
        };
        if session.restore {
            // 最大化窗口：等位移超过系统拖拽阈值再还原 + 摆回光标下。还没超过就什么都不做
            //（单击标题栏不得还原）；本次位移已经用于「触发还原 + 摆位」，不推进基准。
            if !restore_maximized_for_drag(hwnd, session.cursor, cursor, work) {
                return;
            }
            session.restore = false;
            session.cursor = cursor;
            self.caption_drag = Some(session);
            return;
        }
        let mut window = RECT::default();
        // SAFETY: `hwnd` 存活；`window` 是栈上输出值。
        if unsafe { GetWindowRect(hwnd, &mut window) }.is_err() {
            return;
        }
        let (left, top) = caption_position(window, delta, work);
        // SAFETY: `hwnd` 存活；只改位置（`SWP_NOSIZE`），不改变 Z 序、不激活窗口。
        let moved = unsafe {
            SetWindowPos(
                hwnd,
                None,
                left,
                top,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        if moved.is_ok() {
            session.cursor = cursor;
            self.caption_drag = Some(session);
        }
    }

    /// 结束标题栏拖动：杀掉合并定时器并释放鼠标捕获。
    fn end_caption_drag(&mut self, hwnd: HWND) {
        if self.caption_drag.take().is_some() {
            // SAFETY: `id` 由 `begin_caption_drag` 在同一 hwnd 上创建；已过期 / 失败时是 no-op。
            let _ = unsafe { KillTimer(Some(hwnd), CAPTION_TIMER_ID) };
            // 先 `take()` 再释放捕获：嵌套派发的 `WM_CAPTURECHANGED` 会走通用 suppress 分支
            //（此时 `caption_drag` 已是 `None`），不会被当成会话丢失。
            release_capture(hwnd, self);
        }
    }

    /// 双击标题栏：在最大化与还原之间切换（不进入系统 move / maximize 模态循环）。
    fn toggle_maximize(&mut self, hwnd: HWND) {
        // 双击序列里第二次按下被替换成 `WM_NCLBUTTONDBLCLK`：先收尾第一次按下可能留下的
        // 拖动会话（正常路径已由抬起收尾，这里只防御消息丢失）。
        self.end_caption_drag(hwnd);
        // SAFETY: `hwnd` 存活；两个调用都只改窗口显示状态，不持有 Rust 引用。
        unsafe {
            let zoomed = IsZoomed(hwnd).as_bool();
            let _ = ShowWindow(hwnd, if zoomed { SW_RESTORE } else { SW_MAXIMIZE });
        }
    }
}

/// 引擎**主动**释放指针捕获（拖拽结束 / 缩放结束）：只有本窗口确实持有捕获时才置
/// `suppress_capture_cancel` 并释放。
///
/// 两处细节都不能省：
/// - 自己调 `ReleaseCapture` 也会收到 `WM_CAPTURECHANGED`，不置位就会在每次松手后误投
///   `PointerCancel`（复审 F1）；
/// - 不持有捕获时 `ReleaseCapture` 不会产生该消息，若仍然置位，残留的 suppress 会把
///   **下一次真实**的会话丢失当成「自己释放」而吞掉取消（复审 F2）。
fn release_capture(hwnd: HWND, state: &mut WndProcState) {
    // SAFETY: `GetCapture` 只读查询当前线程的捕获窗口。
    if unsafe { GetCapture() } != hwnd {
        return;
    }
    state.suppress_capture_cancel = true;
    // SAFETY: 只释放本线程的捕获；同线程调用会同步派发 `WM_CAPTURECHANGED`。
    let _ = unsafe { ReleaseCapture() };
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
        // 非客户区仍由系统**绘制**（P09 不做自绘标题栏）：`WM_NCCALCSIZE` / `WM_NCHITTEST`
        // 都交给默认过程，最小化/最大化/关闭按钮、边框命中与 DPI 下的标题栏高度继续由系统
        // 负责；只有「按下后会让默认过程进入模态循环」的拖动/缩放由本过程接管（见下方
        // `WM_NCLBUTTONDOWN` 分支）。
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
        // 应用主动禁用了本窗口的 IME（P2-7，自绘搜索框的英文直输等）：输入上下文已经从窗口
        // 上摘下，所有 `WM_IME_*` 一律吞掉 —— 不派发 `ImeEvent`、不写 caret 表单，也不交给
        // `DefWindowProcW`（它可能重新初始化/显示组合窗）。普通 `WM_CHAR` / 快捷键不受影响。
        if !(*state).ime_enabled() && is_any_ime_message(msg) {
            return LRESULT(0);
        }
        // 应用自己持有的指针捕获被系统收回（Alt+Tab、系统弹窗、别的窗口 SetCapture）：
        // 此时**不会**收到 `WM_LBUTTONUP`，拖拽会话会永久停在「按住」。投递一次
        // `PointerCancel` 让交互层作废本次会话（结束拖拽 + 派发 `DragPhase::Cancel`）。
        // 自绘边框缩放 / 标题栏拖动有自己的收尾逻辑，走下面的分支，不重复投递。
        if msg == WM_CAPTURECHANGED && (*state).resize.is_none() && (*state).caption_drag.is_none()
        {
            if (*state).suppress_capture_cancel {
                // 引擎自己刚 `ReleaseCapture`（正常拖拽结束 / 缩放结束）：这不是会话丢失。
                // MSDN 明确「自己调用也会收到该消息」，不区分就会在每次松手后误投一次
                // `PointerCancel`，把 hover 清掉并给观察者注入幽灵事件。
                (*state).suppress_capture_cancel = false;
                return LRESULT(0);
            }
            (*state).events.push(PlatformEvent::PointerCancel);
            (*state).wake();
            return LRESULT(0);
        }

        // ── 自绘非客户区交互（缩放 + 标题栏拖动，P2-9） ──────────────────────────
        // 非客户区的缩放/移动不能交给 `DefWindowProcW`：`WM_NCLBUTTONDOWN` 的缩放分支会进入
        // Win32 的模态 sizing 循环、`HTCAPTION` 会进入 move 模态循环，把我们的泵阻塞在里面，
        // `WM_SIZE`/`WM_TIMER` 只能排队、画面在拖动期间不更新。这里接管按下/移动/抬起，普通
        // 消息循环继续跑，下一轮泵就能出帧。
        //
        // 系统标题栏**仍然由系统绘制**（`WM_NCCALCSIZE` / `WM_NCHITTEST` 交默认过程），所以
        // 这里只接管两个「按下后需要进入模态循环」的命中区域：缩放边框 → 自绘缩放；
        // `HTCAPTION` / `HTSYSMENU` → 自绘拖动。最小化/最大化/关闭按钮（`HTMINBUTTON` 等）
        // 继续走 `DefWindowProcW`，行为不变；它们不进入模态循环。
        {
            let state = &mut *state;
            let hit = wparam.0 as u32;
            if is_caption_hit(hit) {
                match msg {
                    WM_NCLBUTTONDOWN => {
                        state.begin_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    // 双击标题栏：系统行为是最大化/还原切换；自己 `ShowWindow`，
                    // 不让 `DefWindowProcW` 进入 move 模态循环。
                    WM_NCLBUTTONDBLCLK => {
                        state.toggle_maximize(hwnd);
                        return LRESULT(0);
                    }
                    // 引擎不提供系统菜单（见 `WindowOptions` 文档）：右键标题栏交给
                    // `DefWindowProcW` 会调用 `TrackPopupMenu` 的模态循环，直接吞掉。
                    WM_NCRBUTTONDOWN | WM_NCRBUTTONUP | WM_NCRBUTTONDBLCLK => return LRESULT(0),
                    _ => {}
                }
            }
            // Alt+Space（或标题栏图标的键盘等价）：默认过程会打开系统菜单的模态循环。
            if msg == WM_SYSCOMMAND && (wparam.0 as u32 & 0xFFF0) == SC_KEYMENU {
                return LRESULT(0);
            }
            if msg == WM_NCLBUTTONDOWN && is_resize_hit(hit) {
                state.begin_resize(hwnd, hit);
                return LRESULT(0);
            }
            if state.caption_drag.is_some() {
                match msg {
                    // 位置由 1 ms 合并定时器统一 `SetWindowPos`（同 P1-2b）：拖动期间每个
                    // `WM_MOUSEMOVE` 都移动会产生远高于刷新率的窗口重排。捕获期间系统只发
                    // 客户区形式（MSDN：捕获后 `WM_NCMOUSEMOVE` 不再发出）；非客户区形式
                    // 仅作防御（捕获没拿到 / 被系统提前收回）。
                    WM_MOUSEMOVE | WM_NCMOUSEMOVE | WM_MOVING => return LRESULT(0),
                    WM_TIMER if wparam.0 == CAPTION_TIMER_ID => {
                        state.apply_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    // 捕获期间抬起是客户区形式（`WM_LBUTTONUP`），非客户区形式一并收尾，
                    // 避免拖动会话停在「按住」。
                    WM_LBUTTONUP | WM_NCLBUTTONUP => {
                        state.apply_caption_drag(hwnd);
                        state.end_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    WM_CAPTURECHANGED | WM_CANCELMODE => {
                        // 捕获被系统收回（Alt+Tab / 系统弹窗 / 别的窗口 SetCapture）：按最新
                        // 位置收尾，不留半移动的窗口。
                        state.apply_caption_drag(hwnd);
                        state.end_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    _ => {}
                }
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

/// 全部 `WM_IME_*`（组合三连 + `IMM32` 的通知/字符/按键消息）。
///
/// 应用禁用 IME 时窗口一条都不处理：上下文虽然已经摘下，`IMM32` 在切换/重激活时仍可能送
/// 消息，交给 `DefWindowProcW` 会让它重新初始化输入上下文或显示组合窗。
fn is_any_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION
            | WM_IME_COMPOSITION
            | WM_IME_ENDCOMPOSITION
            | WM_IME_SETCONTEXT
            | WM_IME_NOTIFY
            | WM_IME_CONTROL
            | WM_IME_COMPOSITIONFULL
            | WM_IME_SELECT
            | WM_IME_CHAR
            | WM_IME_REQUEST
            | WM_IME_KEYDOWN
            | WM_IME_KEYUP
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
    // 定时器随宿主窗口销毁而失效：宿主窗口关闭后必须在新首窗上重建，否则其余窗口的
    // 动画/连续帧永远不再推进（P2-4）。归属判定用 `WindowId` 而不是 HWND 裸值：
    // 首窗销毁后系统可能把同一个 HWND 复用给新窗，裸值比较会「看起来仍匹配」而不重建
    // 定时器（复审 P2-8）。
    let mut frame_timer: Option<(crate::window::WindowId, isize, usize)> = None;
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
        // 两者都是**每窗口**状态，请求本身携带 `WindowId`（复审 P1-3 / P2-2）：
        // - 光标：`WM_SETCURSOR` 各窗口读自己的 `WndProcState`；广播会把 A 窗口的 I-Beam
        //   写进 B 窗口，而 B 的解析值仍是 Default ⇒ 不会再发变化 ⇒ B 永久显示错光标。
        // - 捕获：`SetCapture` 只对目标 HWND 生效；固定给首窗会导致非首窗拖拽在鼠标移出
        //   窗口后收不到 `WM_LBUTTONUP`，会话永久卡住。
        if let Some((target, cursor)) = app.take_cursor_change()
            && let Some(native) = windows.iter_mut().find(|native| native.id() == target)
        {
            native.state_mut().set_cursor(cursor);
            // SAFETY: 系统光标句柄由 `LoadCursorW` 借用，进程不拥有；`SetCursor` 只影响本线程。
            unsafe { SetCursor(Some(cursor_handle(cursor))) };
        }
        if let Some((target, capture)) = app.take_capture_request()
            && let Some(native) = windows.iter_mut().find(|native| native.id() == target)
        {
            {
                let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
                if capture {
                    // SAFETY: `hwnd` 是本进程创建的窗口；捕获在拖拽结束/失焦时释放。
                    unsafe { SetCapture(hwnd) };
                } else {
                    // 只有确实持有捕获时才置 suppress 并释放（见 `release_capture`）。
                    release_capture(hwnd, native.state_mut());
                }
            }
        }
        if let Some(native) = windows.first_mut() {
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            // 定时器随宿主窗口销毁而失效：宿主已不是当前首窗 ⇒ 清掉记录，下面在新首窗重建
            // （否则首窗关闭后其余窗口的动画/连续帧永远不再推进，P2-4）。
            if frame_timer.is_some_and(|(owner, _, _)| owner != native.id()) {
                frame_timer = None;
            }
            // a11y 到期补发也借用这个定时器：`a11y_retry_pending` 最多持续 A11Y_THROTTLE
            // （100 ms），到期交付后自动停表。
            let needs_tick = app.needs_continuous_frames() || app.a11y_retry_pending();
            match (needs_tick, frame_timer) {
                (true, None) => {
                    // SAFETY: 1 ms 定时器（probe：实测 16.0 ms）；`KillTimer` 在同一线程。
                    let id = unsafe { SetTimer(Some(hwnd), FRAME_TIMER_ID, 1, None) };
                    if id != 0 {
                        frame_timer = Some((native.id(), native.hwnd_raw(), id));
                    }
                }
                (false, Some((_, owner, id))) => {
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
            // IME 启用状态先于 caret hint：刚禁用时不必再写表单，刚启用时要重发 hint
            //（`sync_ime_enabled` 会清掉 `ime_area` 缓存）。
            native.sync_ime_enabled(app);
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
                //
                // **逻辑尺寸必须在这里同步写回**（复审 P2-9）：`place` 只是 `SetWindowPos`，
                // 新客户区要等下一轮的 `WM_SIZE` 才进 `Window`。本泵轮次紧接着就会出帧，
                // 那时布局仍按旧逻辑尺寸跑、而渲染目标已是新物理尺寸 + 新 scale ⇒ 跨屏拖动
                // 会有一帧内容被放大/裁切。先按新 DPI 换算并写回，下一轮 `WM_SIZE` 以同值覆盖
                // （`note_client_resize` 对同值幂等）。
                if !native.is_minimized() {
                    let client = native.client_size();
                    native.note_client_resize(client);
                    window.set_size(crate::math::Size::new(
                        scale.to_logical(client.0 as f32),
                        scale.to_logical(client.1 as f32),
                    ));
                    window.set_surface(client);
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
            // 先问应用：`App::on_close` 返回 `false` 就拦下这次关闭（P2-8）；被拦下时
            // **不**销毁原生窗口（窗口继续存在，下一次点 X 还会再问）。
            if !app.request_close(id) {
                continue;
            }
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
        UI::WindowsAndMessaging::{
            HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT, HTSYSMENU, HTTOPLEFT,
        },
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
        assert!(
            !is_resize_hit(HTCAPTION),
            "标题栏由自绘拖动分支接管，不是缩放"
        );
        assert!(
            !is_resize_hit(HTSYSMENU),
            "标题栏左上角图标区也归自绘拖动（避免系统菜单模态循环）"
        );
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
mod caption_tests {
    use windows::Win32::{
        Foundation::{POINT, RECT},
        UI::WindowsAndMessaging::{HTCAPTION, HTSYSMENU},
    };

    use super::{MIN_VISIBLE_EDGE, caption_position, is_caption_hit, restored_drag_origin};

    fn work() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        }
    }

    fn window() -> RECT {
        RECT {
            left: 100,
            top: 100,
            right: 500,
            bottom: 400,
        }
    }

    #[test]
    fn caption_areas_are_the_title_bar_and_the_menu_icon() {
        assert!(is_caption_hit(HTCAPTION));
        assert!(is_caption_hit(HTSYSMENU));
        // `HTNOWHERE` 以及最小化/最大化/关闭按钮、边框都不归拖动。
        assert!(!is_caption_hit(0));
    }

    #[test]
    fn caption_drag_follows_the_cursor_delta() {
        let (left, top) = caption_position(window(), (50, 30), work());

        assert_eq!((left, top), (150, 130));
    }

    #[test]
    fn caption_drag_keeps_the_title_bar_below_the_work_area_top() {
        // 往上拖：系统原生 move 同样不允许把标题栏拖到工作区顶边之上。
        let (_, top) = caption_position(window(), (0, -500), work());

        assert_eq!(top, 0);
    }

    #[test]
    fn caption_drag_leaves_a_visible_sliver_at_the_bottom() {
        let (_, top) = caption_position(window(), (0, 2000), work());

        assert_eq!(top, work().bottom - MIN_VISIBLE_EDGE);
    }

    #[test]
    fn caption_drag_clamps_horizontally_to_the_work_area() {
        let (far_left, _) = caption_position(window(), (-2000, 0), work());
        let (far_right, _) = caption_position(window(), (3000, 0), work());

        // 至少留 MIN_VISIBLE_EDGE 宽在屏幕里（窗口宽 400）。
        assert_eq!(far_left, work().left - 400 + MIN_VISIBLE_EDGE);
        assert_eq!(far_right, work().right - MIN_VISIBLE_EDGE);
    }

    #[test]
    fn caption_drag_uses_the_cursor_monitor_work_area() {
        // 主屏右侧的第二台显示器：虚拟坐标从 1920 起，工作区含任务栏（bottom 小 40）。
        let secondary = RECT {
            left: 1920,
            top: 0,
            right: 3840,
            bottom: 1000,
        };
        let moved = RECT {
            left: 1900,
            top: 100,
            right: 2300,
            bottom: 400,
        };

        let (left, top) = caption_position(moved, (100, -200), secondary);

        assert_eq!(left, 2000, "跨屏拖动按光标所在显示器平移");
        assert_eq!(top, 0, "垂直仍钳制到第二台显示器的工作区顶边");
    }

    #[test]
    fn caption_drag_does_not_panic_on_a_tiny_work_area() {
        // 退化工作区（多显示器配置异常 / 远程桌面极小分辨率）：钳制区间必须自洽，不能
        // 触发 `clamp(min > max)` panic。工作区比 MIN_VISIBLE_EDGE 还窄时退化为「靠右边界
        // 减去可见边」，窗口 400 宽 ⇒ left = 10 - 64 = -54。
        let tiny = RECT {
            left: 0,
            top: 0,
            right: 10,
            bottom: 10,
        };

        let (left, top) = caption_position(window(), (1, 1), tiny);

        assert_eq!((left, top), (10 - MIN_VISIBLE_EDGE, 0));
    }

    #[test]
    fn restored_drag_origin_keeps_the_cursor_ratio_on_the_title_bar() {
        // 最大化窗口 1920 宽，还原尺寸 800×600：光标在最大化标题栏 50% 处 ⇒ 还原窗口
        // 标题栏的 50% 处也要落在光标下（left = 960 - 800/2），顶边与最大化顶边一致。
        let maximized = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let restored = RECT {
            left: 200,
            top: 100,
            right: 1000,
            bottom: 700,
        };
        let cursor = POINT { x: 960, y: 30 };

        let (left, top) = restored_drag_origin(maximized, restored, cursor, work());

        assert_eq!((left, top), (560, 0));
    }

    #[test]
    fn restored_drag_origin_uses_the_cursor_monitor() {
        // 第二台显示器（1920..3840）上的最大化窗口：光标在它左侧 1/4 处 ⇒ 还原窗口顶边
        // 留在该显示器工作区内，水平按比例摆位。
        let maximized = RECT {
            left: 1920,
            top: 0,
            right: 3840,
            bottom: 1000,
        };
        let restored = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let secondary = RECT {
            left: 1920,
            top: 0,
            right: 3840,
            bottom: 1000,
        };

        let (left, top) =
            restored_drag_origin(maximized, restored, POINT { x: 2400, y: 20 }, secondary);

        assert_eq!((left, top), (2200, 0));
    }
}

#[cfg(test)]
mod wndproc_tests {
    use std::sync::Arc;

    use windows::Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::Ime::GCS_RESULTSTR,
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetWindowRect, HTCAPTION, HTRIGHT,
                IsZoomed, RegisterClassW, SendMessageW, SetWindowLongPtrW, WINDOW_EX_STYLE,
                WM_CAPTURECHANGED, WM_IME_CHAR, WM_IME_COMPOSITION, WM_IME_STARTCOMPOSITION,
                WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WNDCLASSW,
                WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use super::{WndProcState, wndproc};
    use crate::{
        event::ImeEvent,
        platform::{
            dispatcher::{Dispatcher, WM_APP_COMPOSITION_WAKE},
            event::PlatformEvent,
        },
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

    /// 复审 F2：不持有捕获时引擎的 `release_capture` 不得留下 suppress 残留
    /// （否则下一次真实会话丢失会被误当成「自己释放」而吞掉取消）。
    #[test]
    fn release_capture_without_ownership_leaves_no_suppress() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, class_name) = hidden_window("KytrixReleaseCaptureTest", dispatcher);
        // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
        let state = unsafe { &mut *state };
        super::release_capture(hwnd, state);
        assert!(
            !state.suppress_capture_cancel,
            "没有捕获时不得置 suppress（会吞掉下一次真实取消）"
        );
        destroy_hidden_window(hwnd, state as *mut WndProcState);
        // 占位：保证 `class_name` 活到窗口销毁之后。
        let _ = class_name;
    }

    /// 复审 F1/F2：suppress 只吞**同一次**自己释放产生的 `WM_CAPTURECHANGED`。
    #[test]
    fn suppress_consumes_one_capture_change_and_clears() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, class_name) = hidden_window("KytrixSuppressTest", dispatcher);
        // SAFETY: 同上。
        let raw = unsafe { &mut *state };
        raw.suppress_capture_cancel = true;
        // SAFETY: 隐藏窗口存活；消息同步派发到生产 `wndproc`。
        unsafe {
            let _ = SendMessageW(hwnd, WM_CAPTURECHANGED, None, None);
        }
        assert!(!raw.suppress_capture_cancel, "suppress 必须被消费并清零");
        assert!(
            !raw.events
                .iter()
                .any(|event| matches!(event, crate::platform::event::PlatformEvent::PointerCancel)),
            "自己释放产生的捕获变化不得派发 PointerCancel"
        );
        destroy_hidden_window(hwnd, state);
        let _ = class_name;
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

    /// P2-7 回归：应用禁用 IME 后窗口过程吞掉全部 `WM_IME_*`（不派发 `ImeEvent`），
    /// 重新启用后恢复派发。
    ///
    /// 只验证窗口过程的状态机（不真正给测试窗口绑 IME）：隐藏窗口没有输入上下文，所以
    /// 禁用走 `ImmAssociateContext` 返回 NULL 的分支、启用走 `ImmAssociateContextEx(
    /// IACE_DEFAULT)` 分支 —— 两条边都要覆盖。
    #[test]
    fn disabled_ime_swallows_composition_events_until_reenabled() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) = hidden_window("KytrixWndProcImeGate", dispatcher);
        // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
        let raw = unsafe { &mut *state };

        // 基线：启用态的组合开始必须派发 `ImeEvent::Start`。
        // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
        unsafe {
            SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
        }
        assert!(
            raw.take_events()
                .iter()
                .any(|event| matches!(event, PlatformEvent::Ime(ImeEvent::Start))),
            "启用态必须派发 ImeEvent::Start"
        );

        raw.set_ime_enabled(hwnd, false);
        assert!(!raw.ime_enabled(), "禁用状态必须被记账");
        // 组合开始 / 提交 / IME 字符三条路径都要被吞掉。
        // SAFETY: 同上。
        unsafe {
            SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
            SendMessageW(
                hwnd,
                WM_IME_COMPOSITION,
                None,
                Some(LPARAM(GCS_RESULTSTR.0 as isize)),
            );
            SendMessageW(hwnd, WM_IME_CHAR, Some(WPARAM(b'a' as usize)), None);
        }
        assert!(
            raw.take_events().is_empty(),
            "禁用期间不得派发任何 Ime / Text 事件"
        );

        raw.set_ime_enabled(hwnd, true);
        assert!(raw.ime_enabled(), "重新启用状态必须被记账");
        // SAFETY: 同上。
        unsafe {
            SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
        }
        assert!(
            raw.take_events()
                .iter()
                .any(|event| matches!(event, PlatformEvent::Ime(ImeEvent::Start))),
            "重新启用后必须恢复派发"
        );

        destroy_hidden_window(hwnd, state);
    }

    /// P2-9 回归：按下标题栏进入自绘拖动（不交给 `DefWindowProcW` 的 move 模态循环），
    /// 拖动期间的 `WM_MOUSEMOVE` 不直接移动窗口（由 1 ms 定时器合并），抬起收尾并释放捕获。
    #[test]
    fn caption_press_starts_and_ends_the_engine_drag() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionDrag", dispatcher);
        // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
        let raw = unsafe { &mut *state };
        let mut before = RECT::default();
        let mut after = RECT::default();
        // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
        unsafe {
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            );
            assert!(raw.caption_drag.is_some(), "标题栏按下必须进入自绘拖动");
            GetWindowRect(hwnd, &mut before).expect("GetWindowRect");
            SendMessageW(hwnd, WM_MOUSEMOVE, None, None);
            GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
        }
        assert_eq!(
            (after.left, after.top, after.right, after.bottom),
            (before.left, before.top, before.right, before.bottom),
            "拖动期间的 WM_MOUSEMOVE 不得直接 SetWindowPos（合并到定时器）"
        );

        // SAFETY: 同上。
        unsafe {
            SendMessageW(hwnd, WM_LBUTTONUP, None, None);
        }
        assert!(raw.caption_drag.is_none(), "抬起必须结束拖动");
        assert!(
            !raw.suppress_capture_cancel,
            "引擎自己释放捕获产生的 WM_CAPTURECHANGED 必须被消费"
        );
        destroy_hidden_window(hwnd, state);
    }

    /// P2-9 回归：双击标题栏由引擎自己 `ShowWindow` 最大化/还原，不进入系统 move 模态循环。
    #[test]
    fn double_click_on_the_caption_toggles_maximize() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionDblClick", dispatcher);
        // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
        unsafe {
            assert!(!IsZoomed(hwnd).as_bool(), "初始不是最大化");
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDBLCLK,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            );
            assert!(IsZoomed(hwnd).as_bool(), "双击标题栏必须最大化");
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDBLCLK,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            );
            assert!(!IsZoomed(hwnd).as_bool(), "再次双击必须还原");
        }
        destroy_hidden_window(hwnd, state);
    }

    /// P2-9 回归：单击（按下 + 抬起、没有拖动）最大化窗口的标题栏**不得**还原 ——
    /// 还原只应发生在拖动超过系统阈值时（与原生 move 循环一致）。
    #[test]
    fn clicking_the_caption_of_a_maximized_window_does_not_restore_it() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionClick", dispatcher);
        // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
        unsafe {
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDBLCLK,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            );
            assert!(IsZoomed(hwnd).as_bool(), "前置：窗口已最大化");
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            );
            SendMessageW(hwnd, WM_LBUTTONUP, None, None);
            assert!(
                IsZoomed(hwnd).as_bool(),
                "单击标题栏不得还原最大化窗口（只有拖动才还原）"
            );
        }
        destroy_hidden_window(hwnd, state);
    }

    /// P2-9：最大化窗口的标题栏拖动**超过系统阈值**后由引擎还原；阈值内不还原。
    ///
    /// 直接驱动 `restore_maximized_for_drag`（拖动分支的唯一还原点），用合成光标位置替代
    /// 真实鼠标，避免测试依赖光标/前台窗口状态。
    #[test]
    fn dragging_a_maximized_caption_restores_it_after_the_threshold() {
        let _runtime = runtime();
        let dispatcher = Dispatcher::new();
        let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionRestore", dispatcher);
        // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
        unsafe {
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDBLCLK,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            );
        }
        assert!(unsafe { IsZoomed(hwnd) }.as_bool(), "前置：窗口已最大化");

        let press = POINT { x: 600, y: 10 };
        let work = super::cursor_work_area(press).expect("光标所在显示器工作区");
        assert!(
            !super::restore_maximized_for_drag(
                hwnd,
                press,
                POINT {
                    x: press.x + 1,
                    y: press.y + 1,
                },
                work,
            ),
            "未超过拖拽阈值时不得还原"
        );
        // SAFETY: 窗口仍存活。
        assert!(unsafe { IsZoomed(hwnd) }.as_bool(), "阈值内不得还原");

        assert!(
            super::restore_maximized_for_drag(
                hwnd,
                press,
                POINT {
                    x: press.x + 120,
                    y: press.y + 60,
                },
                work,
            ),
            "超过阈值必须还原"
        );
        // SAFETY: 同上。
        assert!(
            !unsafe { IsZoomed(hwnd) }.as_bool(),
            "还原后不再处于最大化状态"
        );
        destroy_hidden_window(hwnd, state);
    }
}
