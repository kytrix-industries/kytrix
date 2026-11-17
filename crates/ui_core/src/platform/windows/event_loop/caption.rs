//! 自绘标题栏拖动 / 边框缩放：会话状态机（`CaptionDrag` / `ResizeSession`）、合并定时器
//! tick、最大化还原与取消语义、工作区钳制。实现体由 `event_loop.rs` 的薄委托调用
//! （零行为变更的结构拆分）。

use std::time::{Duration, Instant};

use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    UI::{
        HiDpi::GetSystemMetricsForDpi,
        Input::KeyboardAndMouse::{GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

use super::{CaptionDrag, CaptionPress, ResizeSession, WndProcState};
use crate::{math::Scale, platform::windows::dpi};

/// 消费者：`wndproc` 的 `WM_TIMER` 分支。
/// 自绘边框缩放的合并定时器 id（与 [`FRAME_TIMER_ID`] 分开）。
///
/// 1 ms 定时器在 P09 probe 里实测合并到 16.0 ms ⇒ ~60 Hz：鼠标以 1000 Hz 上报时，窗口矩形与
/// DComp 几何、`App::frame` 都只按这个节奏推进（P1-2b）。`WM_TIMER` 是低优先级消息，连续的
/// 鼠标消息可能让它延迟；抬起 / 失去捕获时的最终 apply 保证窗口不会停在旧位置（不忙等）。
pub(super) const RESIZE_TIMER_ID: usize = 2;

/// 消费者：`wndproc` 的 `WM_TIMER` 分支与 `WndProcState` 的拖动会话。
/// 自绘标题栏拖动的合并定时器 id（与缩放分开：两种会话互斥，但 id 各自独立便于收尾）。
///
/// 与缩放同一策略：`WM_MOUSEMOVE` 只表示「光标动了」，真正的 `SetWindowPos` 由这个 1 ms
/// 定时器合并到约 60 Hz（Chrome 的 `MoveLoopMouseWatcher` 也是定时轮询光标、按 tick 移动，
/// 而不是每个鼠标消息移一次窗口）。
pub(super) const CAPTION_TIMER_ID: usize = 3;

/// 消费者：`min_visible_edge`。
/// 窗口被拖到屏幕边缘时至少保留在工作区内的可见尺寸（**逻辑像素**）。
///
/// 水平方向保证窗口至少有一段可见、垂直方向保证标题栏不会被拖出工作区（Windows 原生
/// move 循环同样不允许把标题栏拖到顶边之上）。用逻辑像素而不是物理常量：拖动可能发生在
/// 200% DPI 的显示器上，固定的 64 物理像素在高 DPI 下只剩 32 逻辑像素，窗口会被拖得几乎
/// 不可见（win-2 P3）。物理值由 [`min_visible_edge`] 按窗口当前 DPI 换算。
pub(super) const MIN_VISIBLE_EDGE_LOGICAL: f32 = 64.0;

/// 消费者：`clamp_to_work`。
/// 可见边（物理像素）= 逻辑 64 px × DPI 比例，至少 1 px。
///
/// 纯函数：1.0/1.25/1.5/2.0 等比例的换算可以在单测里钉死，不需要真实的高 DPI 显示器。
pub(super) fn min_visible_edge(scale: Scale) -> i32 {
    (MIN_VISIBLE_EDGE_LOGICAL * scale.factor()).round().max(1.0) as i32
}

/// 消费者：`WndProcState::apply_caption_drag` 与单测。
/// 窗口当前 DPI 比例（拖动 / 还原的可见边与钳制都按物理像素换算）。
///
/// 用**窗口** DPI（`WM_DPICHANGED` 之后窗口已跟着到新显示器）与原生 move 循环一致。
pub(super) fn window_scale(hwnd: HWND) -> Scale {
    dpi::scale_from_dpi(dpi::window_dpi(hwnd)).unwrap_or(Scale::ONE)
}

/// 消费者：`wndproc` 的 `WM_NCLBUTTONDOWN` 分支。
/// `WM_NCHITTEST` 的可缩放边框区域。
pub(super) fn is_resize_hit(hit: u32) -> bool {
    matches!(
        hit,
        HTLEFT | HTRIGHT | HTTOP | HTBOTTOM | HTTOPLEFT | HTTOPRIGHT | HTBOTTOMLEFT | HTBOTTOMRIGHT
    )
}

/// 消费者：`wndproc` 的 `WM_NCLBUTTONDOWN` 分支。
/// 会被引擎接管为「拖动窗口」的标题栏区域：标题文字（`HTCAPTION`）+ 左上角图标
/// （`HTSYSMENU`）。
///
/// 系统标题栏仍由 `DefWindowProcW` 绘制与命中测试（本文件不接管 `WM_NCCALCSIZE` /
/// `WM_NCHITTEST`），所以最小化/最大化/关闭按钮继续是 `HTMINBUTTON` 等、行为不变；这里只把
/// 「按下后原本会让 `DefWindowProcW` 进入模态循环」的两个区域接管。`HTSYSMENU` 必须一起接管：
/// 不接管时点标题栏图标会打开系统菜单的模态循环（且坐标上它就是标题栏的一部分）。
pub(super) fn is_caption_hit(hit: u32) -> bool {
    matches!(hit, HTCAPTION | HTSYSMENU)
}

/// 消费者：`WndProcState::self_timed_double_click`。
/// 自计时双击判定（win-2 P3 的纯函数部分）。
///
/// 系统通常在第二次按下时把消息换成 `WM_NCLBUTTONDBLCLK`；但 UI 线程卡顿 / 仍持有捕获时
/// 可能只送来普通的 `WM_NCLBUTTONDOWN`，双击就退化成「开始拖动」。这里按系统同一套规则
/// 自己判定：
/// - 时间：距上次按下**严格小于** `GetDoubleClickTime()`（与系统的「在指定时间内」一致）；
/// - 距离：第二次按下必须落在以首次按下为中心的**双击矩形**内。`SM_CXDOUBLECLK` /
///   `SM_CYDOUBLECLK` 是矩形总宽高（MSDN），因此允许偏差是它们的一半。
pub(super) fn is_self_timed_double_click(
    elapsed: Duration,
    cursor_delta: (i32, i32),
    double_click_time: Duration,
    double_click_half_size: (i32, i32),
) -> bool {
    // `unsigned_abs`：极端坐标差（`i32::MIN`）在 debug 下用 `abs` 会溢出 panic。
    elapsed < double_click_time
        && cursor_delta.0.unsigned_abs() <= double_click_half_size.0.max(0) as u32
        && cursor_delta.1.unsigned_abs() <= double_click_half_size.1.max(0) as u32
}

/// 消费者：`WndProcState::apply_caption_drag`。
/// 标题栏拖动的新窗口位置：把 `window` 按光标增量平移，并钳制到 `work` 工作区。
///
/// 约束与 Windows 原生 move 循环一致（多显示器时 `work` 是**光标所在**显示器的工作区）：
/// - 标题栏不能越过工作区顶边（`top >= work.top`），也不能被拖到下边界之外；
/// - 水平方向允许窗口大部分移出屏幕，但至少留 [`min_visible_edge`] 可见。
///
/// 纯函数：多显示器 / 负坐标 / 不同 DPI 都能在单测里钉死，不需要真的接第二个显示器。
pub(super) fn caption_position(
    window: RECT,
    delta: (i32, i32),
    work: RECT,
    scale: Scale,
) -> (i32, i32) {
    let width = (window.right - window.left).max(1);
    clamp_to_work(
        window.left + delta.0,
        window.top + delta.1,
        width,
        work,
        scale,
    )
}

/// 消费者：`caption_position` / `restored_drag_origin`。
/// 把窗口左上角钳制到工作区内（标题栏不出顶边、底部/左右至少留 [`min_visible_edge`]）。
pub(super) fn clamp_to_work(
    left: i32,
    top: i32,
    width: i32,
    work: RECT,
    scale: Scale,
) -> (i32, i32) {
    let width = width.max(1);
    let edge = min_visible_edge(scale);
    let min_top = work.top;
    let max_top = (work.bottom - edge).max(min_top);
    let min_left = work.left - width + edge;
    let max_left = (work.right - edge).max(min_left);
    (left.clamp(min_left, max_left), top.clamp(min_top, max_top))
}

/// 消费者：`restore_maximized_for_drag`。
/// 最大化窗口被拖动还原后的窗口左上角。
///
/// 与系统/Chrome 一致：
/// - 水平方向按光标在**最大化标题栏**中的相对位置，让光标落到还原窗口标题栏的同一比例处；
/// - 垂直方向保持**按下时**光标在标题栏内的偏移（`press.y - maximized.top`）。标题栏上下沿
///   在最大化与还原两种状态下的非客户区偏移相同，所以「相对窗口顶边的偏移」等价于「相对
///   标题栏的偏移」。只按 `maximized.top` 摆位时，还原触发前光标已经移动的那几十像素会被
///   丢掉，快速拖动（系统阈值刚过、光标已甩出很远）后光标会掉到标题栏外（win-2 P3）。
///
/// 结果再钳制到光标所在显示器的工作区。
pub(super) fn restored_drag_origin(
    maximized: RECT,
    restored: RECT,
    press: POINT,
    cursor: POINT,
    work: RECT,
    scale: Scale,
) -> (i32, i32) {
    let maximized_width = (maximized.right - maximized.left).max(1);
    let ratio = ((cursor.x - maximized.left) as f32 / maximized_width as f32).clamp(0.0, 1.0);
    let restored_width = (restored.right - restored.left).max(1);
    let left = cursor.x - (ratio * restored_width as f32) as i32;
    let top = cursor.y - (press.y - maximized.top);
    clamp_to_work(left, top, restored_width, work, scale)
}

/// 消费者：`WndProcState::apply_caption_drag`。
/// 最大化窗口在拖动超过系统阈值后还原到上次的还原矩形，并按光标比例摆回光标下。
///
/// 返回 `false` = 还没超过拖拽阈值（单击最大化窗口的标题栏**不得**还原，系统行为），
/// 或查询失败；调用方保持会话不动，等下一次 tick 再试。
pub(super) fn restore_maximized_for_drag(
    hwnd: HWND,
    press: POINT,
    cursor: POINT,
    work: RECT,
    scale: Scale,
) -> bool {
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
    let (left, top) = restored_drag_origin(maximized, restored, press, cursor, work, scale);
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

/// 消费者：`WndProcState::apply_caption_drag`。
/// 光标所在显示器的工作区（物理像素）。
///
/// 用**光标**而不是窗口矩形来选显示器：拖动跨屏时窗口横跨两个显示器，`MonitorFromWindow`
/// 只在窗口中心越过边界时才切换，会让跨屏拖动在边界上卡住；光标才是拖动的目标点。
/// 查询失败返回 `None`：调用方跳过本次移动（宁可不移动，也不把窗口拖出屏幕）。
pub(super) fn cursor_work_area(cursor: POINT) -> Option<RECT> {
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

/// 消费者：`WndProcState::begin_resize` / `note_resize_cursor`。
/// 缩放边框期间的系统光标。
pub(super) fn resize_cursor(hit: u32) -> HCURSOR {
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

/// 消费者：`WndProcState::apply_resize`。
/// 从缩放起点与当前鼠标位置算出新的窗口矩形（物理像素，含最小尺寸钳制）。
pub(super) fn resized_rect(session: ResizeSession, cursor: POINT, min: (u32, u32)) -> RECT {
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

/// 消费者：`WndProcState::begin_resize`（`event_loop.rs` 的薄委托）。
/// 开始自绘边框缩放：记录起点并抓鼠标（不再把消息交给 `DefWindowProcW` 的模态循环）。
///
/// 同时启动 [`RESIZE_TIMER_ID`] 合并定时器：`WM_MOUSEMOVE` 只更新最新光标，真正的
/// `SetWindowPos` 由定时器 tick 合并成 ~60 Hz（P1-2b）。
pub(super) fn begin_resize(state: &mut WndProcState, hwnd: HWND, hit: u32) {
    let mut cursor = POINT::default();
    let mut window = RECT::default();
    // SAFETY: 两个输出指针都指向本函数栈上的值；`hwnd` 由当前消息给出且窗口存活。
    let captured =
        unsafe { GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut window).is_ok() };
    if !captured {
        return;
    }
    // SAFETY: `hwnd` 是本进程创建的窗口；捕获在抬起 / 取消时释放。
    unsafe { SetCapture(hwnd) };
    state.resize = Some(ResizeSession {
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

/// 消费者：`WndProcState::note_resize_cursor`（`event_loop.rs` 的薄委托）。
/// 记录最新光标位置并保持缩放光标。
///
/// **不**移动窗口：`SetWindowPos` 只发生在定时器 tick、抬起与失去捕获时（P1-2b）。
pub(super) fn note_resize_cursor(state: &mut WndProcState) {
    let Some(session) = state.resize.as_mut() else {
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

/// 消费者：`WndProcState::apply_resize`（`event_loop.rs` 的薄委托）。
/// 按最新光标位置应用一次窗口矩形（定时器 tick / 抬起 / 失去捕获的唯一 `SetWindowPos` 点）。
pub(super) fn apply_resize(state: &mut WndProcState, hwnd: HWND) {
    let Some(session) = state.resize else {
        return;
    };
    let next = resized_rect(session, session.latest, state.min_size);
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

/// 消费者：`WndProcState::end_resize`（`event_loop.rs` 的薄委托）。
/// 结束自绘边框缩放：杀掉合并定时器并释放鼠标捕获。
pub(super) fn end_resize(state: &mut WndProcState, hwnd: HWND) {
    if state.resize.take().is_some() {
        // SAFETY: `id` 由 `begin_resize` 在同一 hwnd 上创建；已过期 / 失败时是 no-op。
        let _ = unsafe { KillTimer(Some(hwnd), RESIZE_TIMER_ID) };
        // 只有确实持有捕获时才置 suppress 并释放（见 `release_capture` 的说明）。
        release_capture(hwnd, state);
    }
}

// ── 自绘标题栏拖动（P2-9） ──────────────────────────────────────────────

/// 消费者：`WndProcState::begin_caption_drag`（`event_loop.rs` 的薄委托）。
/// 开始标题栏拖动：记录光标基准、抓鼠标、启动合并定时器。
///
/// 最大化窗口**不在这里还原**：系统行为是「拖动超过阈值才还原，单击不还原」。还原延迟到
/// [`WndProcState::apply_caption_drag`] 第一次真正移动时（见 [`restore_maximized_for_drag`]）。
pub(super) fn begin_caption_drag(state: &mut WndProcState, hwnd: HWND) {
    let mut cursor = POINT::default();
    let mut origin = RECT::default();
    // SAFETY: 两个输出指针都指向本函数栈上的值；`hwnd` 由当前消息给出且窗口存活。
    let captured =
        unsafe { GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut origin).is_ok() };
    if !captured {
        return;
    }
    // SAFETY: `hwnd` 存活；`IsZoomed` 只读窗口状态。
    let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
    // SAFETY: `hwnd` 是本进程创建的窗口；捕获在抬起 / 失去捕获时释放。
    unsafe { SetCapture(hwnd) };
    state.caption_drag = Some(CaptionDrag {
        cursor,
        origin,
        restore: zoomed,
        origin_zoomed: zoomed,
    });
    // 1 ms 定时器（probe：实测合并到 16.0 ms，同缩放）。失败不重试：抬起时的最终
    // apply 仍保证窗口落到最后位置，只是拖动中没有中间几何更新。
    // SAFETY: `hwnd` 存活；`KillTimer` 用同一个 id（`end_caption_drag`）。
    let _ = unsafe { SetTimer(Some(hwnd), CAPTION_TIMER_ID, 1, None) };
}

/// 消费者：`WndProcState::apply_caption_drag`（`event_loop.rs` 的薄委托）。
/// 按最新光标位置应用一次窗口平移（定时器 tick / 抬起 / 失去捕获的唯一 `SetWindowPos` 点）。
///
/// 增量式：读**当前**窗口矩形 + 光标距上次应用位置的增量，再按光标所在显示器的工作区
/// 钳制。`SetWindowPos` 失败时不推进基准（下一次 tick 用同一增量重试）。
pub(super) fn apply_caption_drag(state: &mut WndProcState, hwnd: HWND) {
    let Some(mut session) = state.caption_drag else {
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
    let scale = window_scale(hwnd);
    if session.restore {
        // 最大化窗口：等位移超过系统拖拽阈值再还原 + 摆回光标下。还没超过就什么都不做
        //（单击标题栏不得还原）；本次位移已经用于「触发还原 + 摆位」，不推进基准。
        if !restore_maximized_for_drag(hwnd, session.cursor, cursor, work, scale) {
            return;
        }
        session.restore = false;
        session.cursor = cursor;
        state.caption_drag = Some(session);
        return;
    }
    let mut window = RECT::default();
    // SAFETY: `hwnd` 存活；`window` 是栈上输出值。
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_err() {
        return;
    }
    let (left, top) = caption_position(window, delta, work, scale);
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
        state.caption_drag = Some(session);
    }
}

/// 消费者：`WndProcState::end_caption_drag`（`event_loop.rs` 的薄委托）。
/// 结束标题栏拖动：杀掉合并定时器并释放鼠标捕获。
pub(super) fn end_caption_drag(state: &mut WndProcState, hwnd: HWND) {
    if state.caption_drag.take().is_some() {
        // SAFETY: `id` 由 `begin_caption_drag` 在同一 hwnd 上创建；已过期 / 失败时是 no-op。
        let _ = unsafe { KillTimer(Some(hwnd), CAPTION_TIMER_ID) };
        // 先 `take()` 再释放捕获：嵌套派发的 `WM_CAPTURECHANGED` 会走通用 suppress 分支
        //（此时 `caption_drag` 已是 `None`），不会被当成会话丢失。
        release_capture(hwnd, state);
    }
}

/// 消费者：`WndProcState::cancel_caption_drag`（`event_loop.rs` 的薄委托）。
/// 取消标题栏拖动（`Esc` / 右键，win-2 P3）：窗口回到按下时的矩形（按下时最大化则重新
/// 最大化），结束会话并释放捕获。
///
/// 先 `take()` 会话再动窗口：`SetWindowPos` / `ShowWindow` 会同步派发 `WM_WINDOWPOSCHANGED`
/// 等嵌套消息，嵌套进入的 `WndProc` 不得再看到旧会话（与 `end_caption_drag` 同一纪律）。
pub(super) fn cancel_caption_drag(state: &mut WndProcState, hwnd: HWND) {
    let Some(session) = state.caption_drag.take() else {
        return;
    };
    // 显式取消结束了这次按下序列：下一次按下应当从头开始（否则「右键取消后立刻左键按下」
    // 会被自计时双击兜底当成双击）。
    state.last_caption_press = None;
    // SAFETY: `id` 由 `begin_caption_drag` 在同一 hwnd 上创建；已过期 / 失败时是 no-op。
    let _ = unsafe { KillTimer(Some(hwnd), CAPTION_TIMER_ID) };
    let width = (session.origin.right - session.origin.left).max(1);
    let height = (session.origin.bottom - session.origin.top).max(1);
    if session.origin_zoomed {
        if !session.restore {
            // 拖动中已经把最大化窗口还原，还可能移到了光标所在显示器：先摆回起始矩形
            // （最大化窗口原来的显示器），再重新最大化 —— 否则会在新显示器上最大化，
            // 回不到按下前的状态。
            // SAFETY: `hwnd` 存活；只改位置与尺寸，不改变 Z 序、不激活窗口。
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    session.origin.left,
                    session.origin.top,
                    width,
                    height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            // SAFETY: `hwnd` 存活；`SW_MAXIMIZE` 只改显示状态，不持有 Rust 引用。
            let _ = unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
        }
        // `session.restore == true`：窗口本来就还没还原（按下后没有移动过），无需恢复。
    } else {
        // SAFETY: `hwnd` 存活；只改位置与尺寸，不改变 Z 序、不激活窗口。
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                session.origin.left,
                session.origin.top,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }
    // SAFETY: 引擎自己的收尾路径：只有确实持有捕获时才置 suppress 并释放。
    release_capture(hwnd, state);
}

/// 消费者：`WndProcState::self_timed_double_click`（`event_loop.rs` 的薄委托）。
/// 自计时双击判定：系统没投递 `WM_NCLBUTTONDBLCLK` 时（UI 线程卡顿 / 仍持有捕获），
/// 按「距上次按下的时间 + 双击矩形」自己判双击（win-2 P3）。返回 `true` = 本次按下应
/// 按双击处理（最大化/还原切换），不再开始拖动。
///
/// 每次按下都会成为新的「上一次按下」；命中双击后清空，避免第三次快按再次切换。
pub(super) fn self_timed_double_click(state: &mut WndProcState, hwnd: HWND) -> bool {
    let mut cursor = POINT::default();
    // SAFETY: `cursor` 是本函数栈上的输出值。
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return false;
    }
    let now = Instant::now();
    let double_clicked = state.last_caption_press.is_some_and(|press| {
        // 系统度量在 per-monitor-v2 进程里必须按**窗口 DPI** 查询：`GetSystemMetrics`
        // 用的是系统 DPI，高 DPI 显示器上双击矩形会被算小。
        let dpi = dpi::window_dpi(hwnd);
        // SAFETY: 三个都是无副作用、无指针参数的只读查询。
        let (width, height, time) = unsafe {
            (
                GetSystemMetricsForDpi(SM_CXDOUBLECLK, dpi).max(1),
                GetSystemMetricsForDpi(SM_CYDOUBLECLK, dpi).max(1),
                GetDoubleClickTime().max(1),
            )
        };
        is_self_timed_double_click(
            now.saturating_duration_since(press.at),
            (cursor.x - press.cursor.x, cursor.y - press.cursor.y),
            Duration::from_millis(u64::from(time)),
            // `SM_CXDOUBLECLK` / `SM_CYDOUBLECLK` 是双击矩形**总**宽高，允许偏差是一半。
            (width / 2, height / 2),
        )
    });
    state.last_caption_press = (!double_clicked).then_some(CaptionPress { at: now, cursor });
    double_clicked
}

/// 消费者：`WndProcState::toggle_maximize`（`event_loop.rs` 的薄委托）。
/// 双击标题栏：在最大化与还原之间切换（不进入系统 move / maximize 模态循环）。
pub(super) fn toggle_maximize(state: &mut WndProcState, hwnd: HWND) {
    // 双击序列里第二次按下被替换成 `WM_NCLBUTTONDBLCLK`：先收尾第一次按下可能留下的
    // 拖动会话（正常路径已由抬起收尾，这里只防御消息丢失）。
    state.end_caption_drag(hwnd);
    // 这次按下序列已经被双击消费：清掉自计时记录，避免紧接的第三次按下又被当成双击。
    state.last_caption_press = None;
    // SAFETY: `hwnd` 存活；两个调用都只改窗口显示状态，不持有 Rust 引用。
    unsafe {
        let zoomed = IsZoomed(hwnd).as_bool();
        let _ = ShowWindow(hwnd, if zoomed { SW_RESTORE } else { SW_MAXIMIZE });
    }
}

/// 消费者：`WndProcState` 的会话收尾与 `pump` 的捕获请求。
/// 引擎**主动**释放指针捕获（拖拽结束 / 缩放结束）：只有本窗口确实持有捕获时才置
/// `suppress_capture_cancel` 并释放。
///
/// 两处细节都不能省：
/// - 自己调 `ReleaseCapture` 也会收到 `WM_CAPTURECHANGED`，不置位就会在每次松手后误投
///   `PointerCancel`（复审 F1）；
/// - 不持有捕获时 `ReleaseCapture` 不会产生该消息，若仍然置位，残留的 suppress 会把
///   **下一次真实**的会话丢失当成「自己释放」而吞掉取消（复审 F2）。
pub(super) fn release_capture(hwnd: HWND, state: &mut WndProcState) {
    // SAFETY: `GetCapture` 只读查询当前线程的捕获窗口。
    if unsafe { GetCapture() } != hwnd {
        return;
    }
    state.suppress_capture_cancel = true;
    // SAFETY: 只释放本线程的捕获；同线程调用会同步派发 `WM_CAPTURECHANGED`。
    let _ = unsafe { ReleaseCapture() };
}
