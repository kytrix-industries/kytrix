//! 窗口渲染门面 + `CAMetalDisplayLink` + 每窗口一条 `kytrix-metal` GPU 线程
//! （`design/async-render-pipeline.md` §4.5 / §5）。
//!
//! ```text
//! 主线程                                   kytrix-metal 线程（每窗口）
//! ────────────────────────────────────    ──────────────────────────────────────────
//! CAMetalLayer（NSView 持有）              循环：等 condvar
//! CAMetalDisplayLink ── 每 vsync ──┐        → 取完成通知（GPU 计时 / retire / 反馈）
//!   delegate: 存 update.drawable   │        → 取最新 drawable + 最新帧
//!             唤醒 GPU 线程        │        → 3 槽 semaphore（非阻塞，失败即跳帧）
//! 写图层属性（drawableSize/scale） ┘        → 重建 persist → 录制 → presentDrawable
//! sync_pacing(continuous)                  → addCompletedHandler → commit（立即返回）
//! ```
//!
//! 三条硬约束：
//! - **I1**：主线程只做「加锁拷贝 / 写图层属性 / `setPaused`」，没有 `nextDrawable`、
//!   `waitUntilCompleted`、`status()` 轮询、semaphore 等待、persist 重建。
//! - **I2**：`commit` 后立即返回；完成只通过 `addCompletedHandler`。
//! - **I3**：节奏来自 `CAMetalDisplayLink`（主 run loop 的 **common modes**）；空闲 `isPaused = true`，
//!   有帧请求 / 在飞帧 / 连续帧时恢复。
//!
//! 关闭握手（§4.5）：`Drop` 里停 display link → signal shutdown → 有界等待 GPU 线程（2 s）；
//! GPU 线程在退出前自行 drain 在飞帧（完成通知到达后才释放旧资源，I4）。

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, runtime::ProtocolObject,
};
use objc2_foundation::{NSObject, NSRunLoop, NSRunLoopCommonModes};
use objc2_metal::{MTLCommandBuffer, MTLTexture};
use objc2_quartz_core::{
    CAFrameRateRange, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate,
    CAMetalDrawable, CAMetalLayer,
};
use smol_str::{SmolStr, format_smolstr};

#[cfg(any(test, feature = "test-support"))]
use crate::render::macos::readback::ReadbackImage;
#[cfg(feature = "hud")]
use crate::render::timer::Section;
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        DirtyRows, RenderStats,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, LatestFrame},
        macos::{
            compositor::GpuRenderer,
            device::{Device, enter_gpu_thread},
            swapchain,
        },
        timer::{FrameTimings, GpuTimerSample},
    },
    window::FrameIndex,
};

/// 关闭握手的等待上限（UI 线程**唯一**允许等 GPU 线程的时刻，§4.5）。
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 门面：窗口路径是「最新帧槽 + 最新 drawable 槽 + GPU 线程」，离屏路径是同步渲染器。
///
/// 两个分支都是 **macOS 后端内部**的目标区分（离屏 / 窗口），不是跨平台抽象：
/// `render/macos.rs` 在编译期就选定了这一份实现（I14）。
pub(crate) struct Renderer {
    target: RenderTarget,
    /// 粘性错误：GPU 线程退出后 `poll` 持续返回同一错误（`publish` 变 no-op）。
    error: Option<StickyError>,
}

enum RenderTarget {
    /// 离屏 / 快照：同步渲染（`Window::enable_offscreen_renderer`，§4.5 的下半段）。
    Offscreen(GpuRenderer),
    /// 窗口：门面 + display link + 每窗口一条 GPU 线程（§5.1）。
    Window(WindowRenderer),
}

