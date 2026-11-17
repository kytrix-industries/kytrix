//! 窗口渲染门面 + 每窗口一条 `kytrix-d3d12` GPU 线程
//! （`design/async-render-pipeline.md` §4.5 / §6）。
//!
//! ```text
//! UI 线程                                  kytrix-d3d12 线程（每窗口）
//! ────────────────────────────────────    ──────────────────────────────────────────
//! 窗口消息（WM_SIZE 只记录尺寸）           循环：等 work / completion → 空转或取帧
//! publish(FrameRequest) ──► 最新帧槽            → 等 waitable（呈现背压，每帧恰好一次）
//! queue_atlas_upload ─────► 图集槽              → 取「此刻最新」帧 + 图集槽
//! poll() ◄──────────────── 反馈槽               → 内容尺寸变化则 set_content_size
//!                                               （桶内纯 CPU；越桶则 ResizeBuffers）
//!                                               → 遮挡 / 最小化 ⇒ 不取帧（R08-7）
//!                                               → 槽 fence → 录制 → Execute → Signal
//!                                               → Present1(sync, dirty)（有更新帧时 sync=0）
//! WM_APP_COMPOSITION_WAKE ◄─ PostMessage       → 完成观测点：retire + 反馈 + presented 尺寸
//! ```
//!
//! 反馈与 presented 尺寸都在**完成观测点**发布（fence 的 `SetEventOnCompletion` 唤醒），不是
//! `Present1` 返回的时刻；retire 与反馈同一观测点推进（§4.1 / §6.5）。`presented` 尺寸变了才
//! post 一次合成唤醒：UI 线程据此更新 DirectComposition 的内容裁剪 / 背景缩放（`poll` 每轮都会
//! 重算几何，所以静止时不需要额外消息）。内容 visual 恒 1:1，永远不因 resize 被缩放。
//!
//! 三条硬约束：
//! - **I1**：UI 线程只做「加锁拷贝 + `SetEvent`」，没有 fence wait / `Present`。
//! - **I2**：`ExecuteCommandLists` 后立即返回；完成通知只通过 fence（资源按序号延迟释放，I4）。
//! - **I3**：节奏来自 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 句柄，
//!   只在 GPU 线程等待，且每帧（含首帧）恰好等一次。
//!
//! GPU 对象分两层（见 `design/phase-08-d3d12.md` §3.1 与 `device.rs`）：device / direct queue /
//! 根签名 / PSO / 图集纹理是**进程级**共享的（`device::SharedGpu` 的 `OnceLock`，第一个窗口
//! 创建时初始化）；每个窗口只持有自己的 `GpuRenderer`（swapchain / DirectComposition 对象 /
//! persist / 提交流 / fence 与帧槽），并把它整体移交给自己的 `kytrix-d3d12` 线程。
//!
//! 关窗握手（§6.7）：UI 线程**不能**直接 `join` —— DXGI 的 `Present` 可能同步 `SendMessage`
//! 到窗口的消息泵线程，直接 join 会和「消息泵停转」互等。`Drop` 用 `MsgWaitForMultipleObjects`
//! 边泵消息边等 GPU 线程句柄：常规路径下线程很快退出，但驱动挂死时**有界放弃**（累计 10 s
//! 后 detach 并记录诊断），窗口关闭/应用退出不会永久卡住。

