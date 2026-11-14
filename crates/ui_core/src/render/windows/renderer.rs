//! 窗口渲染门面 + 每窗口一条 `kytrix-d3d12` GPU 线程
//! （`design/async-render-pipeline.md` §4.5 / §6）。
//!
//! ```
//! UI 线程                                  kytrix-d3d12 线程（每窗口）
//! ────────────────────────────────────    ──────────────────────────────────────────
//! 窗口消息（WM_SIZE 只记录尺寸）           循环：等 work event
//! publish(FrameRequest) ──► 最新帧槽            → 取最新帧 + 图集槽
//! queue_atlas_upload ─────► 图集槽              → surface 变化则 resize
//! poll() ◄──────────────── 反馈槽               → 等 waitable（每帧恰好一次）
//!                                               → 录制 → Execute → Signal
//!                                               → Present1(1, 0, dirty) → retire
//! ```
//!
//! 三条硬约束：
//! - **I1**：UI 线程只做「加锁拷贝 + `SetEvent`」，没有 fence wait / `ResizeBuffers` / `Present`。
//! - **I2**：`ExecuteCommandLists` 后立即返回；完成通知只通过 fence（资源按序号延迟释放，I4）。
//! - **I3**：节奏来自 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 句柄，
//!   只在 GPU 线程等待，且每帧（含首帧、含 resize 后首帧）恰好等一次。
//!
//! 关窗握手（§6.7）：UI 线程**不能**直接 `join` —— DXGI 的 `Present` 可能同步 `SendMessage`
//! 到窗口的消息泵线程，直接 join 会和「消息泵停转」互等。`Drop` 用 `MsgWaitForMultipleObjects`
//! 边泵消息边等 GPU 线程句柄，2 秒超时后放弃 join。

use std::{
    os::windows::io::AsRawHandle,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smol_str::{SmolStr, format_smolstr};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject},
    UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostQuitMessage,
        QS_ALLINPUT, TranslateMessage, WM_QUIT,
    },
};

#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "hud")]
use crate::render::timer::{GpuTimerSample, Section};
#[cfg(feature = "test-support")]
use crate::render::windows::readback::ReadbackImage;
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        DirtyRows, RenderStats,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, LatestFrame},
        timer::FrameTimings,
        windows::{
            compositor::{GpuRenderer, enter_gpu_thread},
            device::init_error_from,
        },
    },
    window::FrameIndex,
};

/// 关闭握手的等待上限（§6.7）。
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 门面：窗口路径是「最新帧槽 + GPU 线程句柄」，离屏路径是同步渲染器。
///
/// 两个分支都是 **Windows 后端内部**的目标区分（离屏 / 窗口），不是跨平台抽象：
/// `render/windows.rs` 在编译期就选定了这一份实现（I14）。
pub(crate) struct Renderer {
    target: RenderTarget,
    /// 粘性错误：GPU 线程退出后 `poll` 持续返回同一错误（`publish` 变 no-op）。
    error: Option<StickyError>,
}

enum RenderTarget {
    /// 离屏 / 快照：同步渲染（`Window::enable_offscreen_renderer`，§4.5 的下半段）。
    Offscreen(GpuRenderer),
    /// 窗口：门面 + 每窗口一条 GPU 线程（§6.1）。
    Window(WindowRenderer),
}