impl Renderer {
    /// 产品路径（离屏 / 快照）：同步渲染器，不建线程、不建 display link。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_offscreen(size)?),
            error: None,
        })
    }

    /// 指定图集页（快照必须与 `App` 的分配器同页尺寸）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_offscreen_with_page(
        size: (u32, u32),
        atlas_page: u32,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_offscreen_with_page(
                size, atlas_page,
            )?),
            error: None,
        })
    }

    /// 真实窗口：主线程建图层（GPU 侧对象随后整体移交），建 `CAMetalDisplayLink` 并挂到
    /// 主 run loop 的 common modes。
    ///
    /// # Errors
    /// 设备 / 管线创建失败、`CAMetalDisplayLink` 无法创建（`AppInit`），或 GPU 线程无法启动。
    pub(crate) fn new_for_layer(
        size: (u32, u32),
        scale: f64,
        max_frame_rate: f32,
    ) -> Result<Self, EngineError> {
        let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
            detail: SmolStr::new_static("CAMetalDisplayLink requires the main thread"),
        })?;
        let device = Device::new()?;
        let layer = swapchain::new_layer(&device, size, scale)?;
        let gpu = GpuRenderer::new_window(device, size)?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(
                mtm,
                gpu,
                layer,
                size,
                max_frame_rate,
            )?),
            error: None,
        })
    }

    /// 绘图层（只给 `NSView::setLayer` 用）。
    pub(crate) fn layer_handle(&self) -> Option<Retained<CAMetalLayer>> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(window.layer.clone()),
        }
    }

    /// 最近一次**发布**的目标尺寸（UI 侧记录）。
    pub(crate) fn surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.surface,
        }
    }

    /// 发布一帧（**不阻塞**）：窗口路径合并进最新帧槽并唤醒 GPU 线程 / 恢复 display link；
    /// 离屏路径同步渲染。
    ///
    /// 返回最近一次**已完成**帧的统计（窗口路径落后 ≤1 帧，HUD 允许）。
    pub(crate) fn publish(&mut self, request: FrameRequest) -> RenderStats {
        if self.error.is_some() {
            return RenderStats::default();
        }
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => {
                if gpu.surface() != request.surface
                    && let Err(error) = gpu.prepare_target(request.surface)
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

    /// 图层属性（`contentsScale` / `drawableSize`）的**唯一**写入口：只有主线程可调用（§5.3）。
    pub(crate) fn set_layer_scale(&mut self, surface: (u32, u32), scale: f32) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_layer_scale(surface, scale);
        }
    }

    /// display link 的暂停/恢复（主线程；平台泵每轮调用一次，§5.3）。
    pub(crate) fn sync_pacing(&mut self, continuous: bool) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.sync_pacing(continuous);
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
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &crate::paint::instance::InstanceBuffer,
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

/// `CAMetalDisplayLinkDelegate` 的实例变量（主线程独占）。
struct DisplayLinkIvars {
    shared: Arc<Shared>,
}

define_class!(
    /// display link 的委托：**只做两件事** —— 把 update 里的 drawable 放进最新 drawable 槽、
    /// 唤醒 GPU 线程。**不在回调里编码**（§5.1 / §5.3）。
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixMetalDisplayLink"]
    #[ivars = DisplayLinkIvars]
    struct DisplayLinkDelegate;

    impl DisplayLinkDelegate {}

    // SAFETY: `CAMetalDisplayLinkDelegate` 的回调由系统在主线程、display link 所在的 run loop 上
    // 调用；实现里只把 drawable 交给共享槽并唤醒 GPU 线程（都在锁内完成，没有 GPU 编码）。
    unsafe impl CAMetalDisplayLinkDelegate for DisplayLinkDelegate {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn metal_display_link_needs_update(
            &self,
            _link: &CAMetalDisplayLink,
            update: &CAMetalDisplayLinkUpdate,
        ) {
            self.ivars().shared.publish_drawable(update.drawable());
        }
    }
);

impl DisplayLinkDelegate {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DisplayLinkIvars { shared });
        // SAFETY: `init` 是 `NSObject` 的指定初始化器；`this` 由 `alloc` 产生且已装入 ivars。
        unsafe { msg_send![super(this), init] }
    }
}