use std::{
    collections::VecDeque,
    os::windows::io::AsRawHandle,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smol_str::{SmolStr, format_smolstr};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM},
    System::Threading::{
        CreateEventW, GetCurrentThreadId, INFINITE, SetEvent, WaitForMultipleObjects,
    },
    UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostMessageW,
        PostQuitMessage, PostThreadMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
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
    platform::{
        dispatcher::WM_APP_COMPOSITION_WAKE,
        windows::composition::{self, Composition},
    },
    render::{
        DirtyRows, RenderStats,
        color::background_linear,
        compositor::{FramePlan, Plan},
        frame::{FrameFeedback, FrameRequest, FrameSink, LatestFrame, MAX_FRAMES_IN_FLIGHT},
        timer::{FrameTimings, GpuTimerSample},
        windows::{
            compositor::{FrameSubmitError, GpuRenderer, enter_gpu_thread},
            device::init_error_from,
            diagnostics::Diagnostics,
            queue::WaitOutcome,
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

    /// 测试用 WARP 离屏渲染器（图集页尺寸由上传的整页像素长度决定）。
    #[cfg(feature = "test-support")]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_warp_offscreen(size)?),
            error: None,
        })
    }

    /// 真实窗口：构造 GPU 对象（UI 线程）后整体移交给 `kytrix-d3d12` 线程。
    ///
    /// `composition` 与 `NativeWindow` 共享：内容交换链与 1×1 背景交换链各 prime 一帧背景色
    /// （GPU 线程启动之前、`ShowWindow` 之前）并挂到合成树上；此后 GPU 线程只在内容越过 buffer
    /// 时 `ResizeBuffers`（R08-1：交换链对象不变、visual 不动）、在背景色变化时换掉 1×1 背景层
    /// （`GpuRenderer::refresh_background`），其余 DComp 调用都在 UI 线程，互斥锁串行化。
    ///
    /// # Errors
    /// 设备 / 组合交换链创建失败，或 GPU 线程无法启动（`AppInit` / `Composition` / `Swapchain`）。
    pub(crate) fn new_for_hwnd(
        composition: Arc<Mutex<Composition>>,
        size: (u32, u32),
        background: Color,
    ) -> Result<Self, EngineError> {
        // 合成通知（`WM_APP_COMPOSITION_WAKE`）的目标是 UI 线程的窗口：在把 `Arc` 交给 GPU 线程
        // 之前取一次句柄（`Shared` 只存 `isize`，不持有 `Composition`）。
        let hwnd = composition::lock(&composition).hwnd_raw();
        let gpu =
            GpuRenderer::new_for_composition(composition, size, background_linear(background))?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(gpu, size, hwnd)?),
            error: None,
        })
    }

    /// 最近一次**发布**的目标尺寸（UI 侧记录；GPU 线程消费它做内容尺寸记账，§4.4）。
    pub(crate) fn surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.surface,
        }
    }

    /// 最近一次**完成 present** 的内容尺寸（Windows：DirectComposition 几何的唯一输入）。
    pub(crate) fn presented_surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.presented_surface(),
        }
    }

    /// 取走「合成几何可能过期」的兜底标记（UI 线程的泵在 ②b′ 消费）。
    ///
    /// 只有窗口目标有合成几何；离屏目标恒 `false`。标记只会在 GPU 线程的两次唤醒都失败时
    /// 置位（`Shared::notify_ui`）。
    pub(crate) fn take_composition_pending(&self) -> bool {
        match &self.target {
            RenderTarget::Offscreen(_) => false,
            RenderTarget::Window(window) => window.take_composition_pending(),
        }
    }

    /// 后端运行期计数器（R08-3 / R08-7 / R08-13 的可观测面；HUD / 日志 / 测试共用）。
    pub(crate) fn diagnostics(&self) -> Diagnostics {
        crate::render::windows::diagnostics::snapshot()
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
                gpu.set_scale(request.scale);
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

    /// 改窗口背景色（UI 线程只写共享槽 + `wake`，clear color 与 1×1 背景层在 GPU 线程落地）。
    ///
    /// 组合交换链没有可用的 `SetBackgroundColor`：背景色是持久目标的 clear color（linear），
    /// 与 macOS / Linux 同一条路径；Windows 另外用它重建独立背景层，拖动中露出的新区域立即
    /// 是这个颜色（DEVIATIONS §36 F-203 / F-205）。离屏目标没有背景色（从不 present），是 no-op。
    pub(crate) fn set_background_color(&mut self, color: Color) {
        match &self.target {
            RenderTarget::Offscreen(_) => {}
            RenderTarget::Window(window) => {
                window.set_background_color(background_linear(color));
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
                // 后端失败时把累计计数一并写日志（R08-3 / R08-7 的现场数据；诊断不参与决策）。
                eprintln!(
                    "kytrix: d3d12 backend diagnostics after error: {}",
                    self.diagnostics().describe()
                );
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
    Composition { detail: SmolStr },
    AppInit { detail: SmolStr },
    GeometryRegister { detail: SmolStr },
}

impl StickyError {
    fn capture(error: EngineError) -> Self {
        match error {
            EngineError::DeviceLost { reason, log_path } => Self::DeviceLost { reason, log_path },
            EngineError::Swapchain { detail } => Self::Swapchain { detail },
            EngineError::Composition { detail } => Self::Composition { detail },
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
            Self::Composition { detail } => EngineError::Composition {
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
    fn new(gpu: GpuRenderer, size: (u32, u32), hwnd: isize) -> Result<Self, EngineError> {
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
        let surface = (size.0.max(1), size.1.max(1));
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work,
            completion,
            hwnd,
            // SAFETY: 只查询调用线程的 id；本构造发生在 UI 线程（GPU 线程随后才 spawn）。
            thread: unsafe { GetCurrentThreadId() },
            presented: AtomicU64::new(pack_size(surface)),
            composition_pending: AtomicBool::new(false),
            atlas_pending: [AtomicBool::new(false), AtomicBool::new(false)],
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
            surface,
            stats: RenderStats::default(),
            #[cfg(feature = "hud")]
            samples: Vec::with_capacity(3),
            last_published: None,
        })
    }

    /// 最近一次**完成 present** 的内容尺寸（GPU 线程在完成观测点发布）。
    fn presented_surface(&self) -> (u32, u32) {
        unpack_size(self.shared.presented.load(Ordering::Acquire))
    }

    /// 合成几何兜底标记（UI 线程在泵的 ②b′ 消费）。
    fn take_composition_pending(&self) -> bool {
        self.shared.take_composition_pending()
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
        page: u32,
        pixels: &[u8],
        rows: DirtyRows,
    ) -> bool {
        self.shared.queue_atlas(kind, page, pixels, rows)
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.shared.atlas_idle(kind)
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
        // 共享契约（B6-2）：`presented_at` 是**真实上屏**时刻，不可能早于 GPU 完成；
        // 三端都读它，HUD 才能用同一字段区分「发布→完成」与「发布→上屏」。
        debug_assert!(
            feedback
                .presented_at
                .is_none_or(|presented| presented >= feedback.completed_at),
            "presented_at must not precede completed_at"
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
    /// 窗口句柄（`WM_APP_COMPOSITION_WAKE` 的目标；UI 线程的 `NativeWindow` 晚于本结构释放）。
    hwnd: isize,
    /// 主线程 id：窗口消息投不进去时 `notify_ui` 退回 `PostThreadMessageW`（不能丢最后 1:1）。
    thread: u32,
    /// 最近一次**完成 present** 的内容尺寸（`width << 32 | height`；`Acquire`/`Release` 配对）。
    presented: AtomicU64,
    /// 两次唤醒都失败时的兜底标记：UI 线程在泵的几何 pass 里消费它并重算一次几何。
    composition_pending: AtomicBool,
    /// 每 `AtlasKind` 的共享槽是否正在被 GPU 线程消费（`false` = UI 可以入队）。
    atlas_pending: [AtomicBool; 2],
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

    fn queue_atlas(&self, kind: AtlasKind, page: u32, pixels: &[u8], rows: DirtyRows) -> bool {
        let index = kind.index() as usize;
        // 像素在**锁外**复制：整页可达 16 MiB，不能让 GPU 线程的取帧锁等这次 memcpy。
        let pixels = pixels.to_vec();
        // 先占位再入队：GPU 线程 take 之后才释放，UI 线程据此不会覆盖未消费的上传
        // （DEVIATIONS F-211）。标志在**录制完成**后才清（`atlas_uploaded`），
        // 因此「空闲」= 上一笔已经进了命令列表，不只是「被 GPU 线程拿走」。
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

    /// 发布「最近一次完成 present 的内容尺寸」；尺寸变化时唤醒 UI 线程重算合成几何。
    ///
    /// 尺寸没变就不 post：几何只依赖这个尺寸，动画帧不该让泵多跑一轮。
    fn publish_presented(&self, surface: (u32, u32)) {
        let packed = pack_size(surface);
        if self.presented.swap(packed, Ordering::AcqRel) != packed {
            self.notify_ui();
        }
    }

    /// 让 UI 线程多跑一轮泵（`poll_renderers` + ②b′ DComp 几何）。
    ///
    /// 只是通知，不改变 GPU 侧状态：`Shared::presented` 才是待处理状态本身，UI 线程每轮泵迭代
    /// 都会读它并重算几何（同值 no-op），所以消息丢失最多让 1:1 晚一轮。窗口消息投不进去
    /// （队列满 / 句柄已失效）时退回线程消息；**两种投递都失败**时置
    /// `Shared::composition_pending`，泵下一次因任何其它原因醒来时会消费它并强制重算几何。
    fn notify_ui(&self) {
        // SAFETY: `hwnd` 由 UI 线程的 `NativeWindow` 持有且晚于 GPU 线程释放；`PostMessageW`
        // 只入队，不解引用其它指针（句柄失效时返回错误）。
        let posted = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut core::ffi::c_void)),
                WM_APP_COMPOSITION_WAKE,
                WPARAM(0),
                LPARAM(0),
            )
        };
        if posted.is_ok() {
            return;
        }
        if self.thread != 0 {
            // SAFETY: `thread` 是 UI 线程 id（构造时由 `GetCurrentThreadId` 取得）；只入队消息。
            let posted = unsafe {
                PostThreadMessageW(self.thread, WM_APP_COMPOSITION_WAKE, WPARAM(0), LPARAM(0))
            };
            if posted.is_ok() {
                return;
            }
        }
        // 两次唤醒都失败（队列满时通常还有别的消息在队列里；线程已退出时泵也不再需要几何）：
        // 留下标记，UI 线程下一次泵迭代消费它并重算几何，最后的 1:1 不会丢。
        self.composition_pending.store(true, Ordering::Release);
    }

    /// 取走「合成几何可能过期」的兜底标记（UI 线程，泵的 ②b′ 消费）。
    fn take_composition_pending(&self) -> bool {
        self.composition_pending.swap(false, Ordering::AcqRel)
    }

    fn has_pending_frame(&self) -> bool {
        self.lock().frame.has_pending()
    }

    fn take_frame(&self) -> Option<FrameRequest> {
        self.lock().frame.take()
    }

    fn take_atlas(&self) -> [Option<AtlasUpload>; 2] {
        // 这里**不**清 `atlas_pending`：GPU 线程只是把上传拿走，还没录进命令列表。
        // 清得太早，UI 会在本帧未录制（Retry / Idle）时入队下一笔，覆盖掉这一笔（丢行）。
        self.lock().atlas.take()
    }

    /// 本帧的图集上传已经录进命令列表：共享槽这才算空（UI 可以入队下一笔）。
    fn atlas_uploaded(&self, kind: AtlasKind) {
        self.atlas_pending[kind.index() as usize].store(false, Ordering::Release);
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        !self.atlas_pending[kind.index() as usize].load(Ordering::Acquire)
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
    /// 本帧 present 的内容尺寸（完成时发布给 UI 线程的合成几何）。
    surface: (u32, u32),
}

/// GPU 线程主循环（§6.3）。
///
/// 外层只做一件事（R08-4）：`run_gpu_loop` 以 `DeviceLost` 退出时先尝试**进程内恢复**
/// （重建 device / queue / PSO / 图集 + 本窗口的 swapchain / persist / 提交流），恢复成功就继续
/// 出帧，失败才把粘性错误交给 UI。一次 TDR / 驱动更新因此不再需要重启应用。
fn gpu_loop(renderer: GpuRenderer, shared: Arc<Shared>) {
    enter_gpu_thread();
    // panic 出路：展开时把错误交给 UI（`poll()` 不会静默卡死），并标记线程已终止。
    let guard = PanicGuard::new(Arc::clone(&shared));
    // `GpuRenderer` 必须是**局部** `Option`：循环结束后显式 drop（等 GPU 完成、丢掉 back buffer
    // 引用，再销毁 swapchain / 设备），`join` 返回时 teardown 一定已经完成（§6.7）。
    let mut renderer = Some(renderer);
    let mut submitted = VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT);
    loop {
        let result = run_gpu_loop(
            renderer
                .as_mut()
                .expect("renderer is present until teardown"),
            &shared,
            &mut submitted,
        );
        match result {
            Ok(()) => break,
            Err(error) => {
                if !matches!(error, EngineError::DeviceLost { .. }) {
                    shared.publish_error(error);
                    break;
                }
                // 设备真的被移除了：先尝试一次进程内恢复（限次在 `SharedGpu::recover` 里）。
                submitted.clear();
                let current = renderer.take().expect("renderer is present until teardown");
                match current.recover() {
                    Ok(recovered) => {
                        crate::render::windows::diagnostics::device_recovered();
                        renderer = Some(recovered);
                        if shared.shutdown_requested() {
                            break;
                        }
                    }
                    Err(fatal) => {
                        shared.publish_error(fatal);
                        break;
                    }
                }
            }
        }
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
    // 已从共享槽取走、但还没被本帧命令列表消费的图集上传（按 `AtlasKind`）。共享槽标志
    // 要等这里清，UI 才不会在 `Retry` / Idle 帧覆盖掉未录制的上传（DEVIATIONS F-211）。
    let mut atlas_awaiting = [false; 2];
    loop {
        // 完成通知的唯一处理点：按提交顺序发布反馈 + 推进 retire（§4.1 / §6.5）。
        drain_completed(renderer, shared, submitted);
        if shared.shutdown_requested() {
            return Ok(());
        }
        // 空闲时也要应用待定背景色：改色不要求提交一帧（UI 只写共享槽 + 唤醒）。背景色同时
        // 落在 1×1 背景层上（`refresh_background`），所以不画内容也能立刻看到新底色。
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba);
            renderer.refresh_background()?;
        }
        // 遮挡 / 最小化（R08-7）：不取帧、不提交、不 present。帧**留在**最新帧槽里，damage 继续
        // 合并 —— 恢复可见时一帧就能补上全部变化。这里只睡在 `[work, completion]` 上（定时唤醒
        // 一次以重查可见性 / 关闭），不消耗 waitable token。
        if renderer.window_occluded() {
            crate::render::windows::diagnostics::occluded_skipped();
            wait_for_work_or_completion(shared, Some(OCCLUDED_POLL_INTERVAL))?;
            continue;
        }
        if !shared.has_pending_frame() {
            // 空闲：睡在 [work, completion] 上（完成由 fence 的 `SetEventOnCompletion` 明确唤醒，
            // 不轮询；没有待画帧时也不等 waitable，I3 的「每帧恰好一次」）。
            wait_for_work_or_completion(shared, None)?;
            continue;
        }
        // I3：先等呈现节奏，再取「此刻最新」的帧 —— 等待期间 UI 的发布覆盖旧请求并合并 damage，
        // 渲染的是最新快照而不是最多等了一个 vsync 的旧快照（§4.2 的「帧是状态不是队列」）。
        if !wait_for_waitable(renderer, shared, submitted)? {
            // 没消费到 token（关闭 / 遮挡 / 超时后重查）：回到循环顶重新决策。
            continue;
        }
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
        // 取帧之后、可能 `resize_target` 之前再排空一次完成通知：`resize_target` 会 `wait_idle`，
        // 未 drain 的帧的 timestamp 读回缓冲会被后续录制覆盖（`ResolveQueryData` 的写入与 CPU
        // 读回共用同一块缓冲 → GPU 采样错帧）。
        drain_completed(renderer, shared, submitted);
        // 等 waitable 期间到达的背景色：与帧一起落地（在录制前换掉 1×1 背景层，不引入新的
        // GPU 调用点）。
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba);
            renderer.refresh_background()?;
        }
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2；像素按所有权移交给 GPU）。
        for (index, upload) in shared.take_atlas().into_iter().enumerate() {
            if let Some(upload) = upload {
                atlas_awaiting[index] = true;
                renderer.queue_atlas_upload(upload.kind, upload.page, upload.pixels, upload.rows);
            }
        }
        // 内容尺寸变化：buffer 内是纯 CPU 记账（内容 visual 恒 1:1；放大露出的区域由 1×1
        // 背景层填充，UI 只改内容 clip / 背景 scale）；越过高水位 buffer 时在这里重建持久目标
        // 并 `ResizeBuffers`（DEVIATIONS §36 F-199 / R08-1），重建失败（含内容 > 16384）以粘性
        // 错误退出，不静默裁剪。
        renderer.set_content_size(request.surface)?;
        // DPI 与 `surface` 同帧过线：geometry 通道的顶点是逻辑像素，root uniform 的
        // `frame.params.x` 必须与这一帧的 surface 成对（contract §6）。
        renderer.set_scale(request.scale);
        // 本帧提交时已经有更新的帧待画 ⇒ `Present1(sync = 0)` 替换队列里的帧（R08-9 方案 C ②）。
        let replace = shared.has_pending_frame();
        let mut draw_plan = request.plan;
        let mut attempts = 0u32;
        let mut full_records = 0u32;
        loop {
            let slot = renderer.acquire_slot();
            match renderer.wait_slot_fence(slot)? {
                WaitOutcome::Signaled => {}
                WaitOutcome::TimedOut => {
                    // 设备健康但该槽的上一帧还没完成（R08-3）：换下一个槽；全部槽都在飞就跳过
                    // 这一帧（不提交、不 present，也不销毁任何资源）。UI 的下一帧会带来新的
                    // damage，帧槽的 damage 合并保证不丢更新。
                    attempts += 1;
                    if attempts >= MAX_FRAMES_IN_FLIGHT as u32 {
                        break;
                    }
                    continue;
                }
            }
            // 该槽的上一帧刚被等到：在录制覆盖它的 timestamp 读回缓冲**之前**发布它的反馈
            // （`ResolveQueryData` 的写入与 CPU 读回共用这一块缓冲，必须按完成顺序串行化）。
            drain_completed(renderer, shared, submitted);
            let mut frame = FramePlan::new(draw_plan, renderer.is_fresh());
            if frame.is_idle() {
                debug_assert!(false, "idle frame consumed a waitable token");
                break;
            }
            match renderer.submit_frame(&request, &mut frame, slot, replace) {
                Ok((stats, samples)) => {
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
                        signal_value: slot.signal_value,
                        slot: slot.index,
                        surface: request.surface,
                    });
                    // 明确唤醒：该帧 GPU 工作完成时 fence 置位 `completion`（auto-reset），不忙轮询。
                    renderer.set_completion_event(slot.signal_value, shared.completion)?;
                    break;
                }
                Err(FrameSubmitError::NeedsResize) => {
                    // `Present1` 报告交换链 out of date / 显示模式变化（R08-7）：重建目标
                    // （`ResizeBuffers`）后重录这一帧。
                    attempts += 1;
                    if attempts > RESIZE_ATTEMPTS {
                        return Err(EngineError::Swapchain {
                            detail: smol_str::SmolStr::new_static(
                                "Present1 kept reporting an out-of-date swapchain after resizing",
                            ),
                        });
                    }
                    renderer.resize_target()?;
                    draw_plan = Plan::Full;
                    continue;
                }
                Err(FrameSubmitError::RetryFull) => {
                    // `Present1` 拒绝了本帧的 dirty 矩形（AR3 P2-4）：增量帧的 back buffer 只同步了
                    // `pending ∪ damage`，整面 present 会把未同步区域（3 帧前的旧内容 / fresh 时的
                    // 未定义内容）上屏。安全退路只有**整帧重录**：`Plan::Full` 让 `record` 走
                    // `Direct`（整块 clear + 全内容重画 + 整面 present），back buffer 每个像素都有
                    // 定义。这是错误路径上的一次额外录制/提交，稳态不受影响。
                    full_records += 1;
                    if full_records > FULL_RECORD_ATTEMPTS {
                        return Err(EngineError::Swapchain {
                            detail: smol_str::SmolStr::new_static(
                                "Present1 kept rejecting the frame after a full re-record",
                            ),
                        });
                    }
                    draw_plan = Plan::Full;
                    continue;
                }
                Err(FrameSubmitError::Fatal(error)) => return Err(error),
            }
        }
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
            // 该端没有可靠的呈现时间戳（Windows 的 GetFrameStatistics 未接；Linux 的帧回调未接）：
            // 共享契约里 `presented_at` 是可空字段，HUD 会退回用 `latency`。
            presented_at: None,
        });
        // 「交换链里现在有这么大一块内容」：这是 UI 线程重算 DComp 几何的正确时点（合成内容
        // 已经落地），尺寸变化时才 post 唤醒。
        shared.publish_presented(frame.surface);
        observed = true;
    }
    if observed {
        // I4：只把 fence 已到达的帧所引用的旧资源交回 device。
        renderer.release_completed();
    }
}

