//! 窗口渲染门面 + `CAMetalDisplayLink` + 每窗口一条 `kytrix-metal` GPU 线程
//! （`design/async-render-pipeline.md` §4.5 / §5）。
//!
//! ```text
//! 主线程                                   kytrix-metal 线程（每窗口）
//! ────────────────────────────────────    ──────────────────────────────────────────
//! CAMetalLayer（NSView 持有）              循环：等 condvar（有工作时才有上界）
//! CAMetalDisplayLink ── 每 vsync ──┐        → 取完成通知（GPU 计时 / retire）
//!   delegate: 存 update.drawable   │        → 取呈现通知（presentedTime → 真实呈现时刻）
//!             + targetPresentation │        → 取最新 drawable + 最新帧
//!             唤醒 GPU 线程        │        → 目标时刻已过期 ⇒ 跳过本 vsync（R09-6）
//! 写图层属性（drawableSize/scale） ┘        → 3 槽 semaphore（非阻塞，失败即跳帧）
//! sync_pacing(continuous)                  → 重建 persist → 录制
//!                                          → addPresentedHandler + presentDrawable:atTime:
//!                                          → addCompletedHandler + commit（立即返回）
//! ```
//!
//! 三条硬约束：
//! - **I1**：主线程只做「加锁拷贝 / 写图层属性 / `setPaused`」，没有 `nextDrawable`、
//!   `waitUntilCompleted`、`status()` 轮询、semaphore 等待、persist 重建。
//! - **I2**：`commit` 后立即返回；完成只通过 `addCompletedHandler`，呈现时刻只通过
//!   `addPresentedHandler`（`presentedTime` 是唯一权威值）。
//! - **I3**：节奏来自 `CAMetalDisplayLink`（主 run loop 的 **common modes**）；空闲 `isPaused = true`，
//!   有帧请求 / 在飞帧 / 连续帧时恢复。
//!
//! 设备级对象（`MTLDevice` / 两个 metallib / 4 条 PSO / 采样器）全部来自进程单例
//! [`GpuCore`]（R09-5）：本模块的每窗口状态只剩 drawable、persist、上传槽、display link 与
//! GPU 线程。
//!
//! 关闭握手（§4.5 / F-169）：`Drop` 里停 display link → signal shutdown → **有界等待** GPU 线程
//! 退出（[`JOIN_TIMEOUT`]，超时 detach，见 [`join_with_deadline`]）；GPU 线程在退出前自行 drain
//! 在飞帧（完成通知到达后才释放旧资源，I4）。

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, runtime::ProtocolObject,
};
use objc2_core_foundation::CFTimeInterval;
use objc2_foundation::{NSObject, NSRunLoop, NSRunLoopCommonModes};
use objc2_metal::{MTLCommandBuffer, MTLTexture};
use objc2_quartz_core::{
    CACurrentMediaTime, CAFrameRateRange, CAMetalDisplayLink, CAMetalDisplayLinkDelegate,
    CAMetalDisplayLinkUpdate, CAMetalDrawable, CAMetalLayer,
};
use smol_str::{SmolStr, format_smolstr};

#[cfg(any(test, feature = "test-support"))]
use crate::render::macos::readback::ReadbackImage;
#[cfg(feature = "hud")]
use crate::render::timer::Section;
use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, color::Color},
    render::{
        DirtyRows, RenderStats,
        color::background_linear,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, LatestFrame, MAX_FRAMES_IN_FLIGHT},
        macos::{
            compositor::GpuRenderer,
            core::GpuCore,
            device::{Device, enter_gpu_thread},
            queue::InFlight,
            swapchain,
        },
        timer::{FrameTimings, GpuTimerSample},
    },
    window::FrameIndex,
};

/// drawable 尺寸与请求尺寸连续不一致的 vsync 上限（`request.surface` 是 resize 的唯一来源）。
///
/// 正常路径要求两者一致（±1 px，吸收 DPI 取整）：不一致说明这个 drawable 还是上一轮 layout
/// 的产物，用它消费最新帧会把最后一帧按旧尺寸画完 —— 此后 pacing 暂停、没有新帧，窗口永久
/// 停在旧尺寸画面。连续超过本上限仍不一致时，说明不是「等一下就有新 drawable」而是取整误差，
/// 退化为「按 drawable 实际尺寸渲染 + 全量」，避免永久停更。退化状态是**粘性**的：见
/// [`SurfaceMismatch`]。
const MAX_SURFACE_MISMATCHES: u32 = 4;

/// GPU 线程自己等待在飞帧完成的上界（驱动挂死时线程不能无限等待，见
/// [`Shared::wait_for_work`]；主线程的等待另有 [`JOIN_TIMEOUT`]，两者互相独立）。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// 关闭握手的轮询间隔：只在 `shutdown && in_flight > 0` 时生效。
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// 主线程等待 GPU 线程退出的总上限（P2-7）。正常关闭是毫秒级；超过就 detach，
/// 见 [`join_with_deadline`]。
const JOIN_TIMEOUT: Duration = Duration::from_millis(250);

/// 主线程等待 GPU 线程退出的轮询间隔（只有关闭路径才睡）。
const JOIN_POLL: Duration = Duration::from_millis(5);

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

    /// 真实窗口：主线程建图层（GPU 侧对象随后整体移交），建 `CAMetalDisplayLink` 并挂到
    /// 主 run loop 的 common modes。
    ///
    /// `background` 同时写进图层的 `backgroundColor`（sRGB，旧 drawable 之外）与 persist 的
    /// clear color（linear，新 drawable 内部）。
    ///
    /// **主线程只做窗口级工作**（R09-5）：设备 / metallib / PSO 全部来自进程单例
    /// [`GpuCore`]（首次调用后才需要加载，之后是纯读），本函数新建的只有图层、persist、
    /// 三槽上传缓冲与 1×1 占位图集页 —— 都是 `newTexture` / `newBuffer` 级的小分配。
    /// PSO 的首次编译（数十毫秒）落在 `kytrix-metal` 线程的首帧录制里，不阻塞开窗。
    ///
    /// # Errors
    /// 设备 / 图层创建失败、`CAMetalDisplayLink` 无法创建（`AppInit`），或 GPU 线程无法启动。
    pub(crate) fn new_for_layer(
        size: (u32, u32),
        scale: f64,
        max_frame_rate: f32,
        background: Color,
    ) -> Result<Self, EngineError> {
        let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
            detail: SmolStr::new_static("CAMetalDisplayLink requires the main thread"),
        })?;
        let core = Arc::clone(GpuCore::shared()?);
        let device = Device::new()?;
        let layer = swapchain::new_layer(&device, size, scale, background)?;
        // 在飞计数必须在 `Queue` 构造之前建好：提交路径（GPU 线程）与 `sync_pacing`
        // （主线程）读写同一份（R09-17）。
        let in_flight = Arc::new(InFlight::new());
        let shared = Arc::new(Shared::new(Arc::clone(&in_flight)));
        let gpu =
            GpuRenderer::new_window(device, core, in_flight, size, background_linear(background))?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(
                mtm,
                gpu,
                layer,
                size,
                max_frame_rate,
                shared,
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
                gpu.set_scale(request.scale);
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
    ///
    /// 门面收切片（`window.rs` 的公共入口只持有切片），窗口路径在 UI→GPU 边界复制一次进
    /// [`AtlasSlots`]；GPU 线程消费时按**所有权**移动给 `AtlasTextures`，不再复制第二遍整页
    /// （图片页 16 MiB）。离屏路径没有跨线程槽位，直接复制一份。
    /// 把一页图集像素交给 GPU 线程（`pixels` 是 `rows` 顺序紧密打包的行）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: &[u8],
        rows: DirtyRows,
    ) -> bool {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => {
                gpu.queue_atlas_upload(kind, page, pixels.to_vec(), rows);
                true
            }
            RenderTarget::Window(window) => window.queue_atlas_upload(kind, page, pixels, rows),
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

    /// 图层属性（`contentsScale` / `drawableSize`）的**唯一**写入口：只有主线程可调用（§5.3）。
    pub(crate) fn set_layer_scale(&mut self, surface: (u32, u32), scale: f32) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_layer_scale(surface, scale);
        }
    }

    /// 换屏后把 display link 的偏好帧率钉到新屏幕的刷新率。
    ///
    /// 旧实现只在建窗时读一次 `maximumFramesPerSecond`：窗口被拖到 120 Hz 的 ProMotion
    /// 屏上仍按 60 Hz 请求（反之亦然）。离屏目标从不 present，是 no-op。
    pub(crate) fn set_frame_rate(&mut self, max_frame_rate: f32) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_frame_rate(max_frame_rate);
        }
    }

    /// 改窗口背景色（主线程）：layer 立刻写；persist 的 clear color 交给 GPU 线程。
    ///
    /// 离屏目标没有背景色（从不 present），是 no-op。
    pub(crate) fn set_background_color(&mut self, color: Color) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_background_color(color);
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

/// `CAFrameRateRange` 的兜底下限：只在拿不到真实刷新率时使用（正常路径来自
/// `NSScreen.maximumFramesPerSecond`）。**不是**请求的帧率 —— 见 `new_window` 里锁定刷新率的说明。
const MIN_FRAME_RATE: f32 = 60.0;