impl Renderer {
    /// 产品路径（离屏 / 快照）：同步渲染器，不建线程。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_offscreen(size)?),
            error: None,
        })
    }

    /// 测试用 WARP 离屏渲染器 + 指定图集页（快照必须与 `App` 的分配器同页尺寸）。
    #[cfg(feature = "test-support")]
    pub(crate) fn new_warp_offscreen_with_page(
        size: (u32, u32),
        atlas_page: u32,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_warp_offscreen_with_page(
                size, atlas_page,
            )?),
            error: None,
        })
    }

    /// 真实窗口：构造 GPU 对象（UI 线程）后整体移交给 `kytrix-d3d12` 线程。
    ///
    /// # Errors
    /// 设备 / swapchain 创建失败，或 GPU 线程无法启动（`AppInit`）。
    pub(crate) fn new_for_hwnd(hwnd: isize, size: (u32, u32)) -> Result<Self, EngineError> {
        let gpu = GpuRenderer::new_for_hwnd(hwnd, size)?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(gpu, size)?),
            error: None,
        })
    }

    /// 最近一次**发布**的目标尺寸（UI 侧记录；GPU 线程按它判断是否重建，§4.4）。
    pub(crate) fn surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.surface,
        }
    }

    /// 发布一帧（**不阻塞**）：窗口路径合并进最新帧槽并唤醒 GPU 线程；离屏路径同步渲染。
    ///
    /// 返回最近一次**已完成**帧的统计（窗口路径落后 ≤1 帧，HUD 允许）。
    pub(crate) fn publish(&mut self, request: FrameRequest) -> RenderStats {
        if self.error.is_some() {
            return RenderStats::default();
        }
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => {
                if gpu.surface() != request.surface
                    && let Err(error) = gpu.resize(request.surface)
                {
                    self.error = Some(StickyError::capture(error));
                    return RenderStats::default();
                }
                let mut frame = FramePlan::new(request.plan, gpu.is_fresh());
                let mut timings = FrameTimings::new();
                match gpu.submit_snapshot(request.index, &request.payload, &mut frame, &mut timings)
                {
                    Ok(stats) => stats,
                    Err(error) => {
                        self.error = Some(StickyError::capture(error));
                        RenderStats::default()
                    }
                }
            }
            RenderTarget::Window(window) => window.publish(request),
        }
    }

    /// 把一页图集像素交给 GPU 线程（每 `AtlasKind` 只留最新，§4.2）。
    pub(crate) fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => gpu.queue_atlas_upload(kind, pixels, rows),
            RenderTarget::Window(window) => window.queue_atlas_upload(kind, pixels, rows),
        }
    }

    /// 轮询反馈 / 错误（平台泵**每轮**调用；空闲时也跑）。
    ///
    /// GPU 线程以错误退出后，本方法持续返回同一个错误（粘性），直到 `Renderer` 被 drop。
    pub(crate) fn poll(&mut self) -> Result<Option<FrameFeedback>, EngineError> {
        if let Some(error) = &self.error {
            return Err(error.to_error());
        }
        let result = match &mut self.target {
            RenderTarget::Offscreen(_) => Ok(None),
            RenderTarget::Window(window) => window.poll(),
        };
        match result {
            Ok(feedback) => Ok(feedback),
            Err(error) => {
                let sticky = StickyError::capture(error);
                let echo = sticky.to_error();
                self.error = Some(sticky);
                Err(echo)
            }
        }
    }

    /// HUD 的分段计时：窗口路径来自 GPU 线程最近完成的帧。
    #[cfg(feature = "hud")]
    pub(crate) fn timing_sample(&self, section: Section) -> Option<GpuTimerSample> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => window
                .samples
                .iter()
                .find(|sample| sample.section == section)
                .copied(),
        }
    }

    /// 一次性渲染整帧并读回（快照用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => gpu.render_snapshot(buffer),
            RenderTarget::Window(_) => Err(EngineError::AppInit {
                detail: SmolStr::new_static(
                    "render_snapshot requires an offscreen renderer (window renderer is threaded)",
                ),
            }),
        }
    }
}

/// 粘性错误的重建源（§4.5：GPU 线程退出后 `poll` 持续返回同一错误）。
///
/// `EngineError` 不是 `Clone`（公开错误面冻结），而 `poll` 每轮都会被平台泵调用；因此记住
/// 产生错误的**结构字段**、每次重建一个语义相同的值。GPU 线程只产生这四个变体。
enum StickyError {
    DeviceLost { reason: SmolStr, log_path: PathBuf },
    Swapchain { detail: SmolStr },
    AppInit { detail: SmolStr },
    GeometryRegister { detail: SmolStr },
}

impl StickyError {
    fn capture(error: EngineError) -> Self {
        match error {
            EngineError::DeviceLost { reason, log_path } => Self::DeviceLost { reason, log_path },
            EngineError::Swapchain { detail } => Self::Swapchain { detail },
            EngineError::AppInit { detail } => Self::AppInit { detail },
            EngineError::GeometryRegister { detail } => Self::GeometryRegister { detail },
            // 其余变体不会从 GPU 线程产生；降级成 `AppInit` 并保留原始文案（不静默）。
            other => Self::AppInit {
                detail: format_smolstr!("{other}"),
            },
        }
    }

