//! 窗口渲染门面 + 每窗口一条 `kytrix-vulkan` GPU 线程
//!
//! ```text
//! UI 线程（calloop：wl_display 默认队列）      kytrix-vulkan 线程（每窗口）
//! ────────────────────────────────────────    ─────────────────────────────────────────
//! 输入 / configure / IME / 剪贴板 / frame 回调  循环：等 condvar → 取最新帧
//! publish(FrameRequest) ──► 最新帧槽                → 槽 fence → acquire（100ms 上限）
//! queue_atlas_upload ─────► 图集槽                  → 录制 → submit → present
//! poll() ◄──────────────── 反馈槽                    → 完成通知（timeline）→ retire
//! ```
//!
//! 三条硬约束：
//! - **I1**：UI 线程只做「加锁拷贝 + `notify`」，没有 acquire / fence wait / swapchain 重建 /
//!   `vkDeviceWaitIdle`。
//! - **I2**：`vkQueueSubmit` 后立即返回；完成状态只通过 fence / timeline 观察。
//! - **I3**：节奏来自 `vkAcquireNextImageKHR` + Wayland 合成器（`MAILBOX`），100 ms 超时只为响应
//!   shutdown；等待只发生在 GPU 线程。
//!
//! 关闭握手（§4.5 / §7.7）：`Drop` 里 signal shutdown 后**直接 `join`**（无超时）。有界性来自
//! `vkAcquireNextImageKHR` 的 100 ms 上限与槽 fence 的 2 s 有界等待（R11-5）：GPU 线程最多再等
//! 一个超时周期就会看到 shutdown。GPU 线程在退出前自己排空在飞帧：`GpuRenderer::drop` 一次**有界**
//! 的 timeline 等待（等待值 = 最后一次提交时 signal 的那个值，R11-1）加一次**有界**的队列 idle
//! （`SharedGpu::wait_queue_idle`，presentation engine 的同步，R11-3）；等待失败按设备丢失处理，
//! 不再假装成功继续销毁。**teardown 完成之后**才让 UI 观察到线程结束（`join` 返回即 teardown 完成）。
//!
//! panic 出路：GPU 线程展开时 [`PanicGuard`] 会向共享错误槽发布 `AppInit`，平台泵的 `poll()`
//! 因此看到错误而不是静默停摆（不覆盖循环已经发布的错误）。
//!
//! WSI 线程模型（§7.5）：`vkCreateWaylandSurfaceKHR` 创建的 WSI 私有 `wl_event_queue` 完全由
//! Mesa 内部 dispatch；应用只 dispatch 自己的默认队列（calloop 的 `WaylandSource`）。
//!
//! A4 呈现证据：GPU 线程在**每次 present 之前**用 `PresentationHandle` 创建
//! `wp_presentation_feedback`（帧序号写进 user data，见 `run_gpu_loop` 的调用点注释）；终态
//! 事件由 UI 线程的默认队列派发，经平台层换算后回填窗口呈现历史。见
//! `platform/linux/presentation.rs` 的模块文档（为什么必须在 GPU 线程、为什么不能放 UI 线程）。

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smol_str::{SmolStr, format_smolstr};
use wayland_client::protocol::wl_surface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "test-support")]
use crate::render::linux::readback::ReadbackImage;
#[cfg(feature = "hud")]
use crate::render::timer::Section;
use crate::{
    error::EngineError,
    paint::{
        atlas::{AtlasKind, MAX_IMAGE_PAGES},
        color::Color,
    },
    platform::linux::presentation::PresentationHandle,
    render::{
        DirtyRects, RenderStats,
        color::background_linear,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, FrameSink, LatestFrame, MAX_FRAMES_IN_FLIGHT},
        linux::{
            compositor::{GpuRenderer, MAX_RETRY_BACKOFF, Submitted},
            device::enter_gpu_thread,
        },
        timer::{FrameTimings, GpuTimerSample},
    },
    window::FrameIndex,
};

/// 有在飞帧时的完成观测周期：timeline 的完成不会唤醒 condvar，无限等待会让最后一帧的反馈
/// 永远不发布（§4.1 的完成语义）。
const FEEDBACK_POLL: Duration = Duration::from_millis(4);

/// 门面：窗口路径是「最新帧槽 + GPU 线程句柄」，离屏路径是同步渲染器。
///
/// 两个分支都是 **Linux 后端内部**的目标区分（离屏 / 窗口），不是跨平台抽象：
/// `render/linux.rs` 在编译期就选定了这一份实现（I14）。
pub(crate) struct Renderer {
    target: RenderTarget,
    /// 粘性错误：GPU 线程退出后 `poll` 持续返回同一错误（`publish` 变 no-op）。
    error: Option<StickyError>,
}

enum RenderTarget {
    /// 离屏 / 快照：同步渲染（`Window::enable_offscreen_renderer`，§4.5 的下半段）。
    Offscreen(GpuRenderer),
    /// 窗口：门面 + 每窗口一条 GPU 线程（§7.1）。
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

