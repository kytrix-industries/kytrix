//! 窗口渲染门面 + 每窗口一条 `kytrix-d3d12` GPU 线程
//! （`design/async-render-pipeline.md` §4.5 / §6）。
//!
//! ```text
//! UI 线程                                  kytrix-d3d12 线程（每窗口）
//! ────────────────────────────────────    ──────────────────────────────────────────
//! 窗口消息（WM_SIZE 只记录尺寸）           循环：等 work / completion → 空转或取帧
//! publish(FrameRequest) ──► 最新帧槽            → 等 waitable（呈现背压，每帧恰好一次）
//! queue_atlas_upload ─────► 图集槽              → 取「此刻最新」帧 + 图集槽
//! poll() ◄──────────────── 反馈槽               → surface 变化则 resize
//!                                               → 槽 fence → 录制 → Execute → Signal
//!                                               → Present1(1, 0, dirty)
//!                                               → 完成观测点：retire + 反馈
//! ```
//!
//! 反馈在**完成观测点**发布（fence 的 `SetEventOnCompletion` 唤醒），不是 `Present1` 返回的时刻；
//! retire 与反馈同一观测点推进（§4.1 / §6.5）。
//!
//! 三条硬约束：
//! - **I1**：UI 线程只做「加锁拷贝 + `SetEvent`」，没有 fence wait / `ResizeBuffers` / `Present`。
//! - **I2**：`ExecuteCommandLists` 后立即返回；完成通知只通过 fence（资源按序号延迟释放，I4）。
//! - **I3**：节奏来自 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 句柄，
//!   只在 GPU 线程等待，且每帧（含首帧、含 resize 后首帧）恰好等一次。
//!
//! 关窗握手（§6.7）：UI 线程**不能**直接 `join` —— DXGI 的 `Present` 可能同步 `SendMessage`
//! 到窗口的消息泵线程，直接 join 会和「消息泵停转」互等。`Drop` 用 `MsgWaitForMultipleObjects`
//! 边泵消息边等 GPU 线程句柄，**没有超时**（无限等到线程退出；只有 `WAIT_FAILED` 这类异常才
//! 记录并放弃）。

use std::{
    collections::VecDeque,
    os::windows::io::AsRawHandle,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Instant,
};

use smol_str::{SmolStr, format_smolstr};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
    UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostQuitMessage,
        QS_ALLINPUT, TranslateMessage, WM_QUIT,
    },
};