/// 窗口渲染器的主线程句柄（最新帧槽 + 最新 drawable 槽 + 反馈槽 + display link + GPU 线程句柄）。
struct WindowRenderer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// GPU 线程退出信号（`ExitSignal` 在退出 / panic 时发送）。
    done: Receiver<()>,
    layer: Retained<CAMetalLayer>,
    display_link: Retained<CAMetalDisplayLink>,
    /// `delegate` 是 **weak** 属性（objc2 与 Apple 文档都如此）：必须由门面持有强引用，
    /// 否则回调会打在已释放的对象上（§5.1）。
    _delegate: Retained<DisplayLinkDelegate>,
    /// 最近一次**发布**的目标尺寸（`publish` 里更新）。
    surface: (u32, u32),
    /// 最近一次写进图层的物理尺寸 / DPI（避免每帧重复写 CA 属性，§5.3）。
    layer_size: (u32, u32),
    layer_scale: f64,
    stats: RenderStats,
    #[cfg(feature = "hud")]
    samples: Vec<GpuTimerSample>,
    /// 最近一次「发布」的帧序号与时刻（反馈顺序断言：完成时刻不可能早于它自己的发布）。
    last_published: Option<(FrameIndex, Instant)>,
}

impl WindowRenderer {
    fn new(
        mtm: MainThreadMarker,
        gpu: GpuRenderer,
        layer: Retained<CAMetalLayer>,
        size: (u32, u32),
        max_frame_rate: f32,
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let shared = Arc::new(Shared::new());
        let delegate = DisplayLinkDelegate::new(mtm, Arc::clone(&shared));
        let display_link =
            CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
        display_link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // 30…刷新率：系统可能因低电量 / 散热下调，按系统给的来（§5.3）。
        let max_frame_rate = max_frame_rate.max(30.0);
        display_link.setPreferredFrameRateRange(CAFrameRateRange::new(
            30.0,
            max_frame_rate,
            max_frame_rate,
        ));
        // 没有帧要画就没有 vsync：初始空闲（`publish` / `sync_pacing` 负责恢复）。
        display_link.setPaused(true);
        // SAFETY: 主线程、主 run loop；common modes 让 live resize（`NSEventTrackingRunLoopMode`）
        // 与全屏转场期间仍然回调（§5.3，行为由 §11 探针 1 实测）。
        unsafe {
            display_link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
        }

        let (done, done_rx) = mpsc::channel();
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kytrix-metal".to_owned())
            .spawn(move || gpu_loop(gpu, worker, done))
            .map_err(|error| EngineError::AppInit {
                detail: format_smolstr!("spawn kytrix-metal thread: {error}"),
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            done: done_rx,
            layer,
            display_link,
            _delegate: delegate,
            surface: size,
            layer_size: size,
            layer_scale: 0.0,
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
        // 有帧要画就必须有 vsync（§5.3）；`sync_pacing` 每轮还会再校一次。
        if self.display_link.isPaused() {
            self.display_link.setPaused(false);
        }
        self.stats
    }

    fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        self.shared.queue_atlas(kind, pixels, rows);
    }

    fn set_layer_scale(&mut self, surface: (u32, u32), scale: f32) {
        let size = (surface.0.max(1), surface.1.max(1));
        let scale = f64::from(scale);
        if size == self.layer_size && (scale - self.layer_scale).abs() <= f64::EPSILON {
            return;
        }
        swapchain::set_layer_size(&self.layer, size, scale);
        self.layer_size = size;
        self.layer_scale = scale;
    }

    fn sync_pacing(&mut self, continuous: bool) {
        let (pending, in_flight) = self.shared.pacing_state();
        let paused = pacing_paused(continuous, pending, in_flight);
        if self.display_link.isPaused() != paused {
            self.display_link.setPaused(paused);
        }
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
        // `latency` 的定义是 `completed_at - published_at`（macOS 用同一个 `completed_at` 计算）。
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
        // 先停节奏源：之后不会再有 drawable 回调（display link 只能在主线程操作）。
        self.display_link.setDelegate(None);
        // SAFETY: 与挂载同一对主线程 / 主 run loop / common modes 参数。
        unsafe {
            self.display_link
                .removeFromRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
        }
        self.display_link.invalidate();
        self.shared.request_shutdown();
        let Some(thread) = self.thread.take() else {
            return;
        };
        // UI 线程唯一允许的等待点：signal shutdown 之后有界等待 GPU 线程退出（§4.5）。
        match self.done.recv_timeout(JOIN_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                // 放弃 join（线程可能卡在驱动的等待里）：detach，避免把 UI 线程一起拖死。
                eprintln!(
                    "kytrix: the kytrix-metal thread did not exit within {}s; abandoning the join",
                    JOIN_TIMEOUT.as_secs()
                );
            }
        }
    }
}