/// `CAMetalDisplayLinkDelegate` 的实例变量（主线程独占）。
struct DisplayLinkIvars {
    shared: Arc<Shared>,
}

define_class!(
    /// display link 的委托：**只做两件事** —— 把 update 里的 drawable 与它的目标呈现时刻放进
    /// 最新 drawable 槽、唤醒 GPU 线程。**不在回调里编码**（§5.1 / §5.3）。
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
            // R09-6：`targetPresentationTimestamp` 是这次 vsync 要求提交的**呈现时刻**
            // （host time，与 `CACurrentMediaTime()` / `MTLDrawable.presentedTime` 同一时基）。
            // 提交时必须把它交给 `presentDrawable:atTime:`，否则 CA 自行决定呈现时刻，
            // commit 晚于层的调度点就被推到下一个 vsync（整整多一帧）。
            self.ivars()
                .shared
                .publish_drawable(update.drawable(), update.targetPresentationTimestamp());
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
    layer: Retained<CAMetalLayer>,
    display_link: Retained<CAMetalDisplayLink>,
    /// `delegate` 是 **weak** 属性（objc2 与 Apple 文档都如此）：必须由门面持有强引用，
    /// 否则回调会打在已释放的对象上（§5.1）。
    _delegate: Retained<DisplayLinkDelegate>,
    /// 最近一次**发布**的目标尺寸（`publish` 里更新；macOS 是 `Window.surface` 的真实 backing）。
    surface: (u32, u32),
    /// 最近一次写进图层的物理尺寸 / DPI（避免每帧重复写 CA 属性，§5.3）。
    layer_size: (u32, u32),
    layer_scale: f64,
    /// 当前写进 display link 的偏好帧率（换屏时比较用）。
    frame_rate: f32,
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
        shared: Arc<Shared>,
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let delegate = DisplayLinkDelegate::new(mtm, Arc::clone(&shared));
        let display_link =
            CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
        display_link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // `CAFrameRateRange` 的 `minimum` 是「**可以**降到多低」，不是「至少多高」。
        // 旧实现写 `(30, max, max)` 等于告诉系统「30 fps 也可接受」，ProMotion 上 GPU 跟得上
        // 时也会被降到 30 而产生 judder（P1-2.12）。正确用法是把三点都钉在目标刷新率上：
        // Apple GPTK 与 Metal 文档都要求 `CAFrameRateRangeMake(max, max, max)` 锁定刷新率。
        let target = max_frame_rate.max(MIN_FRAME_RATE);
        display_link.setPreferredFrameRateRange(CAFrameRateRange::new(target, target, target));
        // 没有帧要画就没有 vsync：初始空闲（`publish` / `sync_pacing` 负责恢复）。
        display_link.setPaused(true);
        // SAFETY: 主线程、主 run loop；common modes 让 live resize（`NSEventTrackingRunLoopMode`）
        // 与全屏转场期间仍然回调（§5.3，行为由 §11 探针 1 实测）。
        unsafe {
            display_link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
        }

        // GPU 线程自持一份图层引用（保活）：detach 之后主线程先释放窗口/图层，迟到的
        // `presentDrawable` 也不会打到已释放的 `CAMetalLayer` 上（见 `join_with_deadline`）。
        let layer_keep_alive = SendLayer {
            _layer: layer.clone(),
        };
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kytrix-metal".to_owned())
            .spawn(move || gpu_loop(gpu, worker, layer_keep_alive))
            .map_err(|error| EngineError::AppInit {
                detail: format_smolstr!("spawn kytrix-metal thread: {error}"),
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            layer,
            display_link,
            _delegate: delegate,
            surface: size,
            layer_size: size,
            layer_scale: 0.0,
            frame_rate: target,
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

    fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: &[u8],
        rows: DirtyRows,
    ) -> bool {
        self.shared.queue_atlas(kind, page, pixels, rows)
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.shared.atlas_idle(kind)
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

    /// 换屏（或系统刷新率变化）后重新把 display link 的偏好帧率钉到新刷新率。
    ///
    /// 旧实现只在建窗时读一次 `maximumFramesPerSecond`：窗口被拖到 120 Hz 的 ProMotion 屏
    /// 上之后仍按 60 Hz 请求，表现为「高刷屏上依旧 60 fps」。窗口层每轮 `sync_state` 比较
    /// 当前屏刷新率，变化时调用这里。
    fn set_frame_rate(&mut self, max_frame_rate: f32) {
        let target = max_frame_rate.max(MIN_FRAME_RATE);
        if (target - self.frame_rate).abs() <= f32::EPSILON {
            return;
        }
        self.frame_rate = target;
        self.display_link
            .setPreferredFrameRateRange(CAFrameRateRange::new(target, target, target));
    }

    /// 先写主线程独占的图层背景色，再把 persist 的 clear color（linear）交给 GPU 线程。
    ///
    /// 顺序不能反：图层属性只能在主线程写，而 GPU 线程一被唤醒就要读共享槽；两者都来自同一个
    /// `Color`，所以条带颜色永远一致。
    fn set_background_color(&self, color: Color) {
        swapchain::set_layer_background(&self.layer, color);
        self.shared.set_background(background_linear(color));
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
        join_with_deadline(thread);
    }
}

/// 有界等待 GPU 线程退出（P2-7）：超过 [`JOIN_TIMEOUT`] 就 **detach**（丢弃 `JoinHandle`），不让
/// 驱动挂死把主线程（AppKit 事件循环）拖住。
///
/// 参照 Windows 后端的 `join_with_message_pump` / `JOIN_TIMEOUT_MS`：那边边等边泵窗口消息、总上限
/// 10 s；macOS 的 GPU 线程**不需要主线程**任何帮助（drawable 由 display link 在停表前给出，提交是
/// 异步的，完成回调来自 Metal），所以主线程只需要一个很短的固定上限，不需要泵事件。
///
/// **detach 的安全性**：GPU 线程的闭包按值拥有 `GpuRenderer`（device / queue / persist / 上传
/// 缓冲 / 图集）与 `Arc<Shared>`，还持有一份 `CAMetalLayer` 强引用（见 [`gpu_loop`] 的
/// `_keep_alive` 参数）；主线程随后释放自己那一份引用不会让线程手里的任何对象失效。代价只有
/// 驱动挂死时资源要等进程退出才释放 —— 与 Windows 后端 detach 的取舍一致，好过主线程永久卡住。
fn join_with_deadline(thread: JoinHandle<()>) {
    let deadline = Instant::now() + JOIN_TIMEOUT;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            eprintln!(
                "kytrix: kytrix-metal thread did not exit within {:?}; detaching it (GPU likely lost)",
                JOIN_TIMEOUT
            );
            return;
        }
        thread::sleep(JOIN_POLL);
    }
    // 线程已经结束：`join` 立即返回（panic 已由 `PanicGuard` 交给 `poll`）。
    let _ = thread.join();
}

/// 命令缓冲跨线程移交（Metal 完成回调 → GPU 线程 drain）。
struct SendCommandBuffer(Retained<ProtocolObject<dyn MTLCommandBuffer>>);

// SAFETY: Metal 对象本身线程安全（`MTLCommandBuffer` 的完成回调可能在任何线程执行）；完成回调
// 交出引用后，只有 GPU 线程持有它。
unsafe impl Send for SendCommandBuffer {}

/// display link 交给 GPU 线程的一次呈现机会：drawable + 这次 vsync 要求的呈现时刻。
///
/// 目标时刻来自 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp`（host time，R09-6）。
struct DrawableUpdate {
    drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>,
    /// host time（`CACurrentMediaTime()` 时基）。`0.0` = 系统没给（不该发生，按"不可用"处理）。
    target_presentation: CFTimeInterval,
}

// SAFETY: Metal 对象本身线程安全；放进最新 drawable 槽之后只有 GPU 线程持有引用。
// `CFTimeInterval` 是 `f64`，天然可跨线程。
unsafe impl Send for DrawableUpdate {}

/// 图层的跨线程**保活**句柄（GPU 线程只持有引用，不解引用、不读写任何属性）。
struct SendLayer {
    /// 只为保活：detach 之后线程可能还在提交最后一帧，`presentDrawable` 需要图层还活着。
    _layer: Retained<CAMetalLayer>,
}

// SAFETY: 图层属性仍然只有主线程写（§5.3）；GPU 线程这一份引用只用于让 detach 之后仍在收尾的
// `presentDrawable` 不落到已释放的图层上。`Retained` 的引用计数本身是原子的。
unsafe impl Send for SendLayer {}

/// UI 线程与 GPU 线程共享的最新帧 / 图集 / 最新 drawable / 完成队列 / 呈现队列 / 反馈槽 + 唤醒原语。
struct Shared {
    state: Mutex<State>,
    work: Condvar,
    /// 每 `AtlasKind` 的共享槽是否正在被 GPU 线程消费（`false` = UI 可以入队）。
    atlas_pending: [AtomicBool; 2],
    /// 已提交、尚未 drain 的帧数。提交路径在**记账之后**才登记完成回调（R09-17），
    /// `sync_pacing`（主线程）与关闭握手都读它。
    in_flight: Arc<InFlight>,
}