#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "hud")]
use crate::render::timer::Section;
#[cfg(feature = "test-support")]
use crate::render::windows::readback::ReadbackImage;
use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, color::Color},
    render::{
        DirtyRows, RenderStats,
        color::background_srgb,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, LatestFrame, MAX_FRAMES_IN_FLIGHT},
        timer::{FrameTimings, GpuTimerSample},
        windows::{
            compositor::{GpuRenderer, enter_gpu_thread},
            device::init_error_from,
        },
    },
    window::FrameIndex,
};

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
    pub(crate) fn new_for_hwnd(
        hwnd: isize,
        size: (u32, u32),
        background: Color,
    ) -> Result<Self, EngineError> {
        let gpu = GpuRenderer::new_for_hwnd(hwnd, size, background_srgb(background))?;
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
            RenderTarget::Offscreen(gpu) => gpu.queue_atlas_upload(kind, pixels.to_vec(), rows),
            RenderTarget::Window(window) => window.queue_atlas_upload(kind, pixels, rows),
        }
    }

    /// 改窗口背景色（UI 线程只写共享槽 + `wake`，`SetBackgroundColor` 在 GPU 线程执行）。
    ///
    /// 离屏目标没有背景色（从不 present），是 no-op。
    pub(crate) fn set_background_color(&mut self, color: Color) {
        match &self.target {
            RenderTarget::Offscreen(_) => {}
            RenderTarget::Window(window) => {
                window.set_background_color(background_srgb(color));
            }
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
        // `CreateEventW` 需要 `Win32_Security` feature（DEVIATIONS F-79）；两个都是 auto-reset。
        // SAFETY: 无名称的 auto-reset 事件；句柄由 `Shared` 独占并在最后一个引用释放时关闭。
        let work = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW(work)", error))?;
        // SAFETY: 同上；失败时先关闭已经创建的 `work`，不泄漏句柄。
        let completion = match unsafe { CreateEventW(None, false, false, None) } {
            Ok(event) => event,
            Err(error) => {
                // SAFETY: `work` 由本函数独占，尚未交给 `Shared`。
                unsafe {
                    let _ = CloseHandle(work);
                }
                return Err(init_error_from("CreateEventW(completion)", error));
            }
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work,
            completion,
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

    fn set_background_color(&self, rgba: [f32; 4]) {
        self.shared.set_background(rgba);
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
        // `latency` 的定义是 `completed_at - published_at`：本帧的发布时刻可查时，它不能超过
        // 反馈自己记录的完成延迟（Windows 的 `latency` 与 `completed_at` 各取一次 `now`）。
        debug_assert!(
            self.last_published
                .is_none_or(|(index, published)| index != feedback.index
                    || feedback.latency
                        <= feedback.completed_at.saturating_duration_since(published)),
            "frame latency must not exceed the feedback's completion delay"
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

/// GPU 线程的 panic 出路：展开时把错误交给 UI（`poll()` 不会静默卡死），并标记线程已终止。
///
/// 正常退出时 [`PanicGuard::disarm`] 消费自己，**不覆盖**循环已经发布的错误（§6.7）。
struct PanicGuard {
    shared: Arc<Shared>,
    armed: bool,
}

impl PanicGuard {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    /// 正常退出：GPU 线程已经按自己的错误路径收尾，guard 不再发布任何东西。
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if self.armed && std::thread::panicking() {
            self.shared.publish_panic();
        }
    }
}

/// UI 线程与 GPU 线程共享的最新帧 / 图集 / 反馈槽 + 唤醒事件。
struct Shared {
    state: Mutex<State>,
    /// `publish` / 关闭之后的唤醒原语（auto-reset）。
    work: HANDLE,
    /// 在飞帧的 fence 完成通知（auto-reset；GPU 线程用 `SetEventOnCompletion` 设置）。
    completion: HANDLE,
}

// SAFETY: `work` / `completion` 是进程范围的内核事件句柄；事件对象本身线程安全，
// `SetEvent` / `WaitForMultipleObjects` 允许跨线程使用。句柄由 `Shared::drop` 在最后一个
// 引用释放时关闭。
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: 两个句柄都由本结构独占，最后一个引用释放时各关闭一次。
        unsafe {
            let _ = CloseHandle(self.completion);
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
    /// 待应用的背景色（最新者胜；GPU 线程空闲/取帧后都会取走）。
    background: Option<[f32; 4]>,
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
            background: None,
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

    /// 排队一笔背景色变更（最新者胜）：GPU 线程空闲或录制下一帧前应用。
    fn set_background(&self, rgba: [f32; 4]) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.background = Some(rgba);
        drop(state);
        self.wake();
    }

    /// 取走待应用的背景色（取一次清空）。
    fn take_background(&self) -> Option<[f32; 4]> {
        self.lock().background.take()
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

    /// GPU 线程 panic：`poll` 必须看到一条错误而不是静默停摆；不覆盖已有错误，并让后续
    /// `publish` 变成 no-op。
    fn publish_panic(&self) {
        let mut state = self.lock();
        state.feedback = None;
        if state.error.is_none() {
            state.error = Some(EngineError::AppInit {
                detail: SmolStr::new_static("kytrix-d3d12 thread panicked"),
            });
        }
        state.terminated = true;
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
        // GPU 线程可能正阻塞在 [waitable, completion] 上（`work` 不在那个等待集合里）：置位
        // completion 让它从呈现背压里醒来看 shutdown。completion 是 auto-reset；没有等待者时
        // 置位也不会丢事件——下一次等待立刻返回，循环重新检查 shutdown。
        // SAFETY: 事件句柄在 `Shared` 存活期间有效；`SetEvent` 可从任意线程调用。
        unsafe {
            let _ = SetEvent(self.completion);
        }
        self.wake();
    }
}

/// 已提交、尚未观察到 fence 完成的帧（提交顺序 = 反馈发布顺序，§4.1）。
struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    /// 提交时的 CPU 采样（Upload / Present）；GPU 采样在完成观测点追加。
    samples: Vec<GpuTimerSample>,
    /// HUD 的 `latency` 起点（`FrameRequest::published_at`）。
    published_at: Instant,
    signal_value: u64,
    /// ring 槽位：完成时从该槽的 timestamp 读回缓冲读 GPU 时间。
    slot: usize,
}

/// GPU 线程主循环（§6.3）。
fn gpu_loop(renderer: GpuRenderer, shared: Arc<Shared>) {
    enter_gpu_thread();
    // panic 出路：展开时把错误交给 UI（`poll()` 不会静默卡死），并标记线程已终止。
    let guard = PanicGuard::new(Arc::clone(&shared));
    // `GpuRenderer` 必须是**局部** `Option`：循环结束后显式 drop（等 GPU 完成、丢掉 back buffer
    // 引用，再销毁 swapchain / 设备），`join` 返回时 teardown 一定已经完成（§6.7）。
    let mut renderer = Some(renderer);
    let mut submitted = VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT);
    if let Err(error) = run_gpu_loop(
        renderer
            .as_mut()
            .expect("renderer is present until teardown"),
        &shared,
        &mut submitted,
    ) {
        shared.publish_error(error);
    }
    drop(renderer.take());
    shared.mark_terminated();
    guard.disarm();
}

fn run_gpu_loop(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
) -> Result<(), EngineError> {
    loop {
        // 完成通知的唯一处理点：按提交顺序发布反馈 + 推进 retire（§4.1 / §6.5）。
        drain_completed(renderer, shared, submitted);
        if shared.shutdown_requested() {
            return Ok(());
        }
        // 空闲时也要应用待定背景色：改色不要求提交一帧（UI 只写共享槽 + 唤醒）。
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba)?;
        }
        if !shared.has_pending_frame() {
            // 空闲：睡在 [work, completion] 上（完成由 fence 的 `SetEventOnCompletion` 明确唤醒，
            // 不轮询；没有待画帧时也不等 waitable，I3 的「每帧恰好一次」）。
            wait_for_work_or_completion(shared)?;
            continue;
        }
        // I3：先等呈现节奏，再取「此刻最新」的帧 —— 等待期间 UI 的发布覆盖旧请求并合并 damage，
        // 渲染的是最新快照而不是最多等了一个 vsync 的旧快照（§4.2 的「帧是状态不是队列」）。
        // 等待期间 completion 也可能到达：先发布反馈再继续等同一个 waitable（auto-reset 只能
        // 消费一次，不能「等到了又重等」；关闭是这条规则的唯一例外，见 `wait_for_waitable`）。
        wait_for_waitable(renderer, shared, submitted)?;
        if shared.shutdown_requested() {
            // 关闭：waitable 可能刚被消费（它与关闭同时到达），本帧不 present —— swapchain 随
            // 线程退出一起销毁，没有下一帧需要这个令牌。
            return Ok(());
        }
        // waitable 一旦被消费就必须 present（§6.3 的 token 守恒）：`has_pending_frame()` 已在
        // 等待之前为真，且帧槽只有本线程取走，因此这里必然取得到帧；取不到只可能是未来的
        // 重构违反了「等待前先确认有活」。
        let Some(request) = shared.take_frame() else {
            debug_assert!(false, "waitable consumed without a pending frame");
            continue;
        };
        // 等 waitable 期间到达的背景色：与帧一起落地（仍然是录制前，不引入新的 GPU 调用点）。
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba)?;
        }
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2；像素按所有权移交给 GPU）。
        for upload in shared.take_atlas().into_iter().flatten() {
            renderer.queue_atlas_upload(upload.kind, upload.pixels, upload.rows);
        }
        // §4.4：resize 不是独立命令，`FrameRequest.surface` 就是目标尺寸。
        // §6.6：任何 resize 错误（含非设备丢失的 `DXGI_ERROR_INVALID_CALL`）都是显式失败——
        // 不重试、不丢帧：发布粘性错误并退出线程（与 Linux/macOS 一致）。
        if request.surface != renderer.surface() {
            renderer.resize(request.surface)?;
        }
        // 没有像素要画（Idle 且目标未重建）⇒ 不产生 GPU 工作；该序号不会出现在完成通知里（§4.3）。
        // 但 waitable 已经被消费：这样的帧不能走到这里（`window.rs` 在目标未重建时根本不发布
        // Idle 计划；surface 变化会先 resize 并把 `fresh` 置真），因此只作不变量断言。
        let mut frame = FramePlan::new(request.plan, renderer.is_fresh());
        if frame.is_idle() {
            debug_assert!(false, "idle frame consumed a waitable token");
            continue;
        }
        let slot = renderer.acquire_slot();
        renderer.wait_slot_fence(slot)?;
        // 该槽的上一帧刚被等到：在录制覆盖它的 timestamp 读回缓冲**之前**发布它的反馈
        // （`ResolveQueryData` 的写入与 CPU 读回共用这一块缓冲，必须按完成顺序串行化）。
        drain_completed(renderer, shared, submitted);
        let (stats, samples) = renderer.submit_frame(&request, &mut frame, slot)?;
        submitted.push_back(SubmittedFrame {
            index: request.index,
            stats,
            samples,
            published_at: request.published_at,
            signal_value: slot.signal_value,
            slot: slot.index,
        });
        // 明确唤醒：该帧 GPU 工作完成时 fence 置位 `completion`（auto-reset），不忙轮询。
        renderer.set_completion_event(slot.signal_value, shared.completion)?;
    }
}