    fn to_error(&self) -> EngineError {
        match self {
            Self::DeviceLost { reason, log_path } => EngineError::DeviceLost {
                reason: reason.clone(),
                log_path: log_path.clone(),
            },
            Self::Swapchain { detail } => EngineError::Swapchain {
                detail: detail.clone(),
            },
            Self::AppInit { detail } => EngineError::AppInit {
                detail: detail.clone(),
            },
            Self::GeometryRegister { detail } => EngineError::GeometryRegister {
                detail: detail.clone(),
            },
        }
    }
}

/// 窗口渲染器的主线程句柄（最新帧槽 + 图集槽 + 反馈槽 + GPU 线程句柄）。
struct WindowRenderer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// 最近一次**发布**的目标尺寸（`publish` 里更新）。
    surface: (u32, u32),
    stats: RenderStats,
    #[cfg(feature = "hud")]
    samples: Vec<GpuTimerSample>,
    /// 最近一次「发布」的帧序号与时刻（反馈顺序断言：完成时刻不可能早于它自己的发布）。
    last_published: Option<(FrameIndex, Instant)>,
}

impl WindowRenderer {
    fn new(gpu: GpuRenderer, size: (u32, u32)) -> Result<Self, EngineError> {
        // `CreateEventW` 需要 `Win32_Security` feature（DEVIATIONS F-79）；auto-reset。
        // SAFETY: 无名称的 auto-reset 事件；句柄由 `Shared` 独占并在最后一个引用释放时关闭。
        let work = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW(work)", error))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work,
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kytrix-d3d12".to_owned())
            .spawn(move || gpu_loop(gpu, worker))
            .map_err(|error| EngineError::AppInit {
                detail: format_smolstr!("spawn kytrix-d3d12 thread: {error}"),
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            surface: (size.0.max(1), size.1.max(1)),
            stats: RenderStats::default(),
            #[cfg(feature = "hud")]
            samples: Vec::with_capacity(3),
            last_published: None,
        })
    }

    fn publish(&mut self, request: FrameRequest) -> RenderStats {
        self.last_published = Some((request.index, request.published_at));
        self.surface = request.surface;
        self.shared.publish_frame(request);
        self.stats
    }

    fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        self.shared.queue_atlas(kind, pixels, rows);
    }

    fn poll(&mut self) -> Result<Option<FrameFeedback>, EngineError> {
        if let Some(error) = self.shared.take_error() {
            return Err(error);
        }
        let Some(feedback) = self.shared.take_feedback() else {
            return Ok(None);
        };
        self.stats = feedback.stats;
        debug_assert!(
            feedback.index.get() >= 1,
            "feedback must name a committed frame"
        );
        debug_assert!(
            feedback.samples.len() <= 3,
            "at most three GPU sections per frame"
        );
        debug_assert!(
            self.last_published
                .is_none_or(|(index, published)| index != feedback.index
                    || feedback.completed_at >= published),
            "frame feedback completed before it was published"
        );
        #[cfg(feature = "hud")]
        {
            self.samples = feedback.samples.clone();
        }
        Ok(Some(feedback))
    }
}

impl Drop for WindowRenderer {
    fn drop(&mut self) {
        self.shared.request_shutdown();
        let Some(thread) = self.thread.take() else {
            return;
        };
        join_with_message_pump(thread);
    }
}

/// UI 线程与 GPU 线程共享的最新帧 / 图集 / 反馈槽 + 唤醒事件。
struct Shared {
    state: Mutex<State>,
    /// `publish` / 关闭之后的唤醒原语（auto-reset）。
    work: HANDLE,
}

// SAFETY: `work` 是进程范围的内核事件句柄；事件对象本身线程安全，`SetEvent` /
// `WaitForSingleObject` 允许跨线程使用。句柄由 `Shared::drop` 在最后一个引用释放时关闭。
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构独占，最后一个引用释放时关闭一次。
        unsafe {
            let _ = CloseHandle(self.work);
        }
    }
}