/// GPU 线程退出（含 panic）时通知 UI 线程，保证 `Drop` 的等待有界。
struct ExitSignal(Sender<()>);

impl Drop for ExitSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// 命令缓冲跨线程移交（Metal 完成回调 → GPU 线程 drain）。
struct SendCommandBuffer(Retained<ProtocolObject<dyn MTLCommandBuffer>>);

// SAFETY: Metal 对象本身线程安全（`MTLCommandBuffer` 的完成回调可能在任何线程执行）；完成回调
// 交出引用后，只有 GPU 线程持有它。
unsafe impl Send for SendCommandBuffer {}

/// drawable 跨线程移交（主线程 display link delegate → GPU 线程）。
struct SendDrawable(Retained<ProtocolObject<dyn CAMetalDrawable>>);

// SAFETY: Metal 对象本身线程安全；放进最新 drawable 槽之后只有 GPU 线程持有引用。
unsafe impl Send for SendDrawable {}

/// UI 线程与 GPU 线程共享的最新帧 / 图集 / 最新 drawable / 完成队列 / 反馈槽 + 唤醒原语。
struct Shared {
    state: Mutex<State>,
    work: Condvar,
}

struct State {
    frame: LatestFrame,
    atlas: AtlasSlots,
    /// 最新 drawable（display link 每 vsync 覆盖旧的；GPU 线程取走即清空）。
    drawable: Option<SendDrawable>,
    /// 已完成、等 GPU 线程 drain 的命令缓冲（完成回调入队，本队列只增不减）。
    completions: VecDeque<Completion>,
    /// 已提交、尚未 drain 的帧数（`sync_pacing` 与关闭握手都读它）。
    in_flight: usize,
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
            drawable: None,
            completions: VecDeque::new(),
            in_flight: 0,
            feedback: None,
            error: None,
            shutdown: false,
            terminated: false,
        }
    }

    /// GPU 线程的等待条件：有完成通知 / 有帧且有 drawable / 关闭且已无在飞帧。
    fn has_work(&self) -> bool {
        (self.shutdown && self.in_flight == 0)
            || !self.completions.is_empty()
            || (self.frame.has_pending() && self.drawable.is_some())
    }
}