/// 发布已观察到完成的帧（提交顺序）并推进 retire。
fn drain_completed(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
) {
    let completed = renderer.completed_value();
    let mut observed = false;
    while submitted
        .front()
        .is_some_and(|frame| frame.signal_value <= completed)
    {
        let frame = submitted.pop_front().expect("front was checked above");
        let mut samples = frame.samples;
        if let Some(sample) = renderer.resolve_frame_timer(frame.slot) {
            samples.push(sample);
        }
        // 观察到完成的时刻（fence / `SetEventOnCompletion` 醒来），不是 Present 返回的时刻。
        let completed_at = Instant::now();
        shared.publish_feedback(FrameFeedback {
            index: frame.index,
            stats: frame.stats,
            samples,
            completed_at,
            // UI 发布 → GPU 完成（HUD 的 publish→present）。
            latency: completed_at.saturating_duration_since(frame.published_at),
        });
        observed = true;
    }
    if observed {
        // I4：只把 fence 已到达的帧所引用的旧资源交回 device。
        renderer.release_completed();
    }
}

/// 空闲等待：`[work_event, completion_event]`（都是 auto-reset，不会自旋）。
fn wait_for_work_or_completion(shared: &Shared) -> Result<(), EngineError> {
    wait_for_pair(shared.work, shared.completion, "work, completion").map(|_| ())
}