struct State {
    frame: LatestFrame,
    atlas: AtlasSlots,
    feedback: Option<FrameFeedback>,
    error: Option<EngineError>,
    shutdown: bool,
    /// GPU 线程已经退出（`publish` 从此是 no-op）。
    terminated: bool,
}

impl State {
    const fn new() -> Self {
        Self {
            frame: LatestFrame::new(),
            atlas: AtlasSlots::new(),
            feedback: None,
            error: None,
            shutdown: false,
            terminated: false,
        }
    }
}

/// 图集上传槽：每个 `AtlasKind` 只留最新一份（§4.2）。
struct AtlasSlots {
    glyph: Option<AtlasUpload>,
    image: Option<AtlasUpload>,
}

struct AtlasUpload {
    kind: AtlasKind,
    pixels: Vec<u8>,
    rows: DirtyRows,
}

impl AtlasSlots {
    const fn new() -> Self {
        Self {
            glyph: None,
            image: None,
        }
    }

    /// 新像素覆盖旧像素；`DirtyRows` 取并集。**发生覆盖时整页标记**：被丢掉的中间上传的
    /// 脏行永远不会再补，只有整页上传才能保证图集内容正确（§4.2）。
    fn queue(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        let slot = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        };
        let mut rows = rows;
        match slot {
            Some(existing) => {
                // 复用已分配的 16 MiB 容量，不重新分配。
                existing.pixels.clear();
                existing.pixels.extend_from_slice(pixels);
                existing.rows.merge(&mut rows);
                existing.rows.mark(0, u32::MAX);
            }
            None => {
                *slot = Some(AtlasUpload {
                    kind,
                    pixels: pixels.to_vec(),
                    rows,
                });
            }
        }
    }

    fn take(&mut self) -> [Option<AtlasUpload>; 2] {
        [self.glyph.take(), self.image.take()]
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // 锁只在极短的入队/出队区间持有；panic 恢复后状态仍可继续工作，不需要二次 panic。
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wake(&self) {
        // SAFETY: 事件句柄在 `Shared` 存活期间有效；`SetEvent` 可从任意线程调用。
        unsafe {
            let _ = SetEvent(self.work);
        }
    }

    fn publish_frame(&self, request: FrameRequest) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.frame.publish(request);
        drop(state);
        self.wake();
    }

    fn queue_atlas(&self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        // 不唤醒：图集上传统一在「录制下一帧」之前落地（§4.2），单独唤醒只会空转。
        state.atlas.queue(kind, pixels, rows);
    }

    fn has_pending_frame(&self) -> bool {
        self.lock().frame.has_pending()
    }

    fn take_frame(&self) -> Option<FrameRequest> {
        self.lock().frame.take()
    }

    fn take_atlas(&self) -> [Option<AtlasUpload>; 2] {
        self.lock().atlas.take()
    }

    fn shutdown_requested(&self) -> bool {
        self.lock().shutdown
    }

    fn publish_feedback(&self, feedback: FrameFeedback) {
        self.lock().feedback = Some(feedback);
    }

    fn publish_error(&self, error: EngineError) {
        let mut state = self.lock();
        state.feedback = None;
        state.error = Some(error);
    }

    fn take_feedback(&self) -> Option<FrameFeedback> {
        self.lock().feedback.take()
    }

    fn take_error(&self) -> Option<EngineError> {
        self.lock().error.take()
    }

    fn mark_terminated(&self) {
        self.lock().terminated = true;
    }

    fn request_shutdown(&self) {
        self.lock().shutdown = true;
        self.wake();
    }
}