/// 空闲等待：`[work_event, completion_event]`（都是 auto-reset，不会自旋）。
///
/// `timeout` 只用于遮挡期间的可见性重查与缩容之外的定时唤醒；`None` = 无限等。
/// 超时不是错误：回到循环顶部重新检查待画帧 / 遮挡 / 关闭。
///
/// **这里的 `INFINITE` 是刻意保留的**（与 P1-2.13 修掉的那几处不同）：这是 GPU 线程「没有
/// 活干」时的空闲等待，两个事件之一（新帧 / 关闭）出现前本该什么都不做。改成定时轮询只会
/// 白白唤醒 CPU（60 Hz 空转 = 每秒 60 次无效唤醒），并不会让任何失败更早被发现 ——
/// 挂死风险在**等 GPU 完成**的路径上（fence / present waitable），那两处已改成有界等待。
fn wait_for_work_or_completion(
    shared: &Shared,
    timeout: Option<Duration>,
) -> Result<(), EngineError> {
    let millis = timeout.map(|timeout| {
        u32::try_from(timeout.as_millis())
            .unwrap_or(u32::MAX)
            .max(1)
    });
    // SAFETY: 两个句柄都由 `Shared` 持有，生命周期覆盖本次等待；`timeout = None` ⇒ INFINITE。
    let wait = unsafe {
        WaitForMultipleObjects(
            &[shared.work, shared.completion],
            false,
            millis.unwrap_or(INFINITE),
        )
    };
    match wait {
        WAIT_OBJECT_0 => Ok(()),
        // `WaitForMultipleObjects` 的第二个对象 = completion。
        wait if wait.0 == WAIT_OBJECT_0.0 + 1 => Ok(()),
        // 定时唤醒：不是错误，也不是待画帧。
        WAIT_TIMEOUT => Ok(()),
        _ => Err(EngineError::AppInit {
            detail: format_smolstr!(
                "WaitForMultipleObjects(work, completion) failed: {wait:?} ({})",
                std::io::Error::last_os_error()
            ),
        }),
    }
}

