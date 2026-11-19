//! Windows 事件循环：`WndProc`、消息泵、唤醒合并、窗口生命周期
//!
//! 两条实测约束（probe）：
//! 1. `PeekMessageW` 会**取出并消费** `WM_QUIT`，泵必须在取出后显式判 `WM_QUIT` 再退出，
//!    否则后续 `GetMessageW` 永久阻塞（DEVIATIONS F-90）。
//! 2. `WM_PAINT` 不验证会形成风暴（100 ms / 24,576 条，V-63）：`wndproc` 的 `WM_PAINT` 分支
//!    只做 `BeginPaint`/`EndPaint`，不在这里绘制。
//!
//! `WndProc` **不做**任何 `App` 调用：它只把 [`PlatformEvent`] 压进 `WndProcState`，泵在
//! `DispatchMessageW` 返回后统一消费（Rust 借用规则 + 保持 `WndProc` 廉价）。
//!
//! ## 非客户区：全部交系统（R07-2）
//!
//! `WM_NCCALCSIZE` / `WM_NCHITTEST` / `WM_NCLBUTTONDOWN` **全部**交 `DefWindowProcW`：命中码、
//! 边框几何、缩放光标，以及 caption 的 move 模态循环与 8 个命中码的 size 模态循环都由系统
//! 提供（Chrome / Qt / GPUI 同款）。Aero Snap / Win11 Snap Layouts / 双击边框 /
//! `Alt+Space` → 大小 / 拖动中跨屏 DPI 因此全部免费。
//!
//! size 模态循环期间泵的 ④ 不运行，但**重排 + 重绘并没有停**：模态循环会同步
//! `DispatchMessage` 到本窗口的 `WndProc`，每一条 `WM_SIZE` 都由
//! [`resize::ResizeFrameDriver`] 直接驱动一帧真实帧（更新客户区 → 对齐合成几何 → `App::frame`）。
//! 这与 Qt 在 `WM_SIZE` 里同步布局 / 重绘、Chrome 在每个 size 步 `SetScaleAndSize` 之后合成
//! 是同一种结构：重绘由**系统分派的消息**驱动，而不是由引擎自己的顶层消息循环驱动。
//!
//! ## 帧节奏：每窗口 GPU 门禁 + UI 侧节拍线程（R07-10 / A2 / A5）
//!
//! present 节奏由每窗口 GPU 线程的 DXGI waitable 门禁决定（A2 删除了合成线程的进程级时钟：
//! 收到 delta 立即编码发布，不再等一拍）。UI 侧的 [`pump::FramePacer`] 只在「有动画」或
//! 「有被帧预算门禁推迟的帧」时存在，按窗口所在 output 的刷新周期唤醒泵；`WM_TIMER` 只保留
//! 给 a11y 补发兜底；空闲时**没有**任何定时器或线程在跑。

use std::{rc::Rc, sync::Arc};

use windows::Win32::Foundation::HWND;

use crate::{
    error::EngineError,
    math::{PhysicalRect, Scale},
    platform::{
        dispatcher::Dispatcher,
        event::{MessageContext, PlatformEvent, StampedEvent, TranslateState},
        windows::{
            a11y::WindowsA11y,
            dpi,
            ime::{ImeApplied, ImeCommand},
        },
    },
    window::WindowId,
};

mod native_window;
mod pump;
mod recovery;
mod resize;
mod wndproc;