    /// 真实窗口：构造 GPU 对象（UI 线程）后整体移交给 `kytrix-vulkan` 线程。
    ///
    /// `background` 是窗口背景色（persist 的 clear color，linear）。`viewport` 是
    /// `wp_viewport`（`None` = 整数回退，见 V-110）：GPU 线程在 present 前用它写逻辑尺寸，
    /// destination 与 buffer 必须同一次生效（D1 / D2）。`surface` 是同一个 `wl_surface` 的代理
    /// 副本，GPU 线程在 present 前用它写整数回退的 `set_buffer_scale`。
    /// `presentation` 是 A4 的呈现反馈请求句柄（`None` = compositor 没有 `wp_presentation`）：
    /// GPU 线程在**每次 present 之前**用它创建 `wp_presentation_feedback`，帧序号直接写进
    /// user data（时序论证见 `platform/linux/presentation.rs` 的模块文档）。
    ///
    /// # Errors
    /// 设备 / swapchain 创建失败，或 GPU 线程无法启动（`AppInit`）。
    pub(crate) fn new_for_surface(
        display: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        size: (u32, u32),
        background: Color,
        viewport: Option<WpViewport>,
        surface: wl_surface::WlSurface,
        presentation: Option<PresentationHandle>,
    ) -> Result<Self, EngineError> {
        let gpu = GpuRenderer::new_for_surface(
            display,
            surface_ptr,
            size,
            background_linear(background),
            viewport,
            surface,
        )?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(gpu, size, presentation)?),
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