/// 等帧延迟 waitable；期间 completion 到达就先发布反馈、再继续等（不把反馈拖到下一帧）。
///
/// 返回 `true` 表示**消费了** waitable（调用方必须 present 一帧）；`false` 表示提前返回
/// （关闭 / 遮挡 / 超时后需要重新决策），调用方不得再取帧。
///
/// 超时（`None`）**不是错误**（R08-3）：`GetDeviceRemovedReason() == S_OK` 时窗口必须活着。
/// 旧实现在 4 s 后返回 `DeviceLost`（粘性、进程级），锁一次屏就足以让整个应用永久冻结。
fn wait_for_waitable(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
) -> Result<bool, EngineError> {
    let Some(waitable) = renderer.waitable() else {
        // 离屏目标没有交换链：节奏由调用方（同步路径）决定。
        return Ok(true);
    };
    loop {
        match wait_for_pair(waitable, shared.completion, "waitable, completion")? {
            Some(0) => return Ok(true),
            Some(_) => {
                // completion：先发布反馈再继续等（waitable 还没被消费）。
                drain_completed(renderer, shared, submitted);
                if shared.shutdown_requested() || renderer.window_occluded() {
                    return Ok(false);
                }
            }
            None => {
                // 超时：合成器/GPU 没回话但设备可能仍健康。计数 + 让设备丢失判据去做唯一裁决，
                // 然后退避重试（不升级成任何粘性错误）。
                crate::render::windows::diagnostics::present_waitable_timeout();
                if let Some(error) = renderer.check_device_lost() {
                    return Err(error);
                }
                if shared.shutdown_requested() || renderer.window_occluded() {
                    return Ok(false);
                }
                std::thread::sleep(PRESENT_STALL_BACKOFF);
            }
        }
    }
}