/// GPU 线程主循环（§6.3）。
fn gpu_loop(mut renderer: GpuRenderer, shared: Arc<Shared>) {
    enter_gpu_thread();
    loop {
        // 空闲：睡在 work event 上（**没有**待画帧时不等 waitable，I3 的「每帧恰好一次」）。
        if !shared.has_pending_frame() {
            // SAFETY: work event 由 `Shared` 持有，UI 线程的 `Drop` 在 join 之后才释放它。
            unsafe { WaitForSingleObject(shared.work, INFINITE) };
        }
        if shared.shutdown_requested() {
            break;
        }
        let Some(request) = shared.take_frame() else {
            continue;
        };
        debug_assert!(
            request.scale > 0.0,
            "the published frame must carry a positive DPI scale"
        );
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2）。
        for upload in shared.take_atlas().into_iter().flatten() {
            renderer.queue_atlas_upload(upload.kind, &upload.pixels, upload.rows);
        }
        // §4.4：resize 不是独立命令，`FrameRequest.surface` 就是目标尺寸。
        if request.surface != renderer.surface() {
            match renderer.resize(request.surface) {
                Ok(()) => {}
                Err(error) if matches!(&error, EngineError::DeviceLost { .. }) => {
                    shared.publish_error(error);
                    break;
                }
                Err(_) => {
                    // §6.6：非设备丢失的 swapchain 失败（如 `DXGI_ERROR_INVALID_CALL`）不 panic、
                    // 不忙等：本帧放弃，下一次发布的帧会再比较 surface 并重试。重试成功的那一帧
                    // 强制全量（`fresh`），所以被放弃帧的 damage 不会留下错误像素。
                    continue;
                }
            }
        }
        // 没有像素要画（Idle 且目标未重建）⇒ 不产生 GPU 工作；该序号不会出现在完成通知里（§4.3）。
        let mut frame = FramePlan::new(request.plan, renderer.is_fresh());
        if frame.is_idle() {
            continue;
        }
        // I3：每帧渲染前**恰好**等一次 waitable（含首帧、含 resize 后首帧）。
        if let Some(waitable) = renderer.waitable() {
            // SAFETY: waitable 由该 swapchain 独占持有，只在 GPU 线程等待。
            unsafe { WaitForSingleObject(waitable, INFINITE) };
        }
        match renderer.submit_frame(&request, &mut frame) {
            Ok((stats, samples)) => shared.publish_feedback(FrameFeedback {
                index: request.index,
                stats,
                samples,
                // 命令列表完成时刻（近似呈现时刻）。
                completed_at: Instant::now(),
            }),
            Err(error) => {
                shared.publish_error(error);
                break;
            }
        }
    }
    shared.mark_terminated();
    // 离开作用域时 `GpuRenderer` 被 drop：等 GPU 完成、丢掉 back buffer 引用，再销毁
    // swapchain / 设备（§6.7 的关闭顺序）。
}

/// 边泵消息边等 GPU 线程退出（§6.7），超时后放弃 join。
///
/// 不能直接 `join`：DXGI 的 `Present` 可能同步 `SendMessage` 到本线程的消息泵，而本线程正在
/// join 里等 GPU 线程 —— 互等。这里用 `MsgWaitForMultipleObjects`（对象 = GPU 线程句柄）在
/// 等待期间继续派发消息。
fn join_with_message_pump(thread: JoinHandle<()>) {
    // `JoinHandle` 的平台句柄在 join / drop 之前有效；这里只读它（RawHandle 是 Copy）。
    let handle = HANDLE(thread.as_raw_handle());
    let deadline = Instant::now() + JOIN_TIMEOUT;
    let mut exited = false;
    let mut quit = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `handle` 由 `thread` 持有；`QS_ALLINPUT` 只观察本线程的消息队列。
        let wait =
            unsafe { MsgWaitForMultipleObjects(Some(&[handle]), false, millis, QS_ALLINPUT) };
        if wait == WAIT_OBJECT_0 {
            exited = true;
            break;
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            // 有消息：先派发，再重新计算剩余时间（关闭期间窗口仍然存活）。
            quit = pump_messages().or(quit);
            continue;
        }
        // 其余（WAIT_TIMEOUT / WAIT_FAILED / WAIT_ABANDONED_*）：超时或异常，放弃等待。
        break;
    }
    if exited {
        let _ = thread.join();
    } else {
        // 放弃 join（线程可能卡在驱动的等待里）：detach，避免把 UI 线程一起拖死。
        eprintln!(
            "kytrix: the kytrix-d3d12 thread did not exit within {}s; abandoning the join",
            JOIN_TIMEOUT.as_secs()
        );
    }
    if let Some(code) = quit {
        // 关闭握手期间取出的 `WM_QUIT` 还回去：泵的下一轮必须仍然看到它。
        // SAFETY: 只向本线程的消息队列投递退出请求（`PostQuitMessage` 的既有语义）。
        unsafe { PostQuitMessage(code) };
    }
}