/// 等帧延迟 waitable；期间 completion 到达就先发布反馈、再继续等（不把反馈拖到下一帧）。
///
/// waitable 是 auto-reset：一旦这个循环消费了它，本帧就不允许再等第二次（否则会白等一个
/// vsync），因此返回 `Ok` 之后调用方直接开始录制。**只有 waitable 分支才是消费**；completion
/// 分支（帧完成 / 关闭）没有消费 waitable，返回后调用方必须自己检查 shutdown（§6.3）。
fn wait_for_waitable(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
) -> Result<(), EngineError> {
    let Some(waitable) = renderer.waitable() else {
        return Ok(());
    };
    loop {
        if wait_for_pair(waitable, shared.completion, "waitable, completion")? == 0 {
            return Ok(());
        }
        drain_completed(renderer, shared, submitted);
        // 关闭：`request_shutdown` 置位 completion 把我们从这里唤醒。此时 waitable 没有被
        // 消费，不能再去等它（驱动/遮挡让它不再 signal 时关闭会永不返回）——直接早退。
        if shared.shutdown_requested() {
            return Ok(());
        }
    }
}

/// `WaitForMultipleObjects([low, high])`；返回被 signal 的句柄下标（0 或 1）。
fn wait_for_pair(low: HANDLE, high: HANDLE, label: &str) -> Result<usize, EngineError> {
    // SAFETY: 两个句柄都由 `Shared`（或 swapchain）持有，调用方保证它们的生命周期覆盖本次等待。
    let wait = unsafe { WaitForMultipleObjects(&[low, high], false, INFINITE) };
    match wait.0 {
        0 => Ok(0),
        1 => Ok(1),
        // 无超时 + 无 mutex：只剩 `WAIT_FAILED`（或驱动返回的异常值）。
        _ => Err(EngineError::AppInit {
            detail: format_smolstr!(
                "WaitForMultipleObjects({label}) failed: {wait:?} ({})",
                std::io::Error::last_os_error()
            ),
        }),
    }
}