/// `WaitForMultipleObjects([low, high])`；返回被 signal 的句柄下标（0 或 1）。
///
/// **有界等待**（P1-2.13）：旧实现传 `INFINITE`。设备被移除 / 驱动挂死时 DXGI 的 waitable
/// 对象永远不会 signal，GPU 线程就永久阻塞（关闭时还会连带拖住 UI 线程的消息泵）。
/// 超时返回 `Ok(None)` 让调用方重查关闭 / 遮挡标志并交 `GetDeviceRemovedReason` 裁决。
fn wait_for_pair(low: HANDLE, high: HANDLE, label: &str) -> Result<Option<usize>, EngineError> {
    // SAFETY: 两个句柄都由 `Shared`（或 swapchain）持有，调用方保证它们的生命周期覆盖本次等待。
    let wait = unsafe { WaitForMultipleObjects(&[low, high], false, PRESENT_TIMEOUT_MS) };
    match wait.0 {
        0 => Ok(Some(0)),
        1 => Ok(Some(1)),
        // 超时：不是错误，让调用方重查关闭 / 遮挡标志并计入诊断。
        timeout if timeout == WAIT_TIMEOUT.0 => Ok(None),
        // 无 mutex：只剩 `WAIT_FAILED`（或驱动返回的异常值）。
        _ => Err(EngineError::AppInit {
            detail: format_smolstr!(
                "WaitForMultipleObjects({label}) failed: {wait:?} ({})",
                std::io::Error::last_os_error()
            ),
        }),
    }
}