struct State {
    frame: LatestFrame,
    /// 最新帧槽里那一帧的目标尺寸（`FrameRequest::surface`）。
    ///
    /// GPU 线程必须**在取帧之前**用它校验 drawable 的纹理尺寸（P1）：drawable 可能还是上一轮
    /// layout 的产物。`take_frame_for_drawable` 取走帧时同步清空，因此它与 `frame` 一一对应
    /// （同一把锁里维护）。
    published_surface: Option<(u32, u32)>,
    atlas: AtlasSlots,
    /// 最新 drawable 与它被要求呈现的**目标时刻**（display link 每 vsync 覆盖旧的；GPU 线程
    /// 取走即清空）。目标时刻是 host time（`CACurrentMediaTime()` 时基，R09-6）。
    drawable: Option<DrawableUpdate>,
    /// 已完成、等 GPU 线程 drain 的命令缓冲（完成回调入队，本队列只增不减）。
    completions: VecDeque<Completion>,
    /// 已提交命令缓冲的**真实呈现时刻**（`addPresentedHandler` 给的 `presentedTime` 换算成
    /// `Instant`；只增不减，GPU 线程 drain）。
    presented: VecDeque<(FrameIndex, Instant)>,
    feedback: Option<FrameFeedback>,
    error: Option<EngineError>,
    shutdown: bool,
    /// 关闭请求的时刻：[`Shared::wait_for_work`] 的 grace 从它起算（跨多次等待累计）。
    shutdown_at: Option<Instant>,
    /// GPU 线程已经退出（`publish` 从此是 no-op）。
    terminated: bool,
    /// 待应用的 persist clear color（linear；最新者胜，GPU 线程空闲 / 取帧后都会取走）。
    background: Option<[f32; 4]>,
}

impl State {
    const fn new() -> Self {
        Self {
            frame: LatestFrame::new(),
            published_surface: None,
            atlas: AtlasSlots::new(),
            drawable: None,
            completions: VecDeque::new(),
            presented: VecDeque::new(),
            feedback: None,
            error: None,
            shutdown: false,
            shutdown_at: None,
            terminated: false,
            background: None,
        }
    }

    /// GPU 线程的等待条件：有完成通知 / 有呈现通知 / 有帧且有 drawable / 有待应用背景色 /
    /// 关闭且已无在飞帧。
    ///
    /// 关闭开始后不再开新帧：只等完成通知（`in_flight` 归零或有通知可 drain）。否则残留的
    /// 「待画帧 + drawable」会让循环空转，也会让 [`Shared::wait_for_work`] 的 grace 永远不生效。
    ///
    /// 待应用背景色也是工作：`set_background` 用 condvar 唤醒，若等待条件不看它，这次唤醒会
    /// 被当作伪唤醒直接睡回去，颜色永远不落地（Windows 的事件句柄没有这个问题）。
    fn has_work(&self, in_flight: usize) -> bool {
        if self.shutdown {
            return in_flight == 0 || !self.completions.is_empty();
        }
        !self.completions.is_empty()
            || !self.presented.is_empty()
            || self.background.is_some()
            || (self.frame.has_pending() && self.drawable.is_some())
    }
}

/// [`Shared::wait_for_work`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    /// 有新工作，或关闭后的在飞帧已归零（循环的下一个检查点负责退出）。
    Work,
    /// 关闭后等待在飞帧完成超过 [`SHUTDOWN_GRACE`]（驱动没有回调）。
    TimedOut,
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
    page: u32,
    /// `rows` 顺序紧密打包的行（整页上传时就是整页）。
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

    /// 入队一次上传。**槽位由 UI 线程的 `atlas_idle` 保证空闲**（DEVIATIONS F-211）：
    /// 槽里已有未消费的上传时是契约破坏（会丢行），release 也硬失败。
    fn queue(&mut self, kind: AtlasKind, page: u32, pixels: Vec<u8>, rows: DirtyRows) {
        let slot = match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        };
        assert!(
            slot.is_none(),
            "atlas slot must be idle before queueing a new upload (kind {kind:?})"
        );
        *slot = Some(AtlasUpload {
            kind,
            page,
            pixels,
            rows,
        });
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

/// 距离 display link 要求的目标呈现时刻不足这个量，就认为**来不及提交**。
///
/// 一个 vsync 是 8.3 ms（120 Hz）/ 16.7 ms（60 Hz）；留 2 ms 给"提交 → CA 收到"的调度。
/// 过期的提交会被 CA 推到下一个 vsync，表现为固定的 1 帧额外延迟与抖动，所以宁可跳过本 vsync。
const PRESENT_MARGIN: Duration = Duration::from_millis(2);

/// 连续错过目标呈现时刻的上限：超过就退回"立即呈现"。
///
/// 只看 [`PRESENT_MARGIN`] 会在 CPU 持续跟不上（或 display link 给的目标时刻异常）时永远跳过
/// —— 那等于画面冻结。允许连跳 2 个 vsync 重新对齐，第 3 次照常提交。
const MAX_MISSED_PRESENTS: u32 = 2;

/// 等 `presentedTime` 的上界：CoreAnimation 可能永远不回调（窗口被遮挡、drawable 被跳过），
/// 反馈不能永远不发布。超过它就用完成时刻兜底。
const PRESENTED_WAIT: Duration = Duration::from_millis(32);

/// 本 vsync 的呈现决定（[`PresentPacing::plan`] 的结论）。
#[derive(Clone, Copy, Debug, PartialEq)]
enum PresentPlan {
    /// `presentDrawable:atTime:`：钉到 display link 要求的那一帧。
    AtTime(CFTimeInterval),
    /// `presentDrawable:`：目标时刻不可用，或连续错过太多次。
    Now,
    /// 跳过本 vsync：目标时刻已经不够提交（提交注定被推到下一帧）。
    Skip,
}

/// 呈现时刻判定（R09-6；纯状态机便于单测）。
///
/// 输入是 `CACurrentMediaTime()` 与 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp`
/// （同一 host time 时基）。CPU 落后时**跳过本 vsync**而不是立刻 present，避免"这一帧迟到一
/// 个 vsync、下一帧又正常"造成的 8~17 ms 级抖动；但连续错过有上界，否则会冻结画面。
struct PresentPacing {
    /// 连续错过目标时刻的次数。
    missed: u32,
}

impl PresentPacing {
    const fn new() -> Self {
        Self { missed: 0 }
    }

    fn plan(&mut self, now: CFTimeInterval, target: CFTimeInterval) -> PresentPlan {
        // 系统没给可用目标时刻（headless / 异常值）：回到"尽快呈现"，与改动前行为一致。
        if !(target.is_finite() && target > 0.0) {
            self.missed = 0;
            return PresentPlan::Now;
        }
        if now <= target - PRESENT_MARGIN.as_secs_f64() {
            self.missed = 0;
            return PresentPlan::AtTime(target);
        }
        self.missed = self.missed.saturating_add(1);
        if self.missed > MAX_MISSED_PRESENTS {
            // CPU 持续落后：继续跳只会让画面停住，退回立即呈现（迟到的代价小于不呈现）。
            self.missed = 0;
            PresentPlan::Now
        } else {
            PresentPlan::Skip
        }
    }
}

/// host time（秒，`mach_absolute_time` 时基）→ `Instant`。
///
/// `MTLDrawable.presentedTime` 与 `CACurrentMediaTime()` 是同一时基，与 `Instant`（单调时钟）
/// 只差"采样此刻"这一个点：先算两者的差（`age`），再从 `observed` 往回推。非法值（0 / NaN /
/// 未来时刻）返回 `None`，调用方退回完成时刻。
fn host_time_to_instant(
    presented: CFTimeInterval,
    observed: Instant,
    now_host: CFTimeInterval,
) -> Option<Instant> {
    if !presented.is_finite() || presented <= 0.0 || !now_host.is_finite() {
        return None;
    }
    // 理论上 presented ≤ now_host；未来时刻按"刚刚"处理，避免 `from_secs_f64` 收到负数。
    let age = (now_host - presented).max(0.0);
    observed.checked_sub(Duration::from_secs_f64(age))
}

/// 请求的目标尺寸与 drawable 的实际尺寸是否一致（容差 1 px）。
///
/// 1 px 只用于吸收 DPI / 图层取整：`set_layer_scale` 写的 `drawableSize` 就是
/// `Window.surface`（macOS 是 AppKit 的真实 backing），drawable 纹理由图层按同一个
/// `drawableSize` 分配，理论上逐像素相等；差 1 px 不值得再等一个 vsync（真差一整个 layout
/// 时差值远大于 1）。
fn surface_within_tolerance(requested: (u32, u32), drawable: (u32, u32)) -> bool {
    requested.0.abs_diff(drawable.0) <= 1 && requested.1.abs_diff(drawable.1) <= 1
}

