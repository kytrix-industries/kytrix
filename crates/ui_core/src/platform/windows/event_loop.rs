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
//!
//! 自绘标题栏拖动（P2-9，win-2 P3 补齐取消语义）：
//! - `Esc` 取消：窗口回到按下时的矩形（按下时最大化则重新最大化），按键不投递给应用；
//! - 右键取消：不提供系统菜单，按原生 move 循环「右键结束移动」的语义还原原位；
//! - 中键 / 滚轮在拖动期间吞掉：捕获后它们以客户区形式到达，坐标相对被拖动的窗口是负的，
//!   交给应用会派发到错误位置；
//! - 系统没投递 `WM_NCLBUTTONDBLCLK` 时（UI 线程卡顿）用「时间 + 双击矩形」自计时兜底，
//!   见 [`caption::is_self_timed_double_click`]。

use std::{sync::Arc, time::Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};

use crate::{
    error::EngineError,
    math::PhysicalRect,
    platform::{
        dispatcher::Dispatcher,
        event::{MessageContext, PlatformEvent, StampedEvent, TranslateState},
    },
    window::WindowId,
};

mod caption;
mod native_window;
mod pump;
mod wndproc;

pub(crate) use pump::run;
pub(crate) use wndproc::wndproc;

#[cfg(test)]
mod tests;

/// `WndProc` 的每窗口状态（`GWLP_USERDATA` 指向它）。
pub(crate) struct WndProcState {
    dispatcher: Arc<Dispatcher>,
    /// 待泵消费的平台事件（**带消息时间**：R02-14 的时间戳来源是 `MSG.time`，不是分发时刻）。
    events: Vec<StampedEvent>,
    translate: TranslateState,
    /// 一格滚轮对应的行数（`SPI_GETWHEELSCROLLLINES`；`0.0` = 未知/按页，见
    /// [`pump::wheel_lines_from_system`]）。
    wheel_lines: f32,
    /// 最近一次客户区鼠标位置（物理像素）。
    ///
    /// `WM_MOUSELEAVE` 不带坐标，而 `PointerLeave`（R02-39）要给出离开点；用最近一次移动的
    /// 位置是主流做法（Chromium 的 `WebMouseEvent.position_in_widget` 同源）。
    last_client_pos: (i32, i32),
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
    /// 上一次标题栏按下的时刻 + 光标屏幕坐标：`WM_NCLBUTTONDBLCLK` 缺失时的自计时双击兜底
    ///（win-2 P3）。
    last_caption_press: Option<CaptionPress>,
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
    /// 按下时的窗口矩形（物理像素）：`Esc` / 右键取消时原样还回去（win-2 P3）。
    origin: RECT,
    /// 按下时窗口是最大化：第一次拖动超过阈值后才还原（单击标题栏不还原，与系统一致）。
    restore: bool,
    /// 按下时的最大化状态（`restore` 在还原后清零，这个保留整个会话，供取消时决定是
    /// 恢复矩形还是重新最大化）。
    origin_zoomed: bool,
}

/// 上一次标题栏按下（自计时双击兜底，win-2 P3）。
#[derive(Clone, Copy)]
struct CaptionPress {
    /// 按下时刻（单调时钟）。
    at: Instant,
    /// 按下时的光标屏幕坐标（双击矩形以它为圆心）。
    cursor: POINT,
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
            wheel_lines: pump::wheel_lines_from_system(),
            last_client_pos: (0, 0),
            min_size: (1, 1),
            close_requested: false,
            mouse_tracking: false,
            resize: None,
            caption_drag: None,
            last_caption_press: None,
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

    /// 每条消息的翻译上下文（消息时间 + 系统滚轮行数）。
    pub(crate) fn context(&self, ticks_ms: u64) -> MessageContext {
        MessageContext {
            ticks_ms,
            wheel_lines: self.wheel_lines,
        }
    }

    /// 重新读取 `SPI_GETWHEELSCROLLLINES`（`WM_SETTINGCHANGE` 之后系统值可能变了）。
    pub(crate) fn refresh_wheel_lines(&mut self) {
        self.wheel_lines = pump::wheel_lines_from_system();
    }

    /// 记录最近一次客户区鼠标位置（`PointerLeave` 的离开点）。
    pub(crate) fn note_client_pos(&mut self, x: i32, y: i32) {
        self.last_client_pos = (x, y);
    }

    /// 最近一次客户区鼠标位置。
    pub(crate) fn client_pos(&self) -> (i32, i32) {
        self.last_client_pos
    }

    /// 压入一条带明确消息时间的平台事件（`WndProc` 的常规路径）。
    pub(crate) fn push_stamped(&mut self, event: PlatformEvent, ticks_ms: u64) {
        self.events.push(StampedEvent { event, ticks_ms });
    }

    /// 压入一条**引擎合成**的平台事件：时间取当前消息时间（`GetMessageTime()`），
    /// 因为合成发生在处理该消息的过程中，与消息时刻同源。
    pub(crate) fn push_now(&mut self, event: PlatformEvent) {
        let ticks_ms = pump::message_ticks();
        self.push_stamped(event, ticks_ms);
    }

    /// 压入一条已经带时间的平台事件（`WndProc` 的翻译产物）。
    pub(crate) fn push_stamped_event(&mut self, event: StampedEvent) {
        self.events.push(event);
    }

    pub(crate) fn take_events(&mut self) -> Vec<StampedEvent> {
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

    /// 禁用态下把系统可能重新关联的 IME 上下文再摘一次（win-1 P3；幂等）。
    pub(crate) fn reassert_disabled_ime(&mut self, hwnd: HWND) {
        self.translate.ime_mut().reassert_disabled(hwnd);
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

    /// 实现在 `caption::begin_resize`（薄委托）。
    fn begin_resize(&mut self, hwnd: HWND, hit: u32) {
        caption::begin_resize(self, hwnd, hit)
    }

    /// 实现在 `caption::note_resize_cursor`（薄委托）。
    fn note_resize_cursor(&mut self) {
        caption::note_resize_cursor(self)
    }

    /// 实现在 `caption::apply_resize`（薄委托）。
    fn apply_resize(&mut self, hwnd: HWND) {
        caption::apply_resize(self, hwnd)
    }

    /// 实现在 `caption::end_resize`（薄委托）。
    fn end_resize(&mut self, hwnd: HWND) {
        caption::end_resize(self, hwnd)
    }

    /// 实现在 `caption::begin_caption_drag`（薄委托）。
    fn begin_caption_drag(&mut self, hwnd: HWND) {
        caption::begin_caption_drag(self, hwnd)
    }

    /// 实现在 `caption::apply_caption_drag`（薄委托）。
    fn apply_caption_drag(&mut self, hwnd: HWND) {
        caption::apply_caption_drag(self, hwnd)
    }

    /// 实现在 `caption::end_caption_drag`（薄委托）。
    fn end_caption_drag(&mut self, hwnd: HWND) {
        caption::end_caption_drag(self, hwnd)
    }

    /// 实现在 `caption::cancel_caption_drag`（薄委托）。
    fn cancel_caption_drag(&mut self, hwnd: HWND) {
        caption::cancel_caption_drag(self, hwnd)
    }

    /// 实现在 `caption::self_timed_double_click`（薄委托）。
    fn self_timed_double_click(&mut self, hwnd: HWND) -> bool {
        caption::self_timed_double_click(self, hwnd)
    }

    /// 实现在 `caption::toggle_maximize`（薄委托）。
    fn toggle_maximize(&mut self, hwnd: HWND) {
        caption::toggle_maximize(self, hwnd)
    }
}