/// 边泵消息边等 GPU 线程退出（§6.7），**没有超时**。
///
/// 不能直接 `join`：DXGI 的 `Present` 可能同步 `SendMessage` 到本线程的消息泵，而本线程正在
/// join 里等 GPU 线程 —— 互等。这里用 `MsgWaitForMultipleObjects`（对象 = GPU 线程句柄）在
/// 等待期间继续派发消息，直到线程句柄 signaled；只有 `WAIT_FAILED` 这类异常才记录并放弃。
fn join_with_message_pump(thread: JoinHandle<()>) {
    // `JoinHandle` 的平台句柄在 join / drop 之前有效；这里只读它（RawHandle 是 Copy）。
    let handle = HANDLE(thread.as_raw_handle());
    let mut quit = None;
    loop {
        // SAFETY: `handle` 由 `thread` 持有；`QS_ALLINPUT` 只观察本线程的消息队列。
        let wait =
            unsafe { MsgWaitForMultipleObjects(Some(&[handle]), false, INFINITE, QS_ALLINPUT) };
        if wait == WAIT_OBJECT_0 {
            let _ = thread.join();
            break;
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            // 有消息：先派发，再继续等（关闭期间窗口仍然存活）。
            quit = pump_messages().or(quit);
            continue;
        }
        // 异常（`WAIT_FAILED`；无超时 + 无 mutex，不会出现 `WAIT_TIMEOUT` / `WAIT_ABANDONED_*`）：
        // 记录后放弃 join，避免把 UI 线程一起拖死。`thread` 在此 drop（detach）。
        eprintln!(
            "kytrix: MsgWaitForMultipleObjects while joining the kytrix-d3d12 thread failed: {:?} ({})",
            wait,
            std::io::Error::last_os_error()
        );
        break;
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
    use std::sync::{Arc, Mutex};

    use smol_str::SmolStr;

    use super::{AtlasSlots, PanicGuard, Shared, State, StickyError};
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

    fn test_shared() -> Arc<Shared> {
        // SAFETY: 无名称 auto-reset 事件；句柄由 `Shared` 独占并在最后一个引用释放时关闭。
        let work =
            unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
                .expect("work event");
        // SAFETY: 同上。
        let completion =
            unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
                .expect("completion event");
        Arc::new(Shared {
            state: Mutex::new(State::new()),
            work,
            completion,
        })
    }

    #[test]
    fn a_panicking_gpu_thread_publishes_an_error() {
        let shared = test_shared();
        let worker = Arc::clone(&shared);
        // 关掉默认 hook：这条 panic 是测试输入，不是测试失败。
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PanicGuard::new(worker);
            panic!("injected gpu thread panic");
        }));
        std::panic::set_hook(hook);
        assert!(panicked.is_err(), "the guard must run during unwinding");

        let error = shared.take_error().expect("panic must reach poll()");
        match error {
            EngineError::AppInit { detail } => {
                assert_eq!(detail, "kytrix-d3d12 thread panicked");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            !shared.shutdown_requested(),
            "a panic only terminates the thread, it does not fake a shutdown"
        );
    }

    #[test]
    fn a_normal_exit_does_not_overwrite_the_loop_error() {
        let shared = test_shared();
        shared.publish_error(EngineError::Swapchain {
            detail: SmolStr::new_static("ResizeBuffers failed"),
        });
        // 正常退出：guard 被 disarm，不发布任何东西。
        PanicGuard::new(Arc::clone(&shared)).disarm();
        match shared.take_error().expect("the loop error stays visible") {
            EngineError::Swapchain { detail } => {
                assert!(detail.contains("ResizeBuffers"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_panic_keeps_an_already_published_error() {
        let shared = test_shared();
        shared.publish_error(EngineError::DeviceLost {
            reason: SmolStr::new_static("DXGI_ERROR_DEVICE_REMOVED"),
            log_path: std::path::PathBuf::from("logs/dred.txt"),
        });
        shared.publish_panic();
        match shared.take_error().expect("first error wins") {
            EngineError::DeviceLost { reason, .. } => {
                assert_eq!(reason, "DXGI_ERROR_DEVICE_REMOVED");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn request_shutdown_signals_the_completion_wait_set() {
        // P1 回归：GPU 线程可能阻塞在 [waitable, completion] 上，关闭必须能通过 completion
        // 把它唤醒（`work` 不在那个等待集合里）。
        let shared = test_shared();
        shared.request_shutdown();
        // SAFETY: 句柄由 `shared` 持有；0 超时的查询无副作用。
        let wait =
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(shared.completion, 0) };
        assert_eq!(
            wait,
            windows::Win32::Foundation::WAIT_OBJECT_0,
            "request_shutdown must signal completion"
        );
    }

    #[test]
    fn a_background_change_is_taken_exactly_once() {
        let shared = test_shared();
        shared.set_background([0.25, 0.5, 0.75, 1.0]);
        assert_eq!(
            shared.take_background(),
            Some([0.25, 0.5, 0.75, 1.0]),
            "the GPU thread takes the pending background color once"
        );
        assert_eq!(shared.take_background(), None, "take clears the slot");
    }

    #[test]
    fn a_background_change_after_shutdown_or_termination_is_dropped() {
        let shared = test_shared();
        shared.request_shutdown();
        shared.set_background([1.0, 0.0, 0.0, 1.0]);
        assert_eq!(
            shared.take_background(),
            None,
            "the GPU loop exits before applying a pending background color"
        );

        let terminated = test_shared();
        terminated.mark_terminated();
        terminated.set_background([0.0, 1.0, 0.0, 1.0]);
        assert_eq!(terminated.take_background(), None);
    }
}