pub(crate) use pump::run;
pub(crate) use resize::{ResizeFrameDriver, ResizeStep};
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
    /// `WM_GETMINMAXINFO` 的 `ptMinTrackSize`（物理像素，随 DPI 同步换算）。
    min_size: (u32, u32),
    /// 当前消息时刻的窗口比例（`WM_DPICHANGED` **同步**更新，R07-12）。
    ///
    /// 同批次后续消息的物理→逻辑换算、`StampedEvent::scale` 与
    /// `WM_GETMINMAXINFO` 的最小尺寸都读它；`NativeWindow::scale` 是同一值的窗口侧副本，
    /// 由泵在派发 `Dpi` 事件时对齐。
    scale: Scale,
    close_requested: bool,
    /// 待泵消费的显示 / 电源恢复请求（R07-14；同一批里的多条消息合并成一次）。
    ///
    /// `WndProc` 只置位并唤醒（判定见 [`recovery::recovery_plan`]），真正的副作用（重新查询
    /// 显示器、重建交换链、全量重绘）在泵的 ②a′ 执行 —— `WndProc` 不做任何会重入的调用
    /// （文件头纪律 1）。
    display_recovery: bool,
    /// 是否已注册 `TrackMouseEvent(TME_LEAVE)`（每进入一次客户区注册一次）。
    mouse_tracking: bool,
    /// a11y（P11：首显前构造的 `accesskit_windows::Adapter` + `WM_GETOBJECT` 接线）。
    a11y: WindowsA11y,
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
    /// 系统 size / move 模态循环是否进行中（`WM_ENTERSIZEMOVE` → `WM_EXITSIZEMOVE`）。
    ///
    /// 会话期间泵的 ④ 不运行：重排 + 重绘由模态循环分派的 `WM_SIZE` 直接驱动
    /// （[`ResizeFrameDriver`]），与 Qt / Chrome 同构。
    size_move: bool,
    /// 模态循环里驱动出的粘性错误（帧管线 / 合成调用失败）：由泵的下一轮 ② 冒泡。
    ///
    /// `WndProc` 没有错误通道，但绝不能把失败吞掉 —— 帧管线失败在用户看来就是「画面冻结」。
    pending_error: Option<EngineError>,
    /// 系统 size 模态循环里的成帧驱动（连接泵持有的 `App` 与本窗口的合成目标）。
    ///
    /// `None` = 没有 `App`（平台单测里只验证 `WndProc` 记账、不真正建渲染器的窗口）。
    driver: Option<Rc<ResizeFrameDriver>>,
}