struct Completion {
    index: FrameIndex,
    buffer: SendCommandBuffer,
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
                // 复用已分配的容量，不重新分配。
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

/// display link 的暂停状态（§5.3 的状态机；纯函数便于单测）。
///
/// 需要跑：`continuous || 有未取走的帧请求 || 有在飞帧` ⇒ 不暂停；否则暂停（空闲 0 唤醒）。
fn pacing_paused(continuous: bool, pending_frame: bool, in_flight: usize) -> bool {
    !(continuous || pending_frame || in_flight > 0)
}

impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // 锁只在极短的入队/出队区间持有；panic 恢复后状态仍可继续工作，不需要二次 panic。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 唤醒 GPU 线程（display link 回调 / `publish` / 完成通知 / 关闭）。
    fn wake(&self) {
        self.work.notify_all();
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

    /// display link delegate（主线程）：把这一帧的 drawable 放进最新 drawable 槽并唤醒 GPU 线程。
    fn publish_drawable(&self, drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.drawable = Some(SendDrawable(drawable));
        drop(state);
        self.wake();
    }

    /// 完成回调（Metal 的任意线程）：入队 + 唤醒 GPU 线程（**不**碰 retire / 计时）。
    fn push_completion(&self, index: FrameIndex, buffer: SendCommandBuffer) {
        let mut state = self.lock();
        if state.shutdown && state.terminated {
            return;
        }
        state.completions.push_back(Completion { index, buffer });
        drop(state);
        self.wake();
    }

    /// 空闲：睡在 condvar 上直到「有完成通知 / 有帧且有 drawable / 关闭且无在飞帧」。
    fn wait_for_work(&self) {
        let mut state = self.lock();
        while !state.has_work() {
            state = self
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn has_pending_frame(&self) -> bool {
        self.lock().frame.has_pending()
    }

    fn take_frame(&self) -> Option<FrameRequest> {
        self.lock().frame.take()
    }

    fn take_drawable(&self) -> Option<SendDrawable> {
        self.lock().drawable.take()
    }

    fn take_atlas(&self) -> [Option<AtlasUpload>; 2] {
        self.lock().atlas.take()
    }

    /// 取走全部完成通知，并把在飞帧数减去同样多（GPU 线程 drain）。
    fn take_completions(&self) -> Vec<Completion> {
        let mut state = self.lock();
        let completions: Vec<Completion> = state.completions.drain(..).collect();
        state.in_flight = state.in_flight.saturating_sub(completions.len());
        completions
    }

    /// GPU 线程提交成功之后记账（`sync_pacing` 与关闭握手都读它）。
    fn note_submitted(&self) {
        self.lock().in_flight += 1;
    }

    fn in_flight(&self) -> usize {
        self.lock().in_flight
    }

    /// `sync_pacing` 的输入：是否有未取走的帧请求、是否有在飞帧。
    fn pacing_state(&self) -> (bool, usize) {
        let state = self.lock();
        (state.frame.has_pending(), state.in_flight)
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

/// GPU 线程主循环（§5.2）。
fn gpu_loop(mut renderer: GpuRenderer, shared: Arc<Shared>, done: Sender<()>) {
    enter_gpu_thread();
    // 退出（含 panic）时通知 UI 线程；`Drop` 的 `recv_timeout` 因此有界。
    let _exit = ExitSignal(done);
    // 已提交、等完成通知的帧的统计与发布时刻（完成时拼成 `FrameFeedback`；落后 ≤1 帧）。
    // 元组末项是 `FrameRequest::published_at`：macOS 的反馈在**完成通知**里发布，
    // 那时 `request` 已经不在作用域内，所以发布基准要随提交一起记账。
    let mut submitted: VecDeque<(FrameIndex, RenderStats, Vec<GpuTimerSample>, Instant)> =
        VecDeque::new();
    loop {
        shared.wait_for_work();
        if let Err(error) = drain_completions(&mut renderer, &shared, &mut submitted) {
            shared.publish_error(error);
            break;
        }
        if shared.shutdown_requested() {
            // 退出前 drain 在飞帧（I4）：完成通知到达、旧资源释放之后才销毁 device/queue。
            if shared.in_flight() == 0 {
                break;
            }
            continue;
        }
        // 没有脏区就不画，drawable 留给下一个 vsync。
        if !shared.has_pending_frame() {
            continue;
        }
        let Some(drawable) = shared.take_drawable() else {
            continue;
        };
        // ① 三缓冲背压：**非阻塞**；失败就丢掉这个 drawable（帧请求留在槽里，下一个 vsync 再试）。
        if !renderer.try_acquire_upload_slot() {
            continue;
        }
        let Some(request) = shared.take_frame() else {
            // 防御：取到槽但没有帧要画，归还槽位。
            renderer.release_upload_slot();
            continue;
        };
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2）。
        for upload in shared.take_atlas().into_iter().flatten() {
            renderer.queue_atlas_upload(upload.kind, &upload.pixels, upload.rows);
        }
        // ② 目标校验：drawable 的**实际**像素尺寸就是本帧的渲染尺寸（§5.4）。
        let texture = drawable.0.texture();
        let target = (texture.width() as u32, texture.height() as u32);
        let resized = match renderer.prepare_target(target) {
            Ok(resized) => resized,
            Err(error) => {
                shared.publish_error(error);
                break;
            }
        };
        let mut frame = FramePlan::new(request.plan, resized || renderer.is_fresh());
        if frame.is_idle() {
            // 没有像素要画（Idle 且目标未重建）⇒ 不产生 GPU 工作；该序号不会出现在完成通知里（§4.3）。
            renderer.release_upload_slot();
            continue;
        }
        let index = request.index;
        let completed = {
            let shared = Arc::clone(&shared);
            move |buffer| shared.push_completion(index, SendCommandBuffer(buffer))
        };
        match renderer.submit_frame(&request, &drawable.0, &mut frame, completed) {
            Ok((stats, samples)) => {
                shared.note_submitted();
                submitted.push_back((index, stats, samples, request.published_at));
            }
            Err(error) => {
                shared.publish_error(error);
                break;
            }
        }
    }
    shared.mark_terminated();
}

/// 处理完成通知：读 GPU 时间、推进 retire、发布反馈（**只**在 GPU 线程）。
fn drain_completions(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<(FrameIndex, RenderStats, Vec<GpuTimerSample>, Instant)>,
) -> Result<(), EngineError> {
    for completion in shared.take_completions() {
        let index = completion.index;
        let sample = renderer.on_frame_completed(index, &completion.buffer.0)?;
        let (stats, mut samples, published_at) = take_submitted(submitted, index)
            .unwrap_or_else(|| (RenderStats::default(), Vec::new(), Instant::now()));
        if let Some(sample) = sample {
            samples.push(sample);
        }
        let completed_at = Instant::now();
        shared.publish_feedback(FrameFeedback {
            index,
            stats,
            samples,
            // UI 发布 → 观察到 GPU 完成（HUD 的 publish→present）。
            latency: completed_at.saturating_duration_since(published_at),
            // 完成通知到达（GPU 线程被唤醒）的时刻，近似呈现时刻。
            completed_at,
        });
    }
    Ok(())
}

/// 取走某帧提交时记录的统计（完成通知理论上按提交顺序到达，这里按序号匹配，不依赖顺序）。
fn take_submitted(
    submitted: &mut VecDeque<(FrameIndex, RenderStats, Vec<GpuTimerSample>, Instant)>,
    index: FrameIndex,
) -> Option<(RenderStats, Vec<GpuTimerSample>, Instant)> {
    let position = submitted
        .iter()
        .position(|(frame, _, _, _)| *frame == index)?;
    submitted
        .remove(position)
        .map(|(_, stats, samples, published_at)| (stats, samples, published_at))
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{AtlasSlots, StickyError, pacing_paused};
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
        frame.publish(request(1, crate::render::compositor::Plan::Full));
        frame.publish(request(2, crate::render::compositor::Plan::Idle));
        assert_eq!(
            frame.take().expect("frame").plan,
            crate::render::compositor::Plan::Full
        );
    }

    #[test]
    fn pacing_runs_while_continuous_or_busy() {
        assert!(!pacing_paused(true, false, 0), "连续帧必须跑");
        assert!(!pacing_paused(false, true, 0), "有未取走的帧请求必须跑");
        assert!(!pacing_paused(false, false, 1), "有在飞帧必须跑");
        assert!(pacing_paused(false, false, 0), "空闲：display link 暂停");
    }

    /// 离屏路径（快照 / golden）仍是**同步**语义，并且与窗口路径共用同一套 GPU 对象：
    /// 这是 Metal 后端唯一能在无窗口会话里跑通的端到端 GPU 检查。
    #[test]
    fn offscreen_snapshot_round_trips_pixels() {
        use crate::{
            math::Scale,
            paint::{
                color::{ColorResolver, Interpolation},
                instance::InstanceEncoder,
                scene::Scene,
            },
            render::macos::atlas::ATLAS_PAGE,
        };

        let mut renderer = super::Renderer::new_offscreen_with_page((16, 8), ATLAS_PAGE)
            .expect("metal offscreen renderer");
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(
            &Scene::empty(),
            Scale::ONE,
            &ColorResolver::new(Interpolation::default()),
        );
        let image = renderer.render_snapshot(buffer).expect("snapshot");
        assert_eq!((image.width, image.height), (16, 8));
        assert_eq!(image.rgba8.len(), 16 * 8 * 4);
        assert!(
            image.rgba8.iter().all(|byte| *byte == 0),
            "空场景 + 透明 clear color ⇒ 全 0 像素"
        );
    }

    #[test]
    fn sticky_errors_rebuild_the_same_variant() {
        let error = EngineError::DeviceLost {
            reason: SmolStr::new_static("MTLCommandBufferError(11)"),
            log_path: std::path::PathBuf::from("logs/metal.txt"),
        };
        let sticky = StickyError::capture(error);
        match sticky.to_error() {
            EngineError::DeviceLost { reason, log_path } => {
                assert_eq!(reason, "MTLCommandBufferError(11)");
                assert_eq!(log_path, std::path::PathBuf::from("logs/metal.txt"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let sticky = StickyError::capture(EngineError::Swapchain {
            detail: SmolStr::new_static("MTLCommandBuffer failed"),
        });
        let text = sticky.to_error().to_string();
        assert!(text.contains("MTLCommandBuffer"), "{text}");
    }
}