/// present waitable 的单次等待上限（P1-2.13）。
const PRESENT_TIMEOUT_MS: u32 = 1_000;
/// waitable 超时后的退避（R08-3：不升级成错误，只是让合成器有时间恢复）。
const PRESENT_STALL_BACKOFF: Duration = Duration::from_millis(16);
/// 遮挡期间重查可见性的间隔（R08-7）。
const OCCLUDED_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// `Present1` 报告 out of date 之后最多重试几次（R08-7）。
const RESIZE_ATTEMPTS: u32 = 2;
/// `Present1(InvalidCall)` 之后允许**整帧重录**几次（AR3 P2-4）。
///
/// 正常只需要一次：整帧重录走 `Direct`（`present = None`），再被拒会直接落到
/// `invalid_call_recovery` 的 `Fatal` 分支。这个上界只为「重录之后又变成增量帧」这类组合兜底 ——
/// 没有它，任何让该错误分支可重入的改动都会变成无限重试。
const FULL_RECORD_ATTEMPTS: u32 = 1;

/// 关闭时等待 GPU 线程退出的单轮上限（期间继续泵消息）。
const JOIN_TIMEOUT_MS: u32 = 250;
/// 关闭时等待 GPU 线程退出的总上限；超过则 detach，避免进程关不掉。
const JOIN_HANG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 边泵消息边等 GPU 线程退出（§6.7）。
///
/// 不能直接 `join`：DXGI 的 `Present` 可能同步 `SendMessage` 到本线程的消息泵，而本线程正在
/// join 里等 GPU 线程 —— 互等。这里用 `MsgWaitForMultipleObjects`（对象 = GPU 线程句柄）在
/// 等待期间继续派发消息，直到线程句柄 signaled。
///
/// **总时长有界**（P1-2.13）：旧实现每轮传 `INFINITE`，如果 GPU 线程卡在设备挂死的 `Present`
/// 里，关闭流程会永久挂住 UI 线程（用户看到的是「关不掉」）。现在每轮最多等
/// `JOIN_TIMEOUT_MS` 并派发消息，累计超过 `JOIN_HANG_TIMEOUT` 就放弃 join（detach）并返回 ——
/// 让进程能退出，代价是可能跳过 GPU 资源的优雅释放。
fn join_with_message_pump(thread: JoinHandle<()>) {
    // `JoinHandle` 的平台句柄在 join / drop 之前有效；这里只读它（RawHandle 是 Copy）。
    let handle = HANDLE(thread.as_raw_handle());
    let mut quit = None;
    let deadline = std::time::Instant::now() + JOIN_HANG_TIMEOUT;
    loop {
        // SAFETY: `handle` 由 `thread` 持有；`QS_ALLINPUT` 只观察本线程的消息队列。
        let wait = unsafe {
            MsgWaitForMultipleObjects(Some(&[handle]), false, JOIN_TIMEOUT_MS, QS_ALLINPUT)
        };
        if wait == WAIT_OBJECT_0 {
            let _ = thread.join();
            break;
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            // 有消息：先派发，再继续等（关闭期间窗口仍然存活）。
            quit = pump_messages().or(quit);
            continue;
        }
        if wait.0 == WAIT_TIMEOUT.0 {
            if std::time::Instant::now() >= deadline {
                // GPU 线程在设备挂死后不会返回：放弃 join（detach）让进程仍能退出。
                eprintln!(
                    "kytrix: kytrix-d3d12 thread did not exit within {:?}; detaching it (GPU likely lost)",
                    JOIN_HANG_TIMEOUT
                );
                break;
            }
            // 超时后继续泵消息：关闭期间窗口仍然存活，必须保持响应。
            quit = pump_messages().or(quit);
            continue;
        }
        // 异常（`WAIT_FAILED`；无 mutex，不会出现 `WAIT_ABANDONED_*`）：
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

/// `(宽, 高)` → 单个 `u64`（`width << 32 | height`），给无锁的 `presented` 槽用。
fn pack_size(surface: (u32, u32)) -> u64 {
    (u64::from(surface.0) << 32) | u64::from(surface.1)
}

/// [`pack_size`] 的逆。
fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use smol_str::SmolStr;

    use super::{AtlasSlots, PanicGuard, Shared, State, StickyError, pack_size, unpack_size};
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

    /// `atlas_pending` 只能在**录制完成**后清：`take_atlas` 只把上传搬给 GPU 线程，清早了
    /// UI 会在本帧未录制时入队下一笔并覆盖它（DEVIATIONS F-211 的丢行回归面）。
    #[test]
    fn an_atlas_upload_stays_busy_until_it_is_recorded() {
        let shared = test_shared();
        assert!(shared.atlas_idle(AtlasKind::Glyph));
        assert!(shared.queue_atlas(AtlasKind::Glyph, 64, &[1, 2, 3], stale_rows(4, 8)));

        let [glyph, image] = shared.take_atlas();
        assert!(glyph.is_some());
        assert!(image.is_none());
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
            detail: SmolStr::new_static("Present1(1 dirty rects): 0x887A0001"),
        });
        let text = sticky.to_error().to_string();
        assert!(text.contains("Present1"), "{text}");

        // P2-5：运行期 DComp 失败必须保持 `Composition`，不能被降级成 `AppInit`。
        let sticky = StickyError::capture(EngineError::Composition {
            detail: SmolStr::new_static("set_geometry: 0x8007000E"),
        });
        let text = sticky.to_error().to_string();
        assert!(text.contains("DirectComposition"), "{text}");
        assert!(text.contains("set_geometry"), "{text}");
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
            // `hwnd = 0`：`PostMessageW(NULL, ...)` 是线程消息，测试里没有窗口也能投递。
            hwnd: 0,
            thread: 0,
            presented: AtomicU64::new(pack_size((1, 1))),
            composition_pending: AtomicBool::new(false),
            atlas_pending: [AtomicBool::new(false), AtomicBool::new(false)],
        })
    }

    /// `presented` 槽：尺寸变化才 post（动画帧不唤醒 UI），打包/拆包可逆。
    #[test]
    fn the_presented_size_wakes_the_ui_only_when_it_changes() {
        let shared = test_shared();
        assert_eq!(
            unpack_size(shared.presented.load(Ordering::Acquire)),
            (1, 1)
        );

        shared.publish_presented((800, 600));
        assert_eq!(
            unpack_size(shared.presented.load(Ordering::Acquire)),
            (800, 600)
        );
        // 同尺寸再发布：不改变槽位（也不会 post —— 这里无法观察消息，由 `notify_ui` 的
        // 调用点断言：`swap` 的返回值相等时提前返回）。
        let before = shared
            .presented
            .swap(pack_size((800, 600)), Ordering::AcqRel);
        assert_eq!(unpack_size(before), (800, 600));
        shared.publish_presented((640, 480));
        assert_eq!(
            unpack_size(shared.presented.load(Ordering::Acquire)),
            (640, 480)
        );
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
            detail: SmolStr::new_static("content 3000x2000 exceeds the composition buffer"),
        });
        // 正常退出：guard 被 disarm，不发布任何东西。
        PanicGuard::new(Arc::clone(&shared)).disarm();
        match shared.take_error().expect("the loop error stays visible") {
            EngineError::Swapchain { detail } => {
                assert!(detail.contains("composition buffer"), "{detail}");
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