impl WndProcState {
    pub(crate) fn new(
        dispatcher: Arc<Dispatcher>,
        hwnd: HWND,
        window: WindowId,
        driver: Option<Rc<ResizeFrameDriver>>,
    ) -> Result<Self, EngineError> {
        let dpi = dpi::window_dpi(hwnd);
        let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
            factor: dpi as f32 / dpi::BASE_DPI as f32,
        })?;
        Ok(Self {
            dispatcher: dispatcher.clone(),
            events: Vec::with_capacity(8),
            translate: TranslateState::default(),
            wheel_lines: pump::wheel_lines_from_system(),
            last_client_pos: (0, 0),
            min_size: dpi::min_track_size(dpi),
            scale,
            close_requested: false,
            display_recovery: false,
            mouse_tracking: false,
            // **必须在窗口显示之前构造**：`Adapter::new` 内部 `init_uia()`，
            // 且不得在处理 `WM_GETOBJECT` 时构造（adapter.rs:411-441）。
            a11y: WindowsA11y::new(hwnd, dispatcher, window)?,
            suppress_capture_cancel: false,
            cursor: crate::paint::style::Cursor::Default,
            size_move: false,
            pending_error: None,
            driver,
        })
    }

    /// a11y 状态（`WM_GETOBJECT` / 焦点消息 / 每轮交付的唯一入口）。
    pub(crate) fn a11y_mut(&mut self) -> &mut WindowsA11y {
        &mut self.a11y
    }

    fn wake(&self) {
        self.dispatcher.wake();
    }

    /// 当前消息时刻的窗口比例（事件换算与 `WM_GETMINMAXINFO` 都用它）。
    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    /// `WM_DPICHANGED` 的同步记账（R07-12）：新比例 + 新的最小跟踪尺寸。
    ///
    /// 必须在**投递 `Dpi` 事件之前**、在 `SetWindowPos(建议矩形)` 之前调用：`SetWindowPos`
    /// 会同步派发 `WM_SIZE`，那些事件的 `StampedEvent::scale` 与 `WM_GETMINMAXINFO` 都要看到
    /// 新值。
    pub(crate) fn note_dpi(&mut self, dpi: u32, scale: Scale) {
        self.scale = scale;
        self.min_size = dpi::min_track_size(dpi);
    }

    /// 每条消息的翻译上下文（消息时间 + 系统滚轮行数 + 当时的 scale）。
    pub(crate) fn context(&self, ticks_ms: u32) -> MessageContext {
        MessageContext {
            ticks_ms,
            wheel_lines: self.wheel_lines,
            scale: self.scale,
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
    pub(crate) fn push_stamped(&mut self, event: PlatformEvent, ticks_ms: u32) {
        self.push_stamped_event(StampedEvent {
            event,
            ticks_ms,
            scale: self.scale,
        });
    }

    /// 压入一条**引擎合成**的平台事件：时间取当前 tick（`GetTickCount64() as u32`）。
    ///
    /// **不能**用 `GetMessageTime()`（R02-14）：合成点可能在消息循环之外（泵在
    /// `take_capture_requests` 里调 `SetCapture`/`ReleaseCapture`），此时它返回的是上一条
    /// 已取出消息的时间 —— 陈旧值。`tick_now` 与 `MSG.time` 同源同宽（32 位、同回绕），
    /// 因此与消息时间可以直接相减。
    pub(crate) fn push_now(&mut self, event: PlatformEvent) {
        let ticks_ms = pump::tick_now();
        self.push_stamped(event, ticks_ms);
    }

    /// 压入一条已经带时间的平台事件（`WndProc` 的翻译产物）。
    ///
    /// **同一批内合并 `WM_MOUSEMOVE`**（R07-13）：系统以最高 1000 Hz 投递鼠标移动，一批
    /// （约一帧）里常有 16 条；只有最后一条位置对渲染有意义，中间位置的命中测试、hover 迁移
    /// 与光标写入都是纯浪费（Chrome 的 viz 输入合并同款）。
    ///
    /// 合并条件严格：**末条与新条都是 `Pointer{Move}`**。任何按钮/滚轮/进入/离开事件都会切断
    /// 合并链，因此事件顺序语义不变（拖动选择时按钮之间的移动仍然各自可辨）。
    pub(crate) fn push_stamped_event(&mut self, stamped: StampedEvent) {
        let is_move = matches!(
            &stamped.event,
            PlatformEvent::Pointer {
                kind: crate::platform::event::PointerKind::Move,
                ..
            }
        );
        if is_move
            && let Some(last) = self.events.last_mut()
            && matches!(
                last.event,
                PlatformEvent::Pointer {
                    kind: crate::platform::event::PointerKind::Move,
                    ..
                }
            )
        {
            // 覆盖末条：位置、消息时间、scale 全部取最新（同一帧内必须用同一个 scale 解释，
            // 而 `scale` 只会随 `WM_DPICHANGED` 变化，那个消息本身会切断合并链）。
            *last = stamped;
            return;
        }
        self.events.push(stamped);
    }

    pub(crate) fn take_events(&mut self) -> Vec<StampedEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    /// 登记一次显示 / 电源恢复（R07-14）：显示器拓扑、工作区或睡眠唤醒之后由 `WndProc`
    /// 置位，泵的 ②a′ 消费（动作序列见 [`recovery::RECOVERY_ACTIONS`]）。
    pub(crate) fn note_display_recovery(&mut self) {
        self.display_recovery = true;
    }

    /// 取走显示 / 电源恢复请求（同一批里的多次触发只执行一次）。
    pub(crate) fn take_display_recovery(&mut self) -> bool {
        std::mem::take(&mut self.display_recovery)
    }

    /// 本窗口的 IME **已应用**状态（泵每轮与 `App` 的目标状态比较）。
    pub(crate) fn ime_enabled(&self) -> bool {
        self.translate.ime().enabled()
    }

    /// 计划一次 IME 启用/禁用；返回的命令必须**在不持有 `&mut self` 时**执行（R07-7）。
    pub(crate) fn plan_ime_enabled(&mut self, enabled: bool) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_enabled(enabled)
    }

    /// 计划一次 IME caret 区域更新（`None` = 清除 hint）；命令同样要在借用外执行。
    pub(crate) fn plan_ime_cursor_area(
        &mut self,
        area: Option<PhysicalRect>,
    ) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_cursor_area(area)
    }

    /// 窗口销毁前计划把 IME 上下文还回去（避免 `HIMC` 泄漏）。
    pub(crate) fn plan_ime_release(&mut self) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_release_before_destroy()
    }

    /// 禁用态下计划再摘一次被系统重新关联的上下文（幂等）。
    pub(crate) fn plan_ime_reassert(&mut self) -> Option<ImeCommand> {
        self.translate.ime_mut().plan_reassert_disabled()
    }

    /// 写回一条 IMM32 命令的执行结果（唯一的状态回写点）。
    pub(crate) fn note_ime_applied(&mut self, applied: ImeApplied) {
        self.translate.ime_mut().note_applied(applied);
    }

    /// 写回「禁用态重新摘除」的结果：不覆盖已保存的待恢复上下文。
    pub(crate) fn note_ime_reasserted(&mut self, applied: ImeApplied) {
        self.translate.ime_mut().note_reasserted(applied);
    }

    /// 当前修饰键（事件投递时随按键一起给应用层）。
    pub(crate) fn modifiers(&self) -> crate::event::Modifiers {
        self.translate.modifiers()
    }

    /// 复位瞬时输入状态（修饰键 / 半高代理对 / IME 待抑制码元）。失焦时由 `WndProc` 调用。
    pub(crate) fn reset_transient_input(&mut self) {
        self.translate.reset_transient();
    }

    /// 一轮消息排空结束：推进 IME 提交抑制队列的消息批次号（泵每轮 ① 之前调用）。
    ///
    /// 队列只在「入队批次 + 紧随其后的一轮」内匹配，避免「提交后系统不补发 `WM_CHAR`」的残留
    /// 跨会话吞掉用户真实键入（AR3 §1.5 / P2-2）。
    pub(crate) fn end_message_batch(&mut self) {
        self.translate.end_message_batch();
    }

    /// 记录引擎当前光标（`WM_SETCURSOR` 据此重新应用，P2-3）。
    pub(crate) fn set_cursor(&mut self, cursor: crate::paint::style::Cursor) {
        self.cursor = cursor;
    }

    /// `WM_ENTERSIZEMOVE`：系统 size / move 模态循环开始（期间泵的 ④ 不运行）。
    pub(crate) fn begin_size_move(&mut self) {
        self.size_move = true;
    }

    /// `WM_EXITSIZEMOVE`：模态循环结束；之后回到泵的常规出帧路径。
    pub(crate) fn end_size_move(&mut self) {
        self.size_move = false;
    }

    /// 会话中的一条 `WM_SIZE`：取出「要驱动的一步」（**短借用**，纪律 1）。
    ///
    /// 真正的帧驱动由调用方在**离开本借用之后**执行（[`ResizeStep::run`]）：
    /// `App::frame` 的调用面很大，不能赌它不会重入 `WndProc`。
    pub(crate) fn resize_step(&self, client: (u32, u32)) -> Option<ResizeStep> {
        if !self.size_move {
            return None;
        }
        let driver = self.driver.as_ref()?;
        Some(ResizeStep::new(Rc::clone(driver), self.scale, client))
    }

    /// 登记模态循环里产生的粘性错误（泵的下一轮 ② 冒泡）。
    pub(crate) fn note_pending_error(&mut self, error: EngineError) {
        self.pending_error.get_or_insert(error);
    }

    /// 取走模态循环里登记的粘性错误（泵的 ② 步骤每轮检查一次）。
    pub(crate) fn take_pending_error(&mut self) -> Option<EngineError> {
        self.pending_error.take()
    }
}