/// 尺寸不一致时本 vsync 的处理方式（[`SurfaceMismatch::observe`] 的结论）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceVerdict {
    /// 尺寸一致（或没有待画帧）：按正常路径绘制。
    Ready,
    /// 还不值得退化：丢弃这个陈旧 drawable、跳过本 vsync，等下个 vsync 的新 drawable。
    ///
    /// 视觉正确：图层 `contentsGravity = bottomLeft`（flipped NSView 下即视觉左上）让旧
    /// drawable 保持 1:1（不缩放），`backgroundColor` 补齐新暴露的条带，所以跳过一个 vsync
    /// 不会看到拉伸/发虚。
    Skip,
    /// 粘性退化：按 drawable 实际尺寸全量绘制，直到请求尺寸重新落在容差内。
    Degraded,
}

/// drawable 尺寸与请求尺寸不一致的退化状态（P1 的粘性修复）。
///
/// 连续 [`MAX_SURFACE_MISMATCHES`] 个 vsync 不一致后进入退化。状态**粘性**：不能每次成功提交
/// 后归零，否则持续 >1 px 不一致会在「跳 4 个 vsync + 画一帧」之间固定循环（~11 fps）并每轮
/// 重新打印诊断；进入退化后每个 vsync 都画，只有请求尺寸与 drawable 重新在容差内才清除并归零。
struct SurfaceMismatch {
    consecutive: u32,
    degraded: bool,
}

impl SurfaceMismatch {
    const fn new() -> Self {
        Self {
            consecutive: 0,
            degraded: false,
        }
    }

    /// 观察本 vsync 的请求尺寸与 drawable 尺寸，推进退化状态机。
    fn observe(&mut self, requested: Option<(u32, u32)>, drawable: (u32, u32)) -> SurfaceVerdict {
        let Some(requested) = requested else {
            return SurfaceVerdict::Ready;
        };
        if surface_within_tolerance(requested, drawable) {
            // 尺寸重新一致：清除退化 episode 并归零，下一次不一致重新计数。
            self.consecutive = 0;
            self.degraded = false;
            return SurfaceVerdict::Ready;
        }
        if self.degraded {
            return SurfaceVerdict::Degraded;
        }
        self.consecutive += 1;
        if self.consecutive <= MAX_SURFACE_MISMATCHES {
            SurfaceVerdict::Skip
        } else {
            self.degraded = true;
            SurfaceVerdict::Degraded
        }
    }
}

impl Shared {
    fn new(in_flight: Arc<InFlight>) -> Self {
        Self {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicBool::new(false), AtomicBool::new(false)],
            in_flight,
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
        state.published_surface = Some(request.surface);
        state.frame.publish(request);
        drop(state);
        self.wake();
    }

    fn queue_atlas(&self, kind: AtlasKind, page: u32, pixels: &[u8], rows: DirtyRows) -> bool {
        let index = kind.index() as usize;
        // 像素在**锁外**复制：整页可达 16 MiB，不能让 GPU 线程的取帧锁等这次 memcpy。
        let pixels = pixels.to_vec();
        // 先占位再入队：GPU 线程录制完成之后才释放（DEVIATIONS F-211）。
        self.atlas_pending[index].store(true, Ordering::Release);
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            self.atlas_pending[index].store(false, Ordering::Release);
            return false;
        }
        // 不唤醒：图集上传统一在「录制下一帧」之前落地（§4.2），单独唤醒只会空转。
        state.atlas.queue(kind, page, pixels, rows);
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