    /// 记录「UI 侧已经为这个目标尺寸发布过一帧」（B6-2 的合成线程路径）。
    ///
    /// 真实窗口的帧由合成线程直接投进最新帧槽，`publish` 不再经过 UI 线程 —— 但
    /// [`Renderer::surface`] 是 `commit_frame` 判定「本帧是否需要 GPU 工作」的唯一依据，
    /// 必须由 UI 侧在发布时同步。少这一次同步会让 `Plan::Idle` 的帧被反复发布（Windows 上每次
    /// 白等一个 vsync 的帧延迟 token）。
    pub(crate) fn note_published_surface(&mut self, surface: (u32, u32)) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.surface = surface;
        }
    }

    /// 合成线程的交付端点（B6-2）。
    ///
    /// 只有**窗口**目标有：它已经持有「最新帧槽 + 唤醒原语」的 `Arc<Shared>`（`Shared` 是
    /// `Send + Sync`），合成线程直接往那个槽里发布，**不经过 UI 线程**（少一次泵迭代的延迟）。
    /// 离屏目标是同步渲染，没有可跨线程的槽位 ⇒ `None`（走 inline 合成）。
    pub(crate) fn frame_sink(&self) -> Option<Arc<dyn FrameSink>> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(Arc::new(WindowFrameSink {
                shared: Arc::clone(&window.shared),
            })),
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

    /// 把一层的图集像素交给 GPU 线程（`pixels` 是 `rects` 顺序逐矩形紧密打包的字节）。
    ///
    /// `layer` 是图集 texture array 的层号（图集页索引）；`extent` 是该层的页边长。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        extent: u32,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => {
                gpu.queue_atlas_upload(kind, layer, extent, pixels.to_vec(), rects);
                true
            }
            RenderTarget::Window(window) => {
                window.queue_atlas_upload(kind, layer, extent, pixels, rects)
            }
        }
    }

    /// 该图集的共享槽是否空闲（UI 线程据此决定这一帧能否入队，DEVIATIONS F-211）。
    pub(crate) fn atlas_idle(&self, kind: AtlasKind) -> bool {
        match &self.target {
            // 离屏目标是同步渲染：上一笔上传还没被 `submit_snapshot` 消费时同样算忙。
            RenderTarget::Offscreen(gpu) => !gpu.atlas_pending(kind),
            RenderTarget::Window(window) => window.atlas_idle(kind),
        }
    }

    /// 改窗口背景色（UI 线程只写共享槽 + `wake`，clear color 由 GPU 线程应用）。
    ///
    /// 离屏目标没有背景色（从不 present），是 no-op。
    pub(crate) fn set_background_color(&mut self, color: Color) {
        match &self.target {
            RenderTarget::Offscreen(_) => {}
            RenderTarget::Window(window) => window.set_background_color(color),
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
/// 合成线程持有的交付端点：只包一层 `Arc<Shared>`（最新帧槽 + 唤醒原语）。
struct WindowFrameSink {
    shared: Arc<Shared>,
}

impl FrameSink for WindowFrameSink {
    fn submit(&self, request: FrameRequest) -> bool {
        if !self.shared.alive() {
            return false;
        }
        self.shared.publish_frame(request);
        true
    }

    fn is_alive(&self) -> bool {
        self.shared.alive()
    }
}

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
    fn new(
        gpu: GpuRenderer,
        size: (u32, u32),
        presentation: Option<PresentationHandle>,
    ) -> Result<Self, EngineError> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kytrix-vulkan".to_owned())
            .spawn(move || gpu_loop(gpu, worker, presentation))
            .map_err(|error| EngineError::AppInit {
                detail: format_smolstr!("spawn kytrix-vulkan thread: {error}"),
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

    fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        extent: u32,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        self.shared.queue_atlas(kind, layer, extent, pixels, rects)
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.shared.atlas_idle(kind)
    }

    /// persist clear color（linear）：交给 GPU 线程，在空闲或录制下一帧前应用。
    fn set_background_color(&self, color: Color) {
        self.shared.set_background(background_linear(color));
    }

    fn poll(&mut self) -> Result<Option<FrameFeedback>, EngineError> {
        if let Some(error) = self.shared.take_error() {
            return Err(error);
        }
        let Some(feedback) = self.shared.take_feedback() else {
            return Ok(None);
        };
        // 共享契约（`render/frame.rs`）：`presented_at` 要么是**可信**的上屏时刻（A4 数据面：
        // `wp_presentation_feedback.presented` 的呈现时钟），要么如实为 `None`；绝不允许用
        // 完成时刻顶替。Linux 的呈现事件在 **UI 线程**异步到达，由平台层
        // （`platform/linux/presentation.rs`）回填到窗口呈现历史，不经过本反馈槽
        // （`drain_completed` 恒发布 `None`，见那里的注释）。
        debug_assert!(
            feedback.presentation_is_consistent(),
            "presented_at must be a trustworthy on-screen instant or None"
        );
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
        // 反馈自己记录的完成延迟（Linux 的 `latency` 与 `completed_at` 各取一次 `now`）。
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
        // UI 线程唯一允许的等待点（§4.5 / §7.7）：`acquire` 的 100 ms 上限与槽 fence 的 2 s
        // 有界等待（R11-5）保证 GPU 线程在一个超时周期内看到 shutdown，随后 `GpuRenderer::drop`
        // 的 timeline 等待 + 队列 idle 结束在飞工作。`join` **没有超时**：放弃等待会让 GPU 线程
        // 在 UI 归还 `wl_surface` 之后仍触碰已释放资源（use-after-free）；也不再需要单独的退出
        // 信号 —— `join` 返回即 teardown 完成。
        // `Err` 只表示 GPU 线程 panic：`PanicGuard` 已把错误交给 `poll`，默认 hook 也已打印。
        let _ = thread.join();
    }
}

/// GPU 线程的 panic 出路：展开时把错误交给 UI（`poll()` 不会静默卡死），并标记线程已终止。
///
/// 正常退出时 [`PanicGuard::disarm`] 消费自己，**不覆盖**循环已经发布的错误（§7.7）。
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

/// UI 线程与 GPU 线程共享的最新帧 / 图集 / 反馈槽 + 唤醒原语（`condvar.notify`）。
struct Shared {
    state: Mutex<State>,
    work: Condvar,
    /// 每 `AtlasKind` 的图集上传槽位掩码（bit n = texture array 第 n 层正在被 GPU 线程消费；
    /// `0` = UI 可以入队）。图片图集一帧内会连续入队多个层，因此是位掩码而不是单一布尔。
    atlas_pending: [AtomicU32; 2],
}

struct State {
    frame: LatestFrame,
    atlas: AtlasSlots,
    feedback: Option<FrameFeedback>,
    error: Option<EngineError>,
    shutdown: bool,
    /// GPU 线程已经退出（`publish` 从此是 no-op）。
    terminated: bool,
    /// 待应用的 persist clear color（linear；最新者胜，GPU 线程空闲 / 取帧后都会取走）。
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

    /// GPU 线程的等待条件：关闭 / 有帧 / 有待应用背景色。
    ///
    /// 待应用背景色也是工作：`set_background` 用 condvar 唤醒，等待条件不看它，这次唤醒会
    /// 被当成伪唤醒直接睡回去，颜色要等下一帧才落地（改色不要求提交一帧）。
    fn has_work(&self) -> bool {
        self.shutdown || self.frame.has_pending() || self.background.is_some()
    }
}

/// 图集上传槽：字形 1 层 + 图片 `MAX_IMAGE_PAGES` 层，每层只留最新一份（§4.2）。
///
/// D-9-c：一个窗口一帧内会**连续**入队图片图集的多个层（逐层整页/增量），因此槽位必须按层
/// 分开；`take` 一次性把本帧全部待上传层交给 GPU 线程（同一提交内聚合录制）。
struct AtlasSlots {
    glyph: Option<AtlasUpload>,
    image: [Option<AtlasUpload>; MAX_IMAGE_PAGES as usize],
}

/// 一个待上传层：`pixels` 是 `rects` 顺序逐矩形紧密打包的字节（整页上传时就是整页）。
struct AtlasUpload {
    kind: AtlasKind,
    /// texture array 层号（字形恒 0；图片 = 图集页索引）。
    layer: u16,
    /// 该层的页边长（纹素；同一图集的所有层相同）。
    extent: u32,
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl AtlasSlots {
    const fn new() -> Self {
        Self {
            glyph: None,
            image: [const { None }; MAX_IMAGE_PAGES as usize],
        }
    }

    /// 入队一次上传。**层槽由 UI 线程的 `atlas_idle` 保证空闲**（DEVIATIONS F-211）：
    /// 槽里已有未消费的上传时是契约破坏（会丢行），release 也硬失败。
    fn queue(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        extent: u32,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        debug_assert!(
            u32::from(layer) < kind.layers(),
            "atlas layer {layer} out of range for {kind:?}"
        );
        let slot = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image[usize::from(layer)],
        };
        assert!(
            slot.is_none(),
            "atlas slot must be idle before queueing a new upload (kind {kind:?}, layer {layer})"
        );
        *slot = Some(AtlasUpload {
            kind,
            layer,
            extent,
            pixels,
            rects,
        });
    }

    /// 取走本帧全部待上传层（字形在前、图片按层号升序）。
    fn take(&mut self) -> Vec<AtlasUpload> {
        let mut uploads = Vec::with_capacity(1 + usize::from(MAX_IMAGE_PAGES));
        uploads.extend(self.glyph.take());
        uploads.extend(self.image.iter_mut().filter_map(Option::take));
        uploads
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // 锁只在极短的入队/出队区间持有；panic 恢复后状态仍可继续工作，不需要二次 panic。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 唤醒 GPU 线程（`publish` / 关闭之后）。只有一个等待者，`notify_all` 只为覆盖
    /// 「帧 + 关闭」同时到达的情形。
    fn wake(&self) {
        self.work.notify_all();
    }

    /// GPU 线程是否还能接收帧（关闭 / 终止之后为假）。
    fn alive(&self) -> bool {
        let state = self.lock();
        !state.shutdown && !state.terminated
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

    fn queue_atlas(
        &self,
        kind: AtlasKind,
        layer: u16,
        extent: u32,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        let index = kind.index() as usize;
        debug_assert!(
            u32::from(layer) < kind.layers(),
            "atlas layer {layer} out of range for {kind:?}"
        );
        let bit = 1u32 << u32::from(layer);
        // 像素在**锁外**复制：整页可达 16 MiB，不能让 GPU 线程的取帧锁等这次 memcpy。
        let pixels = pixels.to_vec();
        // 先占位再入队：GPU 线程 take 之后才释放，UI 线程据此不会覆盖未消费的上传
        // （DEVIATIONS F-211）。标志在**录制完成**后才清（`atlas_uploaded`），
        // 因此「空闲」= 上一笔已经进了命令列表，不只是「被 GPU 线程拿走」。
        self.atlas_pending[index].fetch_or(bit, Ordering::Release);
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            self.atlas_pending[index].fetch_and(!bit, Ordering::Release);
            return false;
        }
        // 不唤醒：图集上传统一在「录制下一帧」之前落地（§4.2），单独唤醒只会空转。
        state.atlas.queue(kind, layer, extent, pixels, rects);
        true
    }

    /// 排队一笔 persist clear color（最新者胜）：GPU 线程空闲或录制下一帧前应用。
    fn set_background(&self, linear: [f32; 4]) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.background = Some(linear);
        drop(state);
        self.wake();
    }

    /// 取走待应用的 clear color（取一次清空）。
    fn take_background(&self) -> Option<[f32; 4]> {
        self.lock().background.take()
    }

    /// 空闲：睡在 condvar 上直到有帧或关闭（0 自旋唤醒）。
    ///
    /// 有在飞帧（`wait_completion == true`）时用 [`FEEDBACK_POLL`] 的有限等待：timeline 的完成
    /// 不会唤醒 condvar，无限等待会让最后一帧的反馈永远不发布（§4.1 的完成语义）。
    ///
    /// 等待条件是 [`State::has_work`]（含待应用背景色）：`set_background` 的唤醒不能被当成
    /// 伪唤醒睡回去（改色不要求提交一帧）。
    fn wait_for_work(&self, wait_completion: bool) {
        let mut state = self.lock();
        if wait_completion {
            if state.has_work() {
                return;
            }
            let _ = self
                .work
                .wait_timeout(state, FEEDBACK_POLL)
                .unwrap_or_else(PoisonError::into_inner);
            return;
        }
        while !state.has_work() {
            state = self
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn take_frame(&self) -> Option<FrameRequest> {
        self.lock().frame.take()
    }

    fn take_atlas(&self) -> Vec<AtlasUpload> {
        // 这里**不**清 `atlas_pending`：GPU 线程只是把上传拿走，还没录进命令列表。
        // 清得太早，UI 会在本帧未录制（Retry / Idle）时入队下一笔，覆盖掉这一笔（丢行）。
        self.lock().atlas.take()
    }

    /// 本帧的图集上传已经录进命令列表：该 kind 的层槽位掩码清零。
    fn atlas_uploaded(&self, kind: AtlasKind) {
        self.atlas_pending[kind.index() as usize].store(0, Ordering::Release);
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.atlas_pending[kind.index() as usize].load(Ordering::Acquire) == 0
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
                detail: SmolStr::new_static("kytrix-vulkan thread panicked"),
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
        self.wake();
    }
}

/// 已提交、尚未被 timeline 确认完成的帧（提交顺序 = 反馈发布顺序，§4.1）。
struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    /// 提交时的 CPU 采样（Upload / Present）；GPU 采样在完成观测点追加。
    samples: Vec<GpuTimerSample>,
    /// HUD 的 `latency` 起点（`FrameRequest::published_at`）。
    published_at: Instant,
    /// 本帧 submit 时 signal 的 timeline 值（= ring 的提交序号，R11-1）。
    sequence: u64,
    /// ring 槽位：完成时从该槽的 timestamp 读回缓冲读 GPU 时间。
    slot: usize,
}

/// GPU 线程主循环（§7.3）。
fn gpu_loop(renderer: GpuRenderer, shared: Arc<Shared>, presentation: Option<PresentationHandle>) {
    enter_gpu_thread();
    // panic 出路：展开时把错误交给 UI（`poll` 不会静默停摆）。
    let guard = PanicGuard::new(Arc::clone(&shared));
    // `GpuRenderer` 必须是**局部** `Option`：循环结束后显式 drop（timeline 等待 + 队列 idle +
    // 销毁 swapchain / persist / 命令池），UI 的 `join` 返回时 teardown 一定已经完成（§7.7）。
    let mut renderer = Some(renderer);
    let mut submitted = VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT);
    if let Err(error) = run_gpu_loop(
        renderer
            .as_mut()
            .expect("renderer is present until teardown"),
        &shared,
        &mut submitted,
        presentation.as_ref(),
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
    presentation: Option<&PresentationHandle>,
) -> Result<(), EngineError> {
    // 已从最新帧槽取出、但还没产生 GPU 工作的帧。`NOT_READY` / `OUT_OF_DATE` 时**保留**它：
    // 丢掉会把它的 damage 一起丢掉（persist 里那块像素永远不会被补画，I5）。
    let mut pending: Option<FrameRequest> = None;
    // A4：已经请求过呈现反馈的帧序号。`Retry`（acquire 超时 / OUT_OF_DATE）会保留待画帧并
    // 重试同一序号 —— 反馈对象只在**第一次**尝试前请求一次，重试不得重复创建（否则一次
    // commit 会出多条反馈；邮箱按帧去重能兜住，但没必要多占服务端对象）。
    let mut feedback_requested: Option<FrameIndex> = None;
    // 已从共享槽取走、但还没录进命令列表的图集上传（按 `AtlasKind`）。`Retry` 帧不清共享槽
    // 标志，否则 UI 的下一笔会覆盖这一笔（DEVIATIONS F-211）。
    let mut atlas_awaiting = [false; 2];
    loop {
        // ① 新发布的帧并入待画帧：保留新 payload/surface，旧帧的 damage 合并进新 plan（§4.2）。
        if let Some(next) = shared.take_frame() {
            pending = Some(match pending.take() {
                None => next,
                Some(older) => {
                    let mut newer = next;
                    newer.plan = older.plan.merge(newer.plan);
                    newer
                }
            });
        }
        // ② 完成观测点（唯一）：按提交顺序发布反馈 + 推进 retire（旧 swapchain / 旧 persist 在
        // 这里真正释放）。timeline 只读不等待（§7.2 硬规则 2）。
        drain_completed(renderer, shared, submitted)?;
        if shared.shutdown_requested() {
            return Ok(());
        }
        // 空闲时也要应用待定背景色：改色不要求提交一帧（UI 只写共享槽 + 唤醒）。
        if let Some(linear) = shared.take_background() {
            renderer.set_clear_color(linear);
        }
        let Some(request) = pending.as_ref() else {
            // 有在飞帧时用有限等待定期回来观察完成（见 `Shared::wait_for_work`）。
            shared.wait_for_work(!submitted.is_empty());
            continue;
        };
        // 等帧期间到达的背景色：与帧一起落地（仍然是录制前，不引入新的 GPU 调用点）。
        if let Some(linear) = shared.take_background() {
            renderer.set_clear_color(linear);
        }
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2；像素按所有权移交给 GPU）。
        // D-9-c：一次 take 里可能有多个层（图片图集逐层上传），同一提交内聚合录制。
        for upload in shared.take_atlas() {
            atlas_awaiting[upload.kind.index() as usize] = true;
            renderer.queue_atlas_upload(
                upload.kind,
                upload.layer,
                upload.extent,
                upload.pixels,
                upload.rects,
            );
        }
        // §4.4：resize 不是独立命令，`FrameRequest.surface` 就是目标尺寸。
        if request.surface != renderer.surface() {
            renderer.resize(request.surface)?;
        }
        let mut frame = FramePlan::new(request.plan, renderer.is_fresh());
        // 没有像素要画（Idle 且目标未重建）⇒ 不产生 GPU 工作；该序号不会出现在完成通知里（§4.3）。
        if frame.is_idle() {
            pending = None;
            continue;
        }
        // A4：呈现反馈必须在 present（= `wl_surface.attach/commit`）**之前**请求 ——
        // `wp_presentation.feedback` 绑定的是该 surface 的**下一次** commit，而本线程是唯一
        // 的提交者（Vulkan WSI）。请求先入 libwayland 的发送缓冲，`submit_frame` 随后的
        // attach/commit 排在它后面（同一 `wl_display` 的发送缓冲由 libwayland 互斥锁保序），
        // 帧序号随 user data 过线，终态事件到达时直接知道是哪一帧。
        //
        // 只按帧序号请求一次：`Retry`（acquire 超时 / OUT_OF_DATE）会保留待画帧并重试同一
        // 序号，重试不得重复创建对象（一次 commit 会出多条反馈）。
        if let Some(handle) = presentation
            && feedback_requested != Some(request.index)
        {
            handle.request_feedback(request.index);
            feedback_requested = Some(request.index);
        }
        match renderer.submit_frame(request, &mut frame)? {
            Submitted::Presented {
                stats,
                samples,
                slot,
                sequence,
            } => {
                // 上传已经录进本帧命令列表：共享槽这才算空，UI 可以入队下一笔。
                for (index, awaiting) in atlas_awaiting.iter_mut().enumerate() {
                    if std::mem::take(awaiting) {
                        shared.atlas_uploaded(if index == 0 {
                            AtlasKind::Glyph
                        } else {
                            AtlasKind::Image
                        });
                    }
                }
                submitted.push_back(SubmittedFrame {
                    index: request.index,
                    stats,
                    samples,
                    published_at: request.published_at,
                    sequence,
                    slot,
                });
                pending = None;
            }
            // 保留待画帧：可恢复的 WSI 故障（C8）按 `after` 有界退避后重试；`None` 表示立即
            // 重试（acquire 已自带 100 ms 节拍）。`sleep` 前后都检查 shutdown ⇒ 关窗 `join()`
            // 的尾延迟 ≤ `MAX_RETRY_BACKOFF`（50 ms，小于 acquire 的 100 ms 上界）。
            Submitted::Retry { after } => {
                if let Some(after) = after {
                    debug_assert!(
                        after <= MAX_RETRY_BACKOFF,
                        "swapchain retry backoff must stay bounded (got {after:?})"
                    );
                    if shared.shutdown_requested() {
                        return Ok(());
                    }
                    thread::sleep(after.min(MAX_RETRY_BACKOFF));
                    if shared.shutdown_requested() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// 发布已观察到完成的帧（提交顺序）并推进 retire —— 与 Windows 同一观测点（§4.1）。
fn drain_completed(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
) -> Result<(), EngineError> {
    let completed = renderer.drain_completed()?;
    for frame in completed_frames(submitted, completed) {
        let mut samples = frame.samples;
        // GPU 采样必须在 timeline 确认完成之后 resolve（不读在飞槽的 timestamp）。
        if let Some(sample) = renderer.resolve_frame_timer(frame.slot) {
            samples.push(sample);
        }
        // 观察到完成的时刻（timeline 非阻塞查询），不是 `Present` 返回的时刻。
        let completed_at = Instant::now();
        // A2：合成线程不再有进程级帧时钟；Wayland 的 present 节奏由 `wl_surface.frame` 回调
        // （平台层的 `FrameGate`）承担，这里不再向任何帧时钟发布节拍。
        shared.publish_feedback(FrameFeedback {
            index: frame.index,
            stats: frame.stats,
            samples,
            completed_at,
            // UI 发布 → GPU 完成（HUD 的 publish→present）。
            latency: completed_at.saturating_duration_since(frame.published_at),
            // A4：Linux 的 presented_at 来源是 `wp_presentation_feedback.presented`（呈现时钟
            // 时间戳）。事件在 **UI 线程**派发、比本完成反馈更晚到达，由平台层
            // （`platform/linux/presentation.rs` 绑定与换算 → `surface.rs` 的 Dispatch →
            // 邮箱 → 泵回填 `Window::note_frame_presented`）异步写入窗口呈现历史。GPU 线程的
            // 完成反馈槽**恒为 `None`**：不拿 GPU 完成时刻顶替（那会让 HUD / e2e 的上屏延迟
            // 变成假数据）。
            presented_at: None,
        });
    }
    Ok(())
}

/// 按提交顺序取出 timeline 已确认完成的帧。
///
/// 完成判据是本帧 submit 时 signal 的 timeline 值（`SubmittedFrame.sequence`，R11-1），
/// **不是**任何"重新推导出来的序号"：槽位按 acquire 次数轮转、UI 帧序号会跳号（最新帧槽合并、
/// 空闲 / 门禁跳过的帧不产生 GPU 工作），只有 ring 的提交序号与 signal 值严格同源。用别的计数
/// 去比，序号一跳号就会把仍在飞的帧误判为完成（completed_at / latency 记到错帧，并在槽 fence
/// 之前 resolve 在飞 query）。
fn completed_frames<'a>(
    submitted: &'a mut VecDeque<SubmittedFrame>,
    completed: u64,
) -> impl Iterator<Item = SubmittedFrame> + 'a {
    std::iter::from_fn(move || {
        if submitted
            .front()
            .is_some_and(|frame| frame.sequence <= completed)
        {
            submitted.pop_front()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Condvar, Mutex, atomic::AtomicU32},
        time::Instant,
    };

    use smol_str::SmolStr;

    use super::{
        AtlasSlots, PanicGuard, Shared, State, StickyError, SubmittedFrame, completed_frames,
    };
    use crate::{
        error::EngineError,
        paint::atlas::AtlasKind,
        render::{DirtyRects, RenderStats, compositor::Plan, frame::LatestFrame},
        window::FrameIndex,
    };

    fn dirty_rects(y0: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(crate::math::PhysicalRect::new(
            0,
            y0 as i32,
            i32::MAX,
            y1 as i32,
        ));
        rects
    }

    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        })
    }

    #[test]
    fn a_first_atlas_upload_keeps_its_dirty_rects() {
        let mut slots = AtlasSlots::new();
        slots.queue(AtlasKind::Glyph, 0, 64, vec![1, 2, 3], dirty_rects(4, 8));
        slots.queue(AtlasKind::Image, 1, 2048, vec![4, 5], dirty_rects(0, 2));
        let mut uploads = slots.take();
        assert_eq!(uploads.len(), 2, "字形 + 图片第 1 层");
        let image = uploads.pop().expect("image upload");
        assert_eq!(image.kind, AtlasKind::Image);
        assert_eq!(image.layer, 1);
        assert_eq!(image.extent, 2048);
        assert_eq!(image.pixels, vec![4, 5]);
        assert_eq!(image.rects, dirty_rects(0, 2));
        let glyph = uploads.pop().expect("glyph upload");
        assert_eq!(glyph.kind, AtlasKind::Glyph);
        assert_eq!(glyph.layer, 0);
        assert_eq!(glyph.extent, 64);
        assert_eq!(glyph.pixels, vec![1, 2, 3]);
        assert_eq!(glyph.rects, dirty_rects(4, 8));
        assert!(slots.take().is_empty(), "取走后槽位为空");
    }

    /// 同一个 kind 的不同层可以**同时**入队（D-9-c：图片图集一帧内逐层上传）。
    #[test]
    fn multiple_image_layers_can_be_queued_in_one_frame() {
        let mut slots = AtlasSlots::new();
        for layer in 0..4u16 {
            slots.queue(
                AtlasKind::Image,
                layer,
                2048,
                vec![layer as u8],
                dirty_rects(0, 1),
            );
        }
        let uploads = slots.take();
        assert_eq!(uploads.len(), 4);
        assert_eq!(
            uploads
                .iter()
                .map(|upload| upload.layer)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "take 必须按层号升序返回"
        );
    }

    #[test]
    #[should_panic(expected = "atlas slot must be idle")]
    fn queueing_into_a_busy_atlas_slot_is_a_contract_violation() {
        // UI 线程用 `atlas_idle` 门控入队；覆盖未消费的上传会丢行，因此这里是硬失败。
        let mut slots = AtlasSlots::new();
        slots.queue(AtlasKind::Glyph, 0, 64, vec![1, 2, 3], dirty_rects(4, 8));
        slots.queue(AtlasKind::Glyph, 0, 64, vec![9, 9, 9], dirty_rects(16, 20));
    }

    /// `atlas_pending` 只能在**录制完成**后清：`take_atlas` 只把上传搬给 GPU 线程，清早了
    /// UI 会在本帧未录制时入队下一笔并覆盖它（DEVIATIONS F-211 的丢行回归面）。
    #[test]
    fn an_atlas_upload_stays_busy_until_it_is_recorded() {
        let shared = test_shared();
        assert!(shared.atlas_idle(AtlasKind::Glyph));
        assert!(shared.queue_atlas(AtlasKind::Glyph, 0, 64, &[1, 2, 3], dirty_rects(4, 8)));

        let uploads = shared.take_atlas();
        assert_eq!(uploads.len(), 1);
        assert!(
            !shared.atlas_idle(AtlasKind::Glyph),
            "take 只是搬走上传，录制之前共享槽仍然是忙的"
        );

        shared.atlas_uploaded(AtlasKind::Glyph);
        assert!(
            shared.atlas_idle(AtlasKind::Glyph),
            "录制完成之后才允许下一次入队"
        );
    }

    /// D-9-c：图片图集逐层入队 ⇒ 任一层的槽位未录制完，该 kind 都算忙；
    /// 一次 `atlas_uploaded` 清空该 kind 的全部层（同一次提交内一起录制）。
    #[test]
    fn image_atlas_is_busy_until_every_queued_layer_is_recorded() {
        let shared = test_shared();
        assert!(shared.atlas_idle(AtlasKind::Image));
        for layer in 0..4u16 {
            assert!(shared.queue_atlas(
                AtlasKind::Image,
                layer,
                2048,
                &[layer as u8; 4],
                dirty_rects(0, 1)
            ));
        }
        assert!(!shared.atlas_idle(AtlasKind::Image));
        assert_eq!(shared.take_atlas().len(), 4);
        assert!(!shared.atlas_idle(AtlasKind::Image));
        shared.atlas_uploaded(AtlasKind::Image);
        assert!(shared.atlas_idle(AtlasKind::Image));
    }

    #[test]
    fn a_background_change_is_taken_exactly_once() {
        let shared = Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        };
        assert!(!shared.lock().has_work(), "空槽不是 GPU 线程的工作");

        shared.set_background([0.25, 0.5, 0.75, 1.0]);
        // 等待条件必须把背景色当工作，否则 `set_background` 的唤醒会被当成伪唤醒睡回去。
        assert!(shared.lock().has_work(), "待应用背景色必须唤醒 GPU 线程");
        assert_eq!(
            shared.take_background(),
            Some([0.25, 0.5, 0.75, 1.0]),
            "the GPU thread takes the pending background color once"
        );
        assert_eq!(shared.take_background(), None, "take clears the slot");
        assert!(!shared.lock().has_work(), "取走之后不再是工作");
    }

    #[test]
    fn a_background_change_after_shutdown_or_termination_is_dropped() {
        let shared = Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        };
        shared.request_shutdown();
        shared.set_background([1.0, 0.0, 0.0, 1.0]);
        assert_eq!(
            shared.take_background(),
            None,
            "the GPU loop exits before applying a pending background color"
        );

        let terminated = Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        };
        terminated.mark_terminated();
        terminated.set_background([0.0, 1.0, 0.0, 1.0]);
        assert_eq!(terminated.take_background(), None);
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
            surface_local: crate::math::Size::new(1.0, 1.0),
            buffer_scale: 1,
            published_at: std::time::Instant::now(),
        };
        frame.publish(request(1, Plan::Full));
        frame.publish(request(2, Plan::Idle));
        assert_eq!(frame.take().expect("frame").plan, Plan::Full);
    }

    #[test]
    fn only_frames_whose_timeline_value_completed_are_published() {
        // 完成判据只能比 `SubmittedFrame.sequence`（提交时 signal 的 timeline 值，R11-1）。
        // 这里故意用**带空洞**的序号（1/4/7）覆盖防御性场景：观测到 4 时只发布 1 与 4，
        // 7 仍在飞。用「条数 / 位置 / UI 帧序号」去数都会把 7 也弹出来（把 completed_at 记到
        // 错帧，并在槽 fence 之前 resolve 在飞 query）。
        let mut submitted = std::collections::VecDeque::new();
        for index in [1u32, 4, 7] {
            submitted.push_back(SubmittedFrame {
                index: FrameIndex::from_raw(index),
                stats: RenderStats::default(),
                samples: Vec::new(),
                published_at: Instant::now(),
                sequence: u64::from(index),
                slot: 0,
            });
        }
        let done: Vec<FrameIndex> = completed_frames(&mut submitted, 4)
            .map(|frame| frame.index)
            .collect();
        assert_eq!(
            done,
            vec![FrameIndex::from_raw(1), FrameIndex::from_raw(4)],
            "timeline 4 只覆盖 1 和 4"
        );
        assert_eq!(
            submitted.front().map(|frame| frame.index),
            Some(FrameIndex::from_raw(7)),
            "timeline 4 不覆盖 7：仍在飞的帧不得发布"
        );
        assert_eq!(submitted.len(), 1, "只弹出已完成的帧");
    }

    #[test]
    fn sticky_errors_rebuild_the_same_variant() {
        let error = EngineError::DeviceLost {
            reason: SmolStr::new_static("VK_ERROR_DEVICE_LOST"),
            log_path: std::path::PathBuf::from("logs/vk.txt"),
        };
        let sticky = StickyError::capture(error);
        match sticky.to_error() {
            EngineError::DeviceLost { reason, log_path } => {
                assert_eq!(reason, "VK_ERROR_DEVICE_LOST");
                assert_eq!(log_path, std::path::PathBuf::from("logs/vk.txt"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let sticky = StickyError::capture(EngineError::Swapchain {
            detail: SmolStr::new_static("create_swapchain_khr: ERROR_OUT_OF_DATE_KHR"),
        });
        let text = sticky.to_error().to_string();
        assert!(text.contains("create_swapchain_khr"), "{text}");
    }

    #[test]
    fn a_panicking_gpu_thread_publishes_an_error() {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        });
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
                assert_eq!(detail, "kytrix-vulkan thread panicked");
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
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        });
        shared.publish_error(EngineError::Swapchain {
            detail: SmolStr::new_static("recreate_swapchain_khr failed"),
        });
        // 正常退出：guard 被 disarm，不发布任何东西。
        PanicGuard::new(Arc::clone(&shared)).disarm();
        match shared.take_error().expect("the loop error stays visible") {
            EngineError::Swapchain { detail } => {
                assert!(detail.contains("recreate_swapchain_khr"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_panic_keeps_an_already_published_error() {
        let shared = Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        };
        shared.publish_error(EngineError::DeviceLost {
            reason: SmolStr::new_static("VK_ERROR_DEVICE_LOST"),
            log_path: std::path::PathBuf::from("logs/vk.txt"),
        });
        shared.publish_panic();
        match shared.take_error().expect("first error wins") {
            EngineError::DeviceLost { reason, .. } => {
                assert_eq!(reason, "VK_ERROR_DEVICE_LOST");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