/// 取出并派发本线程的消息；返回被取出的 `WM_QUIT` 退出码（不消费它）。
fn pump_messages() -> Option<i32> {
    let mut message = MSG::default();
    loop {
        // SAFETY: 本线程的消息队列；`message` 是栈上输出。
        let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
        if !has_message.as_bool() {
            return None;
        }
        if message.message == WM_QUIT {
            return Some(message.wParam.0 as i32);
        }
        // SAFETY: `message` 由 `PeekMessageW` 填充；`TranslateMessage` 可能产生 `WM_CHAR`。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{AtlasSlots, StickyError};
    use crate::{
        error::EngineError,
        paint::atlas::AtlasKind,
        render::{DirtyRows, compositor::Plan, frame::LatestFrame},
    };

    fn stale_rows(y0: u32, y1: u32) -> DirtyRows {
        let mut rows = DirtyRows::default();
        rows.mark(y0, y1);
        rows
    }

    #[test]
    fn a_first_atlas_upload_keeps_its_dirty_rows() {
        let mut slots = AtlasSlots::new();
        slots.queue(AtlasKind::Glyph, &[1, 2, 3], stale_rows(4, 8));
        let [glyph, image] = slots.take();
        let glyph = glyph.expect("glyph upload");
        assert_eq!(glyph.pixels, vec![1, 2, 3]);
        assert_eq!(glyph.rows.spans(), stale_rows(4, 8).spans());
        assert!(image.is_none());
        assert!(slots.take().iter().all(Option::is_none), "取走后槽位为空");
    }

    #[test]
    fn overwriting_an_upload_marks_the_whole_page() {
        let mut slots = AtlasSlots::new();
        slots.queue(AtlasKind::Glyph, &[1, 2, 3], stale_rows(4, 8));
        slots.queue(AtlasKind::Glyph, &[9, 9, 9], stale_rows(16, 20));
        let [glyph, _] = slots.take();
        let glyph = glyph.expect("latest glyph upload");
        assert_eq!(glyph.pixels, vec![9, 9, 9], "新像素覆盖旧像素");
        assert!(glyph.rows.is_full(), "被丢掉的中间上传的脏行不会再补");
    }

    #[test]
    fn the_latest_frame_merges_the_dropped_damage() {
        // 门面把「最新帧槽」原样用在这里：覆盖时合并 damage（§4.2）。
        let mut frame = LatestFrame::new();
        assert!(!frame.has_pending());
        let request = |index: u32, plan: Plan| crate::render::frame::FrameRequest {
            index: crate::window::FrameIndex::from_raw(index),
            payload: crate::render::frame::FramePayload::empty(),
            plan,
            surface: (1, 1),
            scale: 1.0,
            published_at: std::time::Instant::now(),
        };
        frame.publish(request(1, Plan::Full));
        frame.publish(request(2, Plan::Idle));
        assert_eq!(frame.take().expect("frame").plan, Plan::Full);
    }

    #[test]
    fn sticky_errors_rebuild_the_same_variant() {
        let error = EngineError::DeviceLost {
            reason: SmolStr::new_static("DXGI_ERROR_DEVICE_REMOVED"),
            log_path: std::path::PathBuf::from("logs/dred.txt"),
        };
        let sticky = StickyError::capture(error);
        match sticky.to_error() {
            EngineError::DeviceLost { reason, log_path } => {
                assert_eq!(reason, "DXGI_ERROR_DEVICE_REMOVED");
                assert_eq!(log_path, std::path::PathBuf::from("logs/dred.txt"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let sticky = StickyError::capture(EngineError::Swapchain {
            detail: SmolStr::new_static("ResizeBuffers: 0x887A0001"),
        });
        let text = sticky.to_error().to_string();
        assert!(text.contains("ResizeBuffers"), "{text}");
    }
}