    /// display link delegate（主线程）：把这一帧的 drawable 与它的目标呈现时刻放进最新
    /// drawable 槽并唤醒 GPU 线程。
    fn publish_drawable(
        &self,
        drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>,
        target_presentation: CFTimeInterval,
    ) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.drawable = Some(DrawableUpdate {
            drawable,
            target_presentation,
        });
        drop(state);
        self.wake();
    }

    /// 完成回调（Metal 的任意线程）：入队 + 唤醒 GPU 线程（**不**碰 retire / 计时）。
    fn push_completion(&self, index: FrameIndex, buffer: SendCommandBuffer) {
        let mut state = self.lock();
        // 只丢弃「消费者已经退出」之后的完成通知：`terminated` 之后没有线程会 drain，继续入队
        // 只会泄漏命令缓冲。**不能**在 `shutdown` 时就丢弃 —— 关闭握手正是靠这些通知把在飞帧
        // drain 到 0（§4.5），丢了会让 GPU 线程永远等下去（`Drop` 最终只能放弃 join）。
        if state.terminated {
            return;
        }
        state.completions.push_back(Completion { index, buffer });
        drop(state);
        self.wake();
    }

    /// 呈现回调（CoreAnimation 的呈现线程）：记下本帧的**真实呈现时刻**并唤醒 GPU 线程。
    ///
    /// `presented_time` 是 `MTLDrawable.presentedTime`（host time，秒）；换算成 `Instant` 需要
    /// 同一时刻的 host time 采样，因此这里同时取 `CACurrentMediaTime()`。
    fn push_presented(&self, index: FrameIndex, presented_time: CFTimeInterval) {
        let observed = Instant::now();
        let Some(presented_at) =
            host_time_to_instant(presented_time, observed, CACurrentMediaTime())
        else {
            // `presentedTime == 0`：这一帧没有上屏（被 CA 跳过）。不记呈现时刻，
            // 反馈退回"完成时刻"（HUD 的含义仍是"提交→GPU 完成"）。
            return;
        };
        let mut state = self.lock();
        if state.terminated {
            return;
        }
        state.presented.push_back((index, presented_at));
        drop(state);
        self.wake();
    }

    /// 取走全部呈现通知（GPU 线程 drain）。
    fn take_presented(&self) -> Vec<(FrameIndex, Instant)> {
        self.lock().presented.drain(..).collect()
    }

    /// 空闲：睡在 condvar 上直到「有完成 / 呈现通知 / 有帧且有 drawable / 关闭且无在飞帧」。
    ///
    /// 关闭后有在飞帧但驱动永不回调完成时，等待必须有上界（否则 GPU 线程自己永不退出，
    /// detach 之后还会一直占着资源）：用 100 ms 超时轮询，从关闭请求起**累计**超过
    /// [`SHUTDOWN_GRACE`] 返回 [`WaitOutcome::TimedOut`]。
    ///
    /// `awaiting_presentation` 为真时（已有 GPU 完成、还在等 `presentedTime` 的帧）等待也必须
    /// 有上界：CoreAnimation 可能永远不回调（窗口被遮挡 / drawable 被跳过），反馈不能永远不发布。
    /// 这条超时返回 [`WaitOutcome::Work`]，让调用方用完成时刻兜底发布后重新判定。
    fn wait_for_work(&self, awaiting_presentation: bool) -> WaitOutcome {
        let mut state = self.lock();
        loop {
            if state.has_work(self.in_flight.get()) {
                return WaitOutcome::Work;
            }
            if state.shutdown {
                let started = state.shutdown_at.unwrap_or_else(Instant::now);
                let elapsed = started.elapsed();
                if elapsed >= SHUTDOWN_GRACE {
                    return WaitOutcome::TimedOut;
                }
                let timeout = SHUTDOWN_POLL.min(SHUTDOWN_GRACE - elapsed);
                state = self
                    .work
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            } else if awaiting_presentation {
                // 等呈现通知的上界：睡 [`PRESENTED_WAIT`] 后返回，由调用方用完成时刻兜底发布
                // （HUD 不允许停在旧值；CoreAnimation 可能永远不回调，例如 drawable 被跳过）。
                let _ = self
                    .work
                    .wait_timeout(state, PRESENTED_WAIT)
                    .unwrap_or_else(PoisonError::into_inner);
                return WaitOutcome::Work;
            } else {
                state = self
                    .work
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    fn has_pending_frame(&self) -> bool {
        self.lock().frame.has_pending()
    }

    /// 最新帧槽里那一帧的目标尺寸（**不取帧**）：GPU 线程在占槽/取帧之前用它校验 drawable。
    fn published_surface(&self) -> Option<(u32, u32)> {
        self.lock().published_surface
    }

    /// 取走当前帧，但**仅当**它的目标尺寸与 drawable 的实际纹理尺寸一致（±1 px）。
    ///
    /// 不一致时返回 `None` 并把帧留在最新帧槽里（它的 damage 不能丢）：调用方丢弃这个陈旧
    /// drawable、跳过本 vsync（P1）。`force` = 连续多次不一致之后的退化路径（按 drawable 的
    /// 实际尺寸渲染 + 全量，见 [`MAX_SURFACE_MISMATCHES`]）。
    fn take_frame_for_drawable(&self, target: (u32, u32), force: bool) -> Option<FrameRequest> {
        let mut state = self.lock();
        let surface = state.published_surface?;
        if !force && !surface_within_tolerance(surface, target) {
            return None;
        }
        state.published_surface = None;
        state.frame.take()
    }

    fn take_drawable(&self) -> Option<DrawableUpdate> {
        self.lock().drawable.take()
    }

    fn take_atlas(&self) -> [Option<AtlasUpload>; 2] {
        // 这里**不**清 `atlas_pending`：GPU 线程只是把上传拿走，还没录进 command buffer。
        // 清得太早，UI 会在本帧未录制（Idle / 尺寸不匹配）时入队下一笔，覆盖掉这一笔（丢行）。
        self.lock().atlas.take()
    }

    /// 本帧的图集上传已经编码进 command buffer：共享槽这才算空（UI 可以入队下一笔）。
    fn atlas_uploaded(&self, kind: AtlasKind) {
        self.atlas_pending[kind.index() as usize].store(false, Ordering::Release);
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        !self.atlas_pending[kind.index() as usize].load(Ordering::Acquire)
    }

    /// 取走全部完成通知，并把在飞帧数减去同样多（GPU 线程 drain）。
    ///
    /// 计数错误（扣得比记的多）是记账 bug，由 [`InFlight::note_drained`] 硬失败 —— 旧实现的
    /// `saturating_sub` 会把它静默钳到 0（R09-17）。
    fn take_completions(&self) -> Vec<Completion> {
        let mut state = self.lock();
        let completions: Vec<Completion> = state.completions.drain(..).collect();
        self.in_flight.note_drained(completions.len());
        completions
    }

    fn in_flight(&self) -> usize {
        self.in_flight.get()
    }

    /// `sync_pacing` 的输入：是否有未取走的帧请求、是否有在飞帧。
    fn pacing_state(&self) -> (bool, usize) {
        let state = self.lock();
        (state.frame.has_pending(), self.in_flight.get())
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
    /// `publish` / 完成回调变成 no-op（与 Linux 的 `publish_panic` 同一语义）。
    fn publish_panic(&self) {
        let mut state = self.lock();
        state.feedback = None;
        if state.error.is_none() {
            state.error = Some(EngineError::AppInit {
                detail: SmolStr::new_static("kytrix-metal thread panicked"),
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
        let mut state = self.lock();
        state.shutdown = true;
        state.shutdown_at = Some(Instant::now());
        drop(state);
        self.wake();
    }
}

/// GPU 线程的 panic 出路：展开时把错误交给 UI（`poll()` 不会静默卡死），并标记线程已终止。
///
/// 正常退出时 [`PanicGuard::disarm`] 消费自己，**不覆盖**循环已经发布的错误。
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

/// GPU 线程主循环（§5.2）。
///
/// `_keep_alive` 是**保活引用**：detach 之后线程可能还在提交最后一帧，而 `presentDrawable` 的目标
/// 正是这个图层。主线程在 `Drop` 里已经释放了自己的引用，这份引用保证图层活到线程退出（图层属性
/// 仍然只有主线程写，这里不读不写，§5.3）。
fn gpu_loop(mut renderer: GpuRenderer, shared: Arc<Shared>, _keep_alive: SendLayer) {
    enter_gpu_thread();
    // panic 出路：展开时把错误交给 UI（`poll` 不会静默停摆）；正常路径 disarm。
    let guard = PanicGuard::new(Arc::clone(&shared));
    // 已提交帧的统计与发布时刻（完成时拼成 `FrameFeedback`；落后 ≤1 帧）。按提交顺序保存，
    // 只有 `CompletionOrder` 的连续前缀推进时才发布（完成回调可以乱序到达，见 [`FeedbackQueue`]）。
    let mut feedback = FeedbackQueue::new();
    // drawable 与请求尺寸不一致的退化状态（P1；粘性，见 [`SurfaceMismatch`]）。
    let mut surface = SurfaceMismatch::new();
    // display link 的目标呈现时刻判定（R09-6；纯状态机，见 [`PresentPacing`]）。
    let mut pacing = PresentPacing::new();
    // 最近一次推进到的连续完成前沿：呈现通知晚到时也要按它发布反馈（见 `FeedbackQueue`）。
    let mut frontier: Option<FrameIndex> = None;
    // 已从共享槽取走、但还没编码进本帧 command buffer 的图集上传（按 `AtlasKind`）。
    let mut atlas_awaiting = [false; 2];
    loop {
        // 空闲等待前应用一次待定背景色：改色不要求提交一帧（UI 只写共享槽 + 唤醒）。
        if let Some(linear) = shared.take_background() {
            renderer.set_background_color(linear);
        }
        if shared.wait_for_work(feedback.awaiting_presentation()) == WaitOutcome::TimedOut {
            // 驱动没有回调完成：线程自己不能永远等下去（detach 之后它会一直占着资源；进程退出前
            // 也没有人会来救它）。Metal 的命令缓冲自己持有资源引用，退出时释放 device/queue 不会
            // UAF。
            eprintln!(
                "kytrix: metal shutdown waited {} s for {} in-flight frame(s) without a completion callback; exiting the GPU thread",
                SHUTDOWN_GRACE.as_secs(),
                shared.in_flight()
            );
            break;
        }
        // 呈现通知先落地：它可能比完成通知晚到一帧（`presentedTime` 要等真正上屏）。
        for (index, presented_at) in shared.take_presented() {
            feedback.presented(index, presented_at);
        }
        if let Err(error) = drain_completions(&mut renderer, &shared, &mut feedback, &mut frontier)
        {
            shared.publish_error(error);
            break;
        }
        // 连续完成前缀推进过（或之前推进过）：按提交顺序发布已经拿到呈现时刻的帧；
        // 等不到呈现时刻的帧由 [`PRESENTED_WAIT`] 兜底用完成时刻发布。
        if let Some(frontier) = frontier {
            let now = Instant::now();
            feedback.pop_ready(frontier, now, |frame| shared.publish_feedback(frame));
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
        let Some(update) = shared.take_drawable() else {
            continue;
        };
        // ① drawable 的实际像素尺寸就是本帧的渲染尺寸（§5.4）。
        let texture = update.drawable.texture();
        let target = (texture.width() as u32, texture.height() as u32);
        // ② 呈现时刻判定（R09-6）：display link 要求本帧在 `target_presentation` 这一帧上屏。
        //    已经不够提交（CPU 落后）时**跳过本 vsync**，而不是提交一帧注定被推到下一个 vsync 的
        //    内容 —— 那种"迟到帧"会让画面在 1/2 帧之间抖动。连续落后太多则由 `PresentPacing`
        //    退回立即呈现，避免永远不呈现（画面冻结）。
        let present_at = match pacing.plan(CACurrentMediaTime(), update.target_presentation) {
            PresentPlan::Skip => continue,
            PresentPlan::AtTime(time) => Some(time),
            PresentPlan::Now => None,
        };
        // ③ resize 校验（P1）必须在**占槽与取帧之前**：drawable 可能还是上一轮 layout 的产物，
        //    用它消费最新帧会把最后一帧按旧尺寸画完并 present；此后 pacing 暂停、没有新帧
        //    ⇒ 窗口永久停在旧尺寸画面。不一致就丢弃这个 drawable、跳过本 vsync，帧留在最新
        //    帧槽里（`pending=true` 让 display link 保持运行），下一个 vsync 再取新尺寸的 drawable。
        //    连续超过 `MAX_SURFACE_MISMATCHES` 次后进入**粘性**退化：之后每个 vsync 都按 drawable
        //    尺寸全量画，直到尺寸重新在容差内（诊断只在进入退化时打印一次）。
        let requested = shared.published_surface();
        let was_degraded = surface.degraded;
        let verdict = surface.observe(requested, target);
        if !was_degraded && surface.degraded {
            let requested = requested.unwrap_or(target);
            eprintln!(
                "kytrix: metal drawable {}x{} never matched the requested {}x{} within {} vsyncs; rendering at the drawable size (full frame) from now on",
                target.0, target.1, requested.0, requested.1, MAX_SURFACE_MISMATCHES
            );
        }
        if verdict == SurfaceVerdict::Skip {
            continue;
        }
        let degrade = verdict == SurfaceVerdict::Degraded;
        // ④ 三缓冲背压：**非阻塞**；失败就丢掉这个 drawable（帧请求留在槽里，下一个 vsync 再试）。
        //    必须在尺寸校验之后占槽：占槽后放弃会让 semaphore 与 ring 槽位不同源。
        if !renderer.try_acquire_upload_slot() {
            continue;
        }
        let Some(request) = shared.take_frame_for_drawable(target, degrade) else {
            // 尺寸在两次加锁之间又变了（新帧发布）：归还槽位，帧留在槽里。
            renderer.release_upload_slot();
            continue;
        };
        // 等 drawable / 配额期间到达的背景色：与帧一起落地（仍是录制前，不引入新的 GPU 调用点）。
        if let Some(linear) = shared.take_background() {
            renderer.set_background_color(linear);
        }
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2；像素按所有权移交给 GPU）。
        for (index, upload) in shared.take_atlas().into_iter().enumerate() {
            if let Some(upload) = upload {
                atlas_awaiting[index] = true;
                renderer.queue_atlas_upload(upload.kind, upload.page, upload.pixels, upload.rows);
            }
        }
        let resized = match renderer.prepare_target(target) {
            Ok(resized) => resized,
            Err(error) => {
                shared.publish_error(error);
                break;
            }
        };
        let mut frame = FramePlan::new(request.plan, resized || renderer.is_fresh() || degrade);
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
        let presented = {
            let shared = Arc::clone(&shared);
            move |presented_time| shared.push_presented(index, presented_time)
        };
        match renderer.submit_frame(
            &request,
            &update.drawable,
            present_at,
            &mut frame,
            completed,
            presented,
        ) {
            Ok((stats, samples)) => {
                // 上传已经编码进本帧 command buffer：共享槽这才算空，UI 可以入队下一笔。
                for (index, awaiting) in atlas_awaiting.iter_mut().enumerate() {
                    if std::mem::take(awaiting) {
                        shared.atlas_uploaded(if index == 0 {
                            AtlasKind::Glyph
                        } else {
                            AtlasKind::Image
                        });
                    }
                }
                // **不**在这里清退化状态：粘性状态只在尺寸重新一致时清除（`SurfaceMismatch`）。
                // 在飞记账已经在 `Queue::submit` 里、`commit` **之前**完成（R09-17）。
                feedback.push(index, stats, samples, request.published_at);
            }
            Err(error) => {
                shared.publish_error(error);
                break;
            }
        }
    }
    shared.mark_terminated();
    guard.disarm();
}

/// 处理完成通知：读 GPU 时间、推进 retire（**只**在 GPU 线程）。
///
/// 新的连续完成前沿写进 `frontier`（调用方在 drain 之后统一按提交顺序发布反馈）。
fn drain_completions(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    feedback: &mut FeedbackQueue,
    frontier: &mut Option<FrameIndex>,
) -> Result<(), EngineError> {
    for completion in shared.take_completions() {
        let index = completion.index;
        let (sample, advanced) = renderer.on_frame_completed(index, &completion.buffer.0)?;
        feedback.complete(index, sample);
        if advanced.is_some() {
            *frontier = advanced;
        }
    }
    Ok(())
}

/// 一帧的提交记账（反馈的统计与发布基准）。
struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
    published_at: Instant,
    /// 完成通知到达的时刻（`None` = 仍在飞）。
    completed_at: Option<Instant>,
    /// `MTLDrawable.presentedTime` 换算出的**真实呈现时刻**（`None` = 还没收到呈现通知）。
    presented_at: Option<Instant>,
}

/// 按提交顺序发布反馈的队列（`render/frame.rs` 的跨平台契约）。
///
/// Apple 只保证同一队列 command buffer 的**开始**顺序，完成回调可以乱序到达。按回调顺序发布
/// 会让 HUD 在乱序时用更旧帧的 `latency` 覆盖新值。这里把完成数据记到各自的提交槽上，只有
/// `CompletionOrder` 的连续前缀推进时（[`FeedbackQueue::pop_ready`]）才从队首按提交顺序弹出并
/// 发布；被跳过的帧（不产生 GPU 工作）不在队列里，不影响顺序。
struct FeedbackQueue {
    submitted: VecDeque<SubmittedFrame>,
}

impl FeedbackQueue {
    fn new() -> Self {
        Self {
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
        }
    }

    /// 提交成功：按提交顺序入队本帧的统计与发布基准。
    fn push(
        &mut self,
        index: FrameIndex,
        stats: RenderStats,
        samples: Vec<GpuTimerSample>,
        published_at: Instant,
    ) {
        self.submitted.push_back(SubmittedFrame {
            index,
            stats,
            samples,
            published_at,
            completed_at: None,
            presented_at: None,
        });
    }

    /// 完成通知到达：记录 GPU 采样与完成时刻；**不**发布（可能还在等更早的帧或呈现时刻）。
    fn complete(&mut self, index: FrameIndex, sample: Option<GpuTimerSample>) {
        let position = self.submitted.iter().position(|frame| frame.index == index);
        debug_assert!(
            position.is_some(),
            "completion for frame {} without a submission",
            index.get()
        );
        let Some(frame) = position.and_then(|position| self.submitted.get_mut(position)) else {
            return;
        };
        if let Some(sample) = sample {
            frame.samples.push(sample);
        }
        frame.completed_at = Some(Instant::now());
    }

    /// 呈现通知到达（`MTLDrawable.presentedTime`）：记下**真实呈现时刻**。
    ///
    /// 它通常晚于完成通知（GPU 先跑完、再由 CA 在下一个 vsync 上屏）。也可能落在反馈已经发布
    /// 之后（帧已出队）或极少数情况下早于提交记账（CA 在另一个线程）：两种情况都直接丢弃，
    /// 不能倒回去改已经发布的统计。
    fn presented(&mut self, index: FrameIndex, presented_at: Instant) {
        let Some(frame) = self.submitted.iter_mut().find(|frame| frame.index == index) else {
            return;
        };
        if frame.presented_at.is_none() {
            frame.presented_at = Some(presented_at);
        }
    }

    /// 是否还有"GPU 已完成、在等呈现通知"的帧：决定 [`Shared::wait_for_work`] 是否必须有上界。
    fn awaiting_presentation(&self) -> bool {
        self.submitted
            .iter()
            .any(|frame| frame.completed_at.is_some() && frame.presented_at.is_none())
    }

    /// `CompletionOrder` 的连续前缀推进到 `frontier`：按提交顺序弹出已完成帧并发布。
    ///
    /// 一帧在两种情况下发布：
    /// 1. 拿到了 `presentedTime`（真实呈现时刻，权威值）；
    /// 2. 完成之后已经等了 [`PRESENTED_WAIT`]（CA 可能永远不回调 —— 遮挡、drawable 被跳过、
    ///    或者这一帧被后来的 drawable 顶掉）⇒ 用完成时刻兜底，HUD 不会停在旧值。
    fn pop_ready(
        &mut self,
        frontier: FrameIndex,
        now: Instant,
        mut publish: impl FnMut(FrameFeedback),
    ) {
        while let Some(front) = self.submitted.front() {
            if front.index > frontier {
                break;
            }
            let Some(completed_at) = front.completed_at else {
                debug_assert!(false, "completion frontier passed an incomplete frame");
                break;
            };
            let presented_at = match front.presented_at {
                Some(presented_at) => presented_at,
                None if now.saturating_duration_since(completed_at) >= PRESENTED_WAIT => {
                    completed_at
                }
                // 还在等呈现通知：停在这里，保序（后面的帧也不能先发布）。
                None => break,
            };
            let frame = self.submitted.pop_front().expect("front exists");
            publish(FrameFeedback {
                index: frame.index,
                stats: frame.stats,
                samples: frame.samples,
                // UI 发布 → **呈现**（`presentedTime` 权威值；兜底时是 GPU 完成时刻）。
                latency: presented_at.saturating_duration_since(frame.published_at),
                // 真实呈现时刻（R09-6）：HUD 的 publish→present 不再是"完成通知被处理的时刻"。
                completed_at: presented_at,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use smol_str::SmolStr;

    use super::{
        AtlasSlots, FeedbackQueue, InFlight, JOIN_TIMEOUT, MAX_MISSED_PRESENTS,
        MAX_SURFACE_MISMATCHES, PRESENT_MARGIN, PRESENTED_WAIT, PanicGuard, PresentPacing,
        PresentPlan, SHUTDOWN_GRACE, SendCommandBuffer, Shared, StickyError, SurfaceMismatch,
        SurfaceVerdict, WaitOutcome, host_time_to_instant, join_with_deadline, pacing_paused,
        surface_within_tolerance,
    };
    use crate::{
        error::EngineError,
        paint::atlas::AtlasKind,
        render::{DirtyRows, RenderStats, compositor::Plan, frame::LatestFrame},
        window::FrameIndex,
    };

    fn stale_rows(y0: u32, y1: u32) -> DirtyRows {
        let mut rows = DirtyRows::default();
        rows.mark(y0, y1);
        rows
    }

    fn index(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    fn request(index: u32, surface: (u32, u32)) -> crate::render::frame::FrameRequest {
        crate::render::frame::FrameRequest {
            index: FrameIndex::from_raw(index),
            payload: crate::render::frame::FramePayload::empty(),
            plan: Plan::Full,
            surface,
            scale: 1.0,
            published_at: Instant::now(),
        }
    }

    fn stats(draw_calls: u32) -> RenderStats {
        RenderStats {
            draw_calls,
            instances: draw_calls * 10,
        }
    }

    #[test]
    fn a_first_atlas_upload_keeps_its_dirty_rows() {
        let mut slots = AtlasSlots::new();
        slots.queue(AtlasKind::Glyph, 64, vec![1, 2, 3], stale_rows(4, 8));
        let [glyph, image] = slots.take();
        let glyph = glyph.expect("glyph upload");
        assert_eq!(glyph.page, 64);
        assert_eq!(glyph.pixels, vec![1, 2, 3]);
        assert_eq!(glyph.rows.spans(), stale_rows(4, 8).spans());
        assert!(image.is_none());
        assert!(slots.take().iter().all(Option::is_none), "取走后槽位为空");
    }

    #[test]
    #[should_panic(expected = "atlas slot must be idle")]
    fn queueing_into_a_busy_atlas_slot_is_a_contract_violation() {
        // UI 线程用 `atlas_idle` 门控入队；覆盖未消费的上传会丢行，因此这里是硬失败。
        let mut slots = AtlasSlots::new();
        slots.queue(AtlasKind::Glyph, 64, vec![1, 2, 3], stale_rows(4, 8));
        slots.queue(AtlasKind::Glyph, 64, vec![9, 9, 9], stale_rows(16, 20));
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

    /// R09-6：display link 给的目标呈现时刻必须用 `presentDrawable:atTime:` 钉住。
    #[test]
    fn a_fresh_target_presentation_timestamp_is_pinned() {
        let mut pacing = PresentPacing::new();
        let target = 1_000.010;
        assert_eq!(
            pacing.plan(1_000.0, target),
            PresentPlan::AtTime(target),
            "还来得及提交：钉到目标呈现时刻"
        );
    }

    /// R09-6：目标时刻已经不够提交（CPU 落后）时**跳过本 vsync**，而不是提交一帧注定迟到的内容；
    /// 但连续错过有上界，否则画面会冻结。
    #[test]
    fn a_missed_target_skips_the_vsync_but_never_forever() {
        let mut pacing = PresentPacing::new();
        let target = 1_000.0;
        let late = target - PRESENT_MARGIN.as_secs_f64() / 2.0;
        for _ in 0..MAX_MISSED_PRESENTS {
            assert_eq!(
                pacing.plan(late, target),
                PresentPlan::Skip,
                "落后于目标时刻：跳过本 vsync 重新对齐"
            );
        }
        assert_eq!(
            pacing.plan(late, target),
            PresentPlan::Now,
            "连续错过 {MAX_MISSED_PRESENTS} 次之后退回立即呈现（不允许冻结画面）"
        );
        // 重新对齐之后又回到钉时刻。
        assert_eq!(
            pacing.plan(1_000.0, target + 0.010),
            PresentPlan::AtTime(target + 0.010)
        );
    }

    /// 系统不给目标时刻（headless / 0）：回到"尽快呈现"，与改动前行为一致。
    #[test]
    fn a_missing_target_timestamp_presents_immediately() {
        for target in [0.0, f64::NAN, f64::INFINITY, -1.0] {
            let mut pacing = PresentPacing::new();
            assert_eq!(pacing.plan(123.0, target), PresentPlan::Now, "{target:?}");
        }
    }

    /// `presentedTime` 是 host time（秒）：换算成 `Instant` 之后必须能当作反馈的呈现时刻。
    #[test]
    fn host_time_is_converted_into_the_monotonic_clock() {
        let observed = Instant::now();
        let presented_host = 500.0;
        let now_host = presented_host + 0.010;
        let presented = host_time_to_instant(presented_host, observed, now_host).expect("instant");
        assert!(presented <= observed, "呈现时刻不可能晚于观测时刻");
        let age = observed.saturating_duration_since(presented);
        assert!(
            (Duration::from_millis(9)..=Duration::from_millis(11)).contains(&age),
            "10 ms 前呈现：换算误差 {age:?}"
        );
        // 非法值（MLA 没给 / 被跳过 / NaN）：返回 None，调用方退回完成时刻。
        assert!(host_time_to_instant(0.0, observed, now_host).is_none());
        assert!(host_time_to_instant(f64::NAN, observed, now_host).is_none());
        // 未来时刻（同一时基下不该出现）：按"刚刚"处理而不是溢出。
        assert_eq!(
            host_time_to_instant(now_host + 5.0, observed, now_host),
            Some(observed)
        );
    }

    /// R09-6：拿到 `presentedTime` 时，反馈的 `completed_at` / `latency` 必须用**真实呈现时刻**，
    /// 而不是"完成通知被处理的时刻"（旧实现让 HUD 少报 8~17 ms）。
    #[test]
    fn presented_time_overrides_the_completion_time() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(100);
        feedback.push(index(1), stats(1), Vec::new(), published_at);
        feedback.complete(index(1), None);
        let presented_at = Instant::now();
        feedback.presented(index(1), presented_at);
        assert!(!feedback.awaiting_presentation(), "呈现时刻已到，不必再等");
        let mut published = Vec::new();
        feedback.pop_ready(index(1), Instant::now(), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].completed_at, presented_at,
            "completed_at 必须是 presentedTime 换算出的呈现时刻"
        );
        assert_eq!(
            published[0].latency,
            presented_at.saturating_duration_since(published_at),
            "latency = 发布 → 呈现"
        );
    }

    /// 呈现通知可能永远不来（窗口被遮挡 / drawable 被跳过）：等满 [`PRESENTED_WAIT`] 之后必须
    /// 用完成时刻兜底发布，HUD 不能停在旧值。
    #[test]
    fn a_frame_without_a_presented_time_is_published_after_the_wait() {
        let mut feedback = FeedbackQueue::new();
        feedback.push(index(1), stats(1), Vec::new(), Instant::now());
        feedback.complete(index(1), None);
        assert!(feedback.awaiting_presentation(), "在等呈现通知");
        let mut published = Vec::new();
        // 还没等满：不能发布（保序，后面的帧也不能先出）。
        feedback.pop_ready(index(1), Instant::now(), |frame| published.push(frame));
        assert!(published.is_empty(), "等呈现通知期间不发布");
        assert_eq!(feedback.submitted.len(), 1);
        // 等满之后兜底发布，completed_at 退回完成时刻。
        feedback.pop_ready(index(1), Instant::now() + PRESENTED_WAIT * 2, |frame| {
            published.push(frame)
        });
        assert_eq!(published.len(), 1);
        assert!(feedback.submitted.is_empty());
    }

    /// 呈现通知可能落在反馈已经发布之后（甚至极少数情况下早于提交记账）：两种情况都必须
    /// 静默丢弃，不能 panic（CA 在另一个线程回调）。
    #[test]
    fn a_presented_notification_for_an_unknown_frame_is_ignored() {
        let mut feedback = FeedbackQueue::new();
        feedback.presented(index(9), Instant::now());
        assert!(feedback.submitted.is_empty());
        assert!(!feedback.awaiting_presentation());
    }

    #[test]
    fn drawable_size_tolerance_absorbs_one_pixel_of_dpi_rounding() {
        assert!(surface_within_tolerance((1400, 1040), (1400, 1040)));
        assert!(
            surface_within_tolerance((1400, 1040), (1401, 1039)),
            "±1 px 是 DPI 取整，不值得再等一个 vsync"
        );
        assert!(
            !surface_within_tolerance((1400, 1040), (1402, 1040)),
            "差 2 px 已经不是取整"
        );
        assert!(!surface_within_tolerance((1400, 1040), (1400, 1042)));
        assert!(
            !surface_within_tolerance((1400, 1040), (2500, 1700)),
            "上一轮 layout 的 drawable 必须被拒绝（P1）"
        );
    }

    #[test]
    fn one_pixel_rounding_never_degrades() {
        let mut surface = SurfaceMismatch::new();
        for _ in 0..(MAX_SURFACE_MISMATCHES * 3) {
            assert_eq!(
                surface.observe(Some((1400, 1040)), (1401, 1039)),
                SurfaceVerdict::Ready,
                "±1 px 的取整差必须直接绘制"
            );
            assert!(!surface.degraded);
        }
    }

    #[test]
    fn surface_degradation_is_sticky_until_the_sizes_match_again() {
        let mut surface = SurfaceMismatch::new();
        for _ in 0..MAX_SURFACE_MISMATCHES {
            assert_eq!(
                surface.observe(Some((1400, 1040)), (1402, 1040)),
                SurfaceVerdict::Skip,
                "前 MAX_SURFACE_MISMATCHES 个 vsync 等新 drawable"
            );
            assert!(!surface.degraded);
        }
        assert_eq!(
            surface.observe(Some((1400, 1040)), (1402, 1040)),
            SurfaceVerdict::Degraded
        );
        // 粘性：持续 >1 px 不一致时**每个** vsync 都退化绘制，不再跳 4 个画 1 个。
        for _ in 0..10 {
            assert_eq!(
                surface.observe(Some((1400, 1040)), (1402, 1040)),
                SurfaceVerdict::Degraded
            );
        }
        // 尺寸重新在容差内：清除退化并归零。
        assert_eq!(
            surface.observe(Some((1400, 1040)), (1401, 1039)),
            SurfaceVerdict::Ready
        );
        assert!(!surface.degraded);
        assert_eq!(surface.consecutive, 0);
        // 新的一次不一致重新计数（不继承上一个 episode 的退化）。
        assert_eq!(
            surface.observe(Some((1400, 1040)), (1402, 1040)),
            SurfaceVerdict::Skip
        );
    }

    #[test]
    fn a_stale_drawable_cannot_consume_the_pending_frame() {
        let shared = Shared::new(Arc::new(InFlight::new()));
        shared.publish_frame(request(1, (1800, 1400)));
        assert!(
            shared
                .take_frame_for_drawable((1920, 1556), false)
                .is_none(),
            "陈旧 drawable 不能消费最新帧（P1）"
        );
        assert!(shared.has_pending_frame(), "帧必须留在槽里等新 drawable");
        assert_eq!(
            shared.published_surface(),
            Some((1800, 1400)),
            "取帧失败不能清掉目标尺寸"
        );
        let frame = shared
            .take_frame_for_drawable((1801, 1399), false)
            .expect("1 px 取整差仍在容差内");
        assert_eq!(frame.index.get(), 1);
        assert!(!shared.has_pending_frame());
        assert_eq!(
            shared.published_surface(),
            None,
            "取帧与 published_surface 必须在同一把锁里清空"
        );
    }

    #[test]
    fn the_degraded_path_can_force_consume_a_mismatched_frame() {
        let shared = Shared::new(Arc::new(InFlight::new()));
        shared.publish_frame(request(7, (1800, 1400)));
        let frame = shared
            .take_frame_for_drawable((1920, 1556), true)
            .expect("退化路径按 drawable 尺寸消费帧");
        assert_eq!(frame.index.get(), 7);
        assert_eq!(frame.surface, (1800, 1400), "请求本身的目标尺寸不改");
        assert_eq!(shared.published_surface(), None);
    }

    /// 背景色是 condvar 的等待条件之一：没有它，`set_background` 的唤醒会被当成伪唤醒睡回去。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_pending_background_color_wakes_the_gpu_loop() {
        let shared = Shared::new(Arc::new(InFlight::new()));
        assert!(!shared.lock().has_work(0), "空闲：没有工作");
        shared.set_background([0.25, 0.5, 0.75, 1.0]);
        assert_eq!(
            shared.wait_for_work(false),
            WaitOutcome::Work,
            "待应用背景色必须让 GPU 线程从等待中返回"
        );
        assert_eq!(shared.take_background(), Some([0.25, 0.5, 0.75, 1.0]));
        assert_eq!(shared.take_background(), None, "取一次清空");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_background_change_after_shutdown_or_termination_is_dropped() {
        let shared = Shared::new(Arc::new(InFlight::new()));
        shared.request_shutdown();
        shared.set_background([1.0, 0.0, 0.0, 1.0]);
        assert_eq!(
            shared.take_background(),
            None,
            "GPU 线程在应用背景色之前已退出"
        );

        let terminated = Shared::new(Arc::new(InFlight::new()));
        terminated.mark_terminated();
        terminated.set_background([0.0, 1.0, 0.0, 1.0]);
        assert_eq!(terminated.take_background(), None);
    }

    /// 完成回调乱序时，反馈必须按**提交顺序**发布：第 2 帧先完成不能先发布，第 1 帧完成后
    /// 连续前缀推进，按 1 → 2 发布；每帧的 latency 用它自己的发布基准。
    #[test]
    fn out_of_order_completions_publish_in_submission_order() {
        let mut feedback = FeedbackQueue::new();
        let base = Instant::now();
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            base - Duration::from_millis(100),
        );
        feedback.push(index(2), stats(2), Vec::new(), base);
        feedback.push(index(3), stats(3), Vec::new(), base);
        // 第 2、3 帧先完成：连续前缀（第 1 帧）未到，不能发布。
        feedback.complete(index(2), None);
        feedback.complete(index(3), None);
        let mut published = Vec::new();
        // 第 1 帧完成：`CompletionOrder` 的前沿一次推进到 3，按提交顺序发布 1、2、3。
        feedback.complete(index(1), None);
        // 已经等满呈现通知的上界：三帧都用完成时刻兜底发布（发布顺序不受影响）。
        let ready_at = Instant::now() + PRESENTED_WAIT * 2;
        feedback.pop_ready(index(3), ready_at, |frame| published.push(frame));

        assert_eq!(
            published
                .iter()
                .map(|frame| frame.index.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "反馈按提交顺序发布"
        );
        assert_eq!(published[0].stats.draw_calls, 1);
        assert_eq!(published[1].stats.draw_calls, 2);
        assert_eq!(published[2].stats.draw_calls, 3);
        assert!(
            published[0].latency >= Duration::from_millis(90),
            "第 1 帧的 latency 用自己的发布基准，不是更旧帧的完成时刻"
        );
        assert!(
            published[1].latency < Duration::from_millis(90),
            "第 2 帧不能被更旧帧的 latency 污染"
        );
        assert!(feedback.submitted.is_empty(), "发布后队列清空");
    }

    #[test]
    fn shutdown_wait_is_bounded_when_completions_never_arrive() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.in_flight.preset(1);
        shared.request_shutdown();
        let worker = Arc::clone(&shared);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(worker.wait_for_work(false));
        });
        let started = Instant::now();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(6)),
            Ok(WaitOutcome::TimedOut),
            "驱动永不回调完成时 GPU 线程自己的等待必须有界（detach 后它也要能退出）"
        );
        assert!(started.elapsed() >= SHUTDOWN_GRACE, "必须等满 grace 才放弃");
    }

    /// 正常关闭：GPU 线程已经退出时 `join` 立即返回，主线程不白等 [`JOIN_TIMEOUT`]。
    #[test]
    fn a_finished_gpu_thread_is_joined_immediately() {
        let thread = std::thread::spawn(|| {});
        let started = Instant::now();
        join_with_deadline(thread);
        assert!(
            started.elapsed() < JOIN_TIMEOUT,
            "已退出的线程不能被超时拖住：{:?}",
            started.elapsed()
        );
    }

    /// P2-7 回归：GPU 线程卡在驱动里永不返回时，主线程（AppKit 事件循环）必须在
    /// [`JOIN_TIMEOUT`] 附近放弃等待（detach），不能无限阻塞窗口关闭 / 应用退出。
    #[test]
    fn a_hung_gpu_thread_is_detached_after_the_bounded_wait() {
        // 用一个挂住的线程模拟「驱动没有回调完成、线程永不返回」。
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(60));
        });
        let started = Instant::now();
        join_with_deadline(thread);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= JOIN_TIMEOUT,
            "必须等满上限才放弃 join：{elapsed:?}"
        );
        assert!(
            elapsed < JOIN_TIMEOUT + Duration::from_secs(5),
            "detach 等待必须有界（不能退化成永久 join）：{elapsed:?}"
        );
        // 放掉挂住的线程，避免它陪测试进程一起跑到超时。
        let _ = release.send(());
    }

    /// F-169 的「terminated-only」语义：关闭期间必须继续收完成通知（关闭握手靠它把在飞帧
    /// drain 到 0），只有 GPU 线程已经退出（`terminated`）之后的通知才可以丢弃。
    #[test]
    fn completion_notifications_are_dropped_only_after_termination() {
        use objc2_metal::{MTLCommandQueue, MTLDevice};

        use crate::render::macos::device::Device;

        let shared = Shared::new(Arc::new(InFlight::new()));
        let device = Device::new().expect("metal device");
        let queue = device.raw().newCommandQueue().expect("command queue");
        let buffer = || SendCommandBuffer(queue.commandBuffer().expect("command buffer"));
        shared.request_shutdown();
        shared.in_flight.preset(2);
        shared.push_completion(index(1), buffer());
        let completions = shared.take_completions();
        assert_eq!(
            completions.len(),
            1,
            "shutdown 期间的完成通知必须入队（关闭握手 drain 在飞帧）"
        );
        assert_eq!(shared.in_flight(), 1, "drain 必须按完成数减少在飞帧");

        shared.mark_terminated();
        shared.push_completion(index(2), buffer());
        assert!(
            shared.take_completions().is_empty(),
            "terminated 之后不再入队（消费者已退出，继续入队只会泄漏命令缓冲）"
        );
    }

    /// 离屏路径（快照 / golden）仍是**同步**语义，并且与窗口路径共用同一套 GPU 对象：
    /// 这是 Metal 后端唯一能在无窗口会话里跑通的端到端 GPU 检查。
    #[test]
    fn offscreen_snapshot_round_trips_pixels() {
        use crate::{
            math::Scale,
            paint::{color::ColorResolver, instance::InstanceEncoder, scene::Scene},
        };

        let mut renderer =
            super::Renderer::new_offscreen((16, 8)).expect("metal offscreen renderer");
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder
            .encode(
                &Scene::empty(),
                Scale::ONE,
                &ColorResolver,
                &crate::paint::geometry_store::GeometryStore::new(),
            )
            .expect("encode");
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

    /// 与 Linux / Windows 的同构测试：GPU 线程 panic 后 `poll()` 必须看到一条粘性错误，
    /// 而不是静默停摆。
    #[test]
    fn a_panicking_gpu_thread_publishes_an_error() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
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
                assert_eq!(detail, "kytrix-metal thread panicked");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            !shared.shutdown_requested(),
            "a panic only terminates the thread, it does not fake a shutdown"
        );
    }

    /// panic 只填补**空**错误槽：循环已经发布的错误是更具体的原因，不能被覆盖。
    #[test]
    fn a_panic_keeps_an_already_published_error() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.publish_error(EngineError::DeviceLost {
            reason: SmolStr::new_static("MTLCommandBufferError(11)"),
            log_path: std::path::PathBuf::from("logs/metal.txt"),
        });
        let worker = Arc::clone(&shared);
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PanicGuard::new(worker);
            panic!("injected gpu thread panic");
        }));
        std::panic::set_hook(hook);
        assert!(panicked.is_err(), "the guard must run during unwinding");

        match shared.take_error().expect("first error wins") {
            EngineError::DeviceLost { reason, .. } => {
                assert_eq!(reason, "MTLCommandBufferError(11)");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
