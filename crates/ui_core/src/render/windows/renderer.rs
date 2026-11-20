//! 窗口渲染门面 + 每窗口一条 `kytrix-d3d12` GPU 线程
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
//! GPU 对象分两层（见 `device.rs`）：device / direct queue /
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
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smol_str::{SmolStr, format_smolstr};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM},
    Graphics::Dxgi::DXGI_FRAME_STATISTICS,
    System::{
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Threading::{CreateEventW, GetCurrentThreadId, INFINITE, SetEvent, WaitForMultipleObjects},
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
    paint::{
        atlas::{AtlasKind, MAX_IMAGE_PAGES},
        color::Color,
    },
    platform::{
        dispatcher::WM_APP_COMPOSITION_WAKE,
        windows::composition::{self, Composition, CompositionState},
    },
    render::{
        DirtyRects, RenderStats,
        color::background_linear,
        compositor::{FramePlan, Plan},
        frame::{FrameFeedback, FrameRequest, FrameSink, LatestFrame, MAX_FRAMES_IN_FLIGHT},
        present_evidence::{
            clamp_presented_at, presented_qpc_from_frame_statistics, qpc_instant,
            refresh_period_qpc,
        },
        timer::{FrameTimings, GpuTimerSample},
        windows::{
            compositor::{FrameSubmitError, GpuRenderer, enter_gpu_thread},
            device::init_error_from,
            diagnostics::Diagnostics,
            display,
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

    /// 请求 GPU 线程检查 / 重建 DirectComposition 设备与 visual tree（C2；泵的几何 pass 调用）。
    ///
    /// 泵已经在本轮消费了 `WM_PAINT` 健康检查代际并调过 `CheckDeviceState`：Invalid 时才请求，
    /// 真正重建在 GPU 线程（`GpuRenderer::rebuild_composition`）。返回 `false` = 没有窗口目标或
    /// GPU 线程已经不在（关闭路径），调用方不需要任何处理。
    pub(crate) fn request_composition_rebuild(&self) -> bool {
        match &self.target {
            RenderTarget::Offscreen(_) => false,
            RenderTarget::Window(window) => window.shared.request_composition_rebuild(),
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

    /// 把一页图集像素交给 GPU 线程（`pixels` 是 `rects` 顺序逐矩形紧密打包的字节）。
    ///
    /// `layer` 是图集 texture array 的层号（图集页索引）；`spec` 是该图集 array 的规格
    /// （页边长 + 层数；图片的层数 = App 当前打开的图集页数）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => {
                gpu.queue_atlas_upload(kind, layer, spec, pixels.to_vec(), rects);
                true
            }
            RenderTarget::Window(window) => {
                window.queue_atlas_upload(kind, layer, spec, pixels, rects)
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

    /// 该窗口本轮是否参与渲染（遮挡 / 最小化跳过的窗口不参与图集扩层闸门，RBT-10）。
    pub(crate) fn atlas_participating(&self) -> bool {
        match &self.target {
            // 离屏目标由调用方同步提交帧，没有「跳过」状态。
            RenderTarget::Offscreen(_) => true,
            RenderTarget::Window(window) => window.atlas_participating(),
        }
    }

    /// 测试：离屏目标上某图集**已入队未录制**的层号（窗口路径的上传在 GPU 线程里，返回 `None`）。
    #[cfg(test)]
    pub(crate) fn atlas_pending_layers(&self, kind: AtlasKind) -> Option<Vec<u16>> {
        match &self.target {
            RenderTarget::Offscreen(gpu) => Some(gpu.atlas_pending_layers(kind)),
            RenderTarget::Window(_) => None,
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

    fn reset_epoch(&self) -> u64 {
        // GPU 线程重建镜像时用 `AcqRel` 写；`Acquire` 读保证 UI 线程看到重建之后的全部效果。
        self.shared.reset_epoch.load(Ordering::Acquire)
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
            reset_epoch: AtomicU64::new(0),
            composition_pending: AtomicBool::new(false),
            composition_rebuild: AtomicBool::new(false),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
            atlas_participating: AtomicBool::new(true),
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
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        self.shared.queue_atlas(kind, layer, spec, pixels, rects)
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.shared.atlas_idle(kind)
    }

    /// 本轮该窗口是否参与渲染（GPU 循环遮挡 / 最小化跳过的窗口不参与图集扩层闸门）。
    fn atlas_participating(&self) -> bool {
        self.shared.atlas_participating()
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
        // 共享契约（`render/frame.rs`）：`presented_at` 要么是**可信**的上屏时刻（Windows 恒为
        // `None` —— `GetFrameStatistics` 未接），要么如实为 `None`；绝不允许用完成时刻顶替。
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
    /// 渲染器重建代际（设备恢复 / 显示恢复 / 新渲染器挂上 ⇒ +1）。
    ///
    /// GPU 线程在重建之后 +1，UI 线程每帧通过 [`FrameSink::reset_epoch`] 读它；值变了
    /// ⇒ UI 必须整帧重发实例缓冲（AR4 P1-1：镜像可能已被清空，增量载荷补不满它）。
    reset_epoch: AtomicU64,
    /// 两次唤醒都失败时的兜底标记：UI 线程在泵的几何 pass 里消费它并重算一次几何。
    composition_pending: AtomicBool,
    /// 「DirectComposition 设备可能失效，请 GPU 线程检查/重建」请求（C2）。
    ///
    /// 泵的几何 pass 消费 `WM_PAINT` 健康检查代际并调 `CheckDeviceState`；Invalid 时置位本标记 +
    /// `SetEvent(work)` 唤醒 GPU 线程，由 GPU 线程重建 visual tree（需要 D3D12 设备/交换链）。
    composition_rebuild: AtomicBool,
    /// 每 `AtlasKind` 的图集上传槽位掩码（bit n = texture array 的第 n 层正在被 GPU 线程消费；
    /// `0` = UI 可以入队）。图片图集一帧内会连续入队多个层，因此是位掩码而不是单一布尔。
    atlas_pending: [AtomicU32; 2],
    /// GPU 循环本轮是否**参与渲染**（遮挡 / 最小化时置 false，见 `run_gpu_loop`）。
    ///
    /// App 的扩层闸门据此把「暂时不渲染的窗口」排除在外（RBT-10）：这类窗口不会消费
    /// `take_atlas`，若仍计入 `atlas_idle` 门禁，它的待上传位会**永久**挡住其它窗口的扩层。
    atlas_participating: AtomicBool,
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

/// 图集上传槽：字形 1 层 + 图片 `MAX_IMAGE_PAGES` 层（页数上限），每层只留最新一份（§4.2）。
///
/// 一个窗口一帧内会**连续**入队图片图集的多个层（逐层整页/增量），因此槽位必须按层分开；
/// `take` 一次性把本帧全部待上传层交给 GPU 线程（同一提交内聚合录制）。数组的**当前**层数是
/// 运行时值（`AtlasArraySpec::layers`），槽位数组只按页数上限留位。
struct AtlasSlots {
    glyph: Option<AtlasUpload>,
    image: [Option<AtlasUpload>; MAX_IMAGE_PAGES as usize],
}

/// 一个待上传层：`pixels` 是 `rects` 顺序逐矩形紧密打包的字节（整页上传时就是整页）。
struct AtlasUpload {
    kind: AtlasKind,
    /// texture array 层号（字形恒 0；图片 = 图集页索引）。
    layer: u16,
    /// 该图集 array 的规格（页边长 + 层数；图片层数随打开的页数增长）。
    spec: crate::render::AtlasArraySpec,
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
        spec: crate::render::AtlasArraySpec,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        assert!(
            u32::from(layer) < spec.layers,
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
            spec,
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

    /// 发布「本轮是否参与渲染」。GPU 循环在遮挡 / 最小化跳过时置 false，恢复后置 true。
    ///
    /// 判据与真正的跳过**同源**（`GpuRenderer::window_occluded()` 的那次判断），不是另猜
    /// DComp 状态：App 的扩层闸门读到的就是 GPU 循环自己这一轮的决策（RBT-10）。
    fn set_atlas_participating(&self, participating: bool) {
        self.atlas_participating
            .store(participating, Ordering::Release);
    }

    /// 该窗口本轮是否参与渲染（遮挡 / 最小化跳过的窗口不参与图集闸门）。
    fn atlas_participating(&self) -> bool {
        self.atlas_participating.load(Ordering::Acquire)
    }

    /// 发布「本轮是否参与渲染」，并返回本轮是否**跳过渲染**（遮挡 / 最小化且非
    /// `force_present`）。
    ///
    /// 「跳过」与「不参与扩层闸门」必须由同一个表达式决定（RBT-10 / RBT-14）：GPU 循环只
    /// 调用本函数，不再自己拼条件。跳过 ⇒ 本循环不会 `take_atlas`，待上传位会一直亮着，必须
    /// 让 App 的扩层闸门把它排除在外；恢复可见（或 `force_present` 放行）⇒ 重新参与。
    fn publish_atlas_participation(&self, occluded: bool, force_present: bool) -> bool {
        let skip = occluded && !force_present;
        self.set_atlas_participating(!skip);
        skip
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
        spec: crate::render::AtlasArraySpec,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        let index = kind.index() as usize;
        assert!(
            u32::from(layer) < spec.layers,
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
        state.atlas.queue(kind, layer, spec, pixels, rects);
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

    /// 请求 GPU 线程检查 / 重建 DirectComposition 设备与 visual tree（C2；UI 线程）。
    ///
    /// 只置位 + `SetEvent(work)`：检查与重建在 GPU 线程（需要 D3D12 设备/交换链，且 DComp 调用
    /// 不能从 `WndProc`/泵的借用里做）。GPU 线程不可用时返回 `false`。
    fn request_composition_rebuild(&self) -> bool {
        if !self.alive() {
            return false;
        }
        self.composition_rebuild.store(true, Ordering::Release);
        self.wake();
        true
    }

    /// 取走「重建 DComp」请求（GPU 线程循环顶部消费）。
    fn take_composition_rebuild_request(&self) -> bool {
        self.composition_rebuild.swap(false, Ordering::AcqRel)
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

    /// 渲染器重建（设备恢复 / 显示拓扑恢复 / 新渲染器挂上）：代际 +1。
    ///
    /// 调用点在 [`gpu_loop`] 的两处重建分支：
    /// - 设备恢复：`recover()` 已经把 GPU 侧的 `InstanceMirror` 重建为空，UI 侧必须从下一帧
    ///   开始整帧重发实例缓冲（[`FrameSink::reset_epoch`] 的握手）；
    /// - 显示 / 电源恢复（R07-14）：`rebuild_target` 重建了交换链与持久目标，同样让 UI 整帧
    ///   重发，恢复后的第一帧因此不依赖任何帧前状态。
    ///
    /// `AcqRel`：与 UI 线程的 `Acquire` 读配对，保证「代际可见」晚于「镜像/目标已重建」。
    fn note_renderer_rebuilt(&self) {
        self.reset_epoch.fetch_add(1, Ordering::AcqRel);
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
    /// A4：`Present1` 成功之后立即采样的**上屏时刻下界证据**（`None` = 该拍拿不到统计）。
    ///
    /// 采样点在本次 present 之后、下一次 present 之前 ⇒ 「最后一次已上屏内容」一定不晚于本帧
    /// ⇒ 它是本帧真实上屏时刻的下界。发布前再经 [`clamp_presented_at`] 与 `completed_at`
    /// 取较大者（两个下界取 max 仍是下界）。
    presented: Option<Instant>,
    signal_value: u64,
    /// ring 槽位：完成时从该槽的 timestamp 读回缓冲读 GPU 时间。
    slot: usize,
    /// 本帧 present 的内容尺寸（完成时发布给 UI 线程的合成几何）。
    surface: (u32, u32),
}

/// `presented_at` 的 Windows 证据采样状态（A4）。
///
/// `DXGI_FRAME_STATISTICS` 只给「最近一次**已上屏**内容发生在哪个 vblank」+「最近一次 vblank
/// 采样的 QPC 时刻」，而 `GetFrameStatistics` 是**进程级/交换链级快照**、没有逐帧归属信息：
///
/// - present 之后立即采样时，「最后一次已上屏内容」必然早于本帧 ⇒ 结果**恒为本帧上屏时刻的
///   下界**（真实上屏只能更晚）；
/// - 若本帧完成时它仍是最后一次 present（没有更晚的帧上屏），完成观测点再采一次：这一刻可能
///   已经等到本帧自己的 vblank（值更紧）。两个下界取 `max` 仍是下界。
///
/// 周期：`DXGI_FRAME_STATISTICS` **没有** `SyncRefreshPeriod` 字段（`SyncGPUTime` reserved 恒 0，
/// MS docs 与 windows crate 0.62 均已核对），因此用相邻两次采样的
/// `ΔSyncQPCTime / ΔSyncRefreshCount` 实测（[`refresh_period_qpc`]）；`lag == 0` 时不需要周期。
struct FrameStatisticsClock {
    /// `QueryPerformanceFrequency`（ticks/秒；0 = 系统没给）。
    frequency: i64,
    /// 进程级 QPC → `Instant` 锚点（首次转换时采样一次）。
    anchor: OnceLock<(i64, Instant)>,
    /// 上一次采样的 `(SyncRefreshCount, SyncQPCTime)`：周期推导用。
    last_sync: Option<(u32, i64)>,
    /// 最近一次实测的刷新周期（QPC ticks）。
    period_qpc: Option<i64>,
    /// 有统计的采样次数 / 失败次数（诊断）。
    samples: u64,
    failures: u64,
    /// 已经打印过「本机拿不到帧统计」的一次性诊断（不刷屏）。
    warned: bool,
}

impl FrameStatisticsClock {
    fn new() -> Self {
        let mut frequency = 0i64;
        // SAFETY: kernel32 查询；入参指向本函数栈上的变量。
        let frequency =
            unsafe { QueryPerformanceFrequency(&mut frequency) }.map_or(0, |()| frequency);
        Self {
            frequency,
            anchor: OnceLock::new(),
            last_sync: None,
            period_qpc: None,
            samples: 0,
            failures: 0,
            warned: false,
        }
    }

    /// 进程级 QPC → `Instant` 锚点（`(QueryPerformanceCounter, Instant::now())` 同一次采样）。
    fn anchor(&self) -> Option<(i64, Instant)> {
        if let Some(anchor) = self.anchor.get() {
            return Some(*anchor);
        }
        let mut counter = 0i64;
        // SAFETY: kernel32 查询；入参指向本函数栈上的变量。
        unsafe { QueryPerformanceCounter(&mut counter) }.ok()?;
        let anchor = (counter, Instant::now());
        Some(*self.anchor.get_or_init(|| anchor))
    }

    /// 一次 `GetFrameStatistics` 采样 → 本帧上屏时刻的下界证据（`None` = 该拍不可信）。
    fn observe(&mut self, stats: Option<DXGI_FRAME_STATISTICS>) -> Option<Instant> {
        let Some(stats) = stats else {
            self.failures = self.failures.saturating_add(1);
            self.warn_if_unavailable();
            return None;
        };
        self.samples = self.samples.saturating_add(1);
        let current = (stats.SyncRefreshCount, stats.SyncQPCTime);
        if let Some(previous) = self.last_sync
            && let Some(period) = refresh_period_qpc(previous, current, self.frequency)
        {
            self.period_qpc = Some(period);
        }
        self.last_sync = Some(current);
        let presented_qpc = presented_qpc_from_frame_statistics(
            stats.SyncRefreshCount,
            stats.PresentRefreshCount,
            stats.SyncQPCTime,
            self.period_qpc,
        )?;
        let (anchor_qpc, anchor) = self.anchor()?;
        qpc_instant(
            presented_qpc,
            anchor_qpc,
            anchor,
            Instant::now(),
            self.frequency,
        )
    }

    /// 连续几十帧都拿不到统计 ⇒ 本机/本显示器不提供（`presented_at` 如实为 `None`），
    /// 打印一次诊断后不再刷屏。
    fn warn_if_unavailable(&mut self) {
        if self.warned || self.samples > 0 || self.failures < 60 {
            return;
        }
        self.warned = true;
        eprintln!(
            "kytrix: IDXGISwapChain::GetFrameStatistics unavailable on this display; \
             FrameFeedback::presented_at stays None (no fabricated timestamps)"
        );
    }
}

/// GPU 线程错误边界的权威判据：设备是否真的已移除（C1）。
///
/// 只在**非** `DeviceLost` 错误上调用（`DeviceLost` 本身就是粘性结论）：`GetDeviceRemovedReason`
/// 是唯一权威来源，设备健康时恒 `false`。
fn device_lost_for_boundary(renderer: Option<&GpuRenderer>) -> bool {
    renderer.is_some_and(|current| current.check_device_lost().is_some())
}

/// GPU 线程致命错误的处置分类（C1 边界复判 + C2 的 DComp 可恢复分支）。
///
/// - `DeviceLost` 是粘性结论，必须整 renderer 恢复；
/// - 其它错误（`Swapchain` / `Composition` / ...）只有权威判据确认设备已移除时才走设备恢复 ——
///   典型来源是 `ResizeBuffers` 在 TDR / 驱动复位竞态下返回设备级 HRESULT 却被包成交换链错误；
/// - **只有 `Composition` 错误**且 `CheckDeviceState` 判 DComp 失效时才只重建合成设备/visual tree
///   （C2）：DComp 状态是**附加事实**，不能把 `AppInit` / `FramePipeline` / `Swapchain` 等真实
///   致命错误吞成 visual 重建（那会掩盖原始错误，持续 Invalid 时还会以「DComp 重试耗尽」误导
///   归因）。收窄条件放在本函数内，纯逻辑可测（review-c RC-4）。
///
/// 逐调用点的 `classify` 让错误**分类正确**；本判据保证**不会漏掉**尚未接线的调用点（新增 DXGI
/// 调用点忘记 classify 时仍然恢复而不是静默退出线程）。
fn fatal_disposition(
    error: &EngineError,
    device_lost: bool,
    dcomp_invalid: bool,
) -> FatalDisposition {
    if matches!(error, EngineError::DeviceLost { .. }) || device_lost {
        FatalDisposition::DeviceLost
    } else if dcomp_invalid && matches!(error, EngineError::Composition { .. }) {
        FatalDisposition::CompositionInvalid
    } else {
        FatalDisposition::Fatal
    }
}

/// GPU 线程致命错误的处置（C1/C2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FatalDisposition {
    /// 真正致命：交给 UI（粘性错误）。
    Fatal,
    /// 设备已移除：恢复整个 renderer（D3D12 设备 + DComp visual tree）。
    DeviceLost,
    /// D3D12 设备健康但 DirectComposition 失效：只重建 DComp 设备 / visual tree。
    CompositionInvalid,
}

/// DirectComposition 设备 / visual tree 重建的限次预算（C2）。
///
/// 与 `SharedGpu::recover`（D3D12 设备恢复）同一量级：窗口内最多
/// [`MAX_COMPOSITION_REBUILDS`] 次。DWM 反复重启 / DComp 持续失效时，无限重建就是「每帧重建
/// visual tree」的活锁；超限后按粘性 `Composition` 错误上报，由应用决定是否重建窗口。
const COMPOSITION_RECOVERY_WINDOW: Duration = Duration::from_secs(5);
const MAX_COMPOSITION_REBUILDS: usize = 3;

/// DComp 重建预算（GPU 线程独占；纯逻辑，可用 `allow_rebuild` 单测）。
struct CompositionRecovery {
    /// 最近窗口内的重建时刻（FIFO）。
    rebuilds: VecDeque<Instant>,
}

impl CompositionRecovery {
    fn new() -> Self {
        Self {
            rebuilds: VecDeque::with_capacity(MAX_COMPOSITION_REBUILDS),
        }
    }

    /// 登记一次重建尝试；窗口内超限返回 `false`（调用方按粘性错误上报）。
    fn allow_rebuild(&mut self) -> bool {
        let now = Instant::now();
        while self
            .rebuilds
            .front()
            .is_some_and(|at| now.duration_since(*at) >= COMPOSITION_RECOVERY_WINDOW)
        {
            self.rebuilds.pop_front();
        }
        if self.rebuilds.len() >= MAX_COMPOSITION_REBUILDS {
            return false;
        }
        self.rebuilds.push_back(now);
        true
    }
}

/// 重建预算耗尽时的粘性错误（C2；文案带次数与窗口，便于诊断归因）。
fn composition_rebuild_exhausted() -> EngineError {
    EngineError::Composition {
        detail: format_smolstr!(
            "DirectComposition device stayed invalid after {MAX_COMPOSITION_REBUILDS} rebuilds within {COMPOSITION_RECOVERY_WINDOW:?}"
        ),
    }
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
    let mut composition_recovery = CompositionRecovery::new();
    loop {
        let result = run_gpu_loop(
            renderer
                .as_mut()
                .expect("renderer is present until teardown"),
            &shared,
            &mut submitted,
            &mut composition_recovery,
        );
        match result {
            Ok(()) => break,
            Err(error) => {
                // C1 的边界复判：任何 GPU 线程致命错误在宣布致命之前，用权威判据
                // （`GetDeviceRemovedReason`）复查一次设备是否真的还在。逐调用点 `classify`
                // 之外还会有遗漏（尤其 `ResizeBuffers` 这类把设备级 HRESULT 包成
                // `Swapchain` 的出口），这里兜底：设备已死就必须走恢复，而不是退出线程。
                let device_lost = match &error {
                    EngineError::DeviceLost { .. } => true,
                    _ => device_lost_for_boundary(renderer.as_ref()),
                };
                // C2：D3D12 健康时再取一次 DComp 的权威状态（原始事实）。「只有 `Composition`
                // 错误才允许走 visual 重建」的收窄在 `fatal_disposition` 内完成（纯逻辑可测）：
                // `AppInit` / `FramePipeline` / `Swapchain` 等致命错误即使 DComp 恰好 Invalid
                // 也必须保持致命，不能被吞成 visual 重建。
                let dcomp_invalid = !device_lost
                    && renderer.as_ref().is_some_and(|current| {
                        current.composition_state() == CompositionState::Invalid
                    });
                match fatal_disposition(&error, device_lost, dcomp_invalid) {
                    FatalDisposition::Fatal => {
                        shared.publish_error(error);
                        break;
                    }
                    FatalDisposition::DeviceLost => {
                        // 设备真的被移除了：先尝试一次进程内恢复（限次在 `SharedGpu::recover` 里）。
                        submitted.clear();
                        let current = renderer.take().expect("renderer is present until teardown");
                        match current.recover() {
                            Ok(recovered) => {
                                crate::render::windows::diagnostics::device_recovered();
                                renderer = Some(recovered);
                                // 镜像已随设备重建 ⇒ UI 必须整帧重发：`InstanceMirror` 现在为空，
                                // 而增量载荷只带「变化」的 lane，其余 lane 会画成退化实例
                                // （AR4 P1-1）。
                                shared.note_renderer_rebuilt();
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
                    FatalDisposition::CompositionInvalid => {
                        if !composition_recovery.allow_rebuild() {
                            shared.publish_error(composition_rebuild_exhausted());
                            break;
                        }
                        let current = renderer
                            .as_mut()
                            .expect("renderer is present until teardown");
                        match current.rebuild_composition() {
                            Ok(()) => {
                                crate::render::windows::diagnostics::composition_recovered();
                                // visual tree 换了：旧 visual 上的内容已随旧设备释放 ⇒ UI 整帧重发。
                                shared.note_renderer_rebuilt();
                                // **不**清 `submitted`：D3D12 设备 / 提交 / fence 都还在，
                                // 完成反馈与 retire 必须继续推进（否则 retire 队列会泄漏到卸载）。
                            }
                            Err(fatal) => {
                                shared.publish_error(fatal);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    drop(renderer.take());
    shared.mark_terminated();
    guard.disarm();
}

/// GPU 线程本地的显示 / 电源恢复状态（R07-14）。
///
/// 代际是进程级的单调信号，失败重试与它绑定：**换代际就重新计数**，避免「几次互不相关的
/// 拓扑变化各失败一次」被累加成一次粘性错误。
struct DisplayRecovery {
    /// 已成功消费的代际。
    seen: u64,
    /// 正在重建的代际（`None` = 没有进行中的恢复）。
    pending: Option<u64>,
    /// `pending` 代际的连续失败次数。
    attempts: u32,
}

impl DisplayRecovery {
    fn new() -> Self {
        Self {
            seen: display::epoch(),
            pending: None,
            attempts: 0,
        }
    }
}

/// 显示 / 电源恢复（R07-14）：消费平台层推进的代际，重建本窗口的目标并让 UI 整帧重发。
///
/// 返回 `true` = 已清掉遮挡退避：调用方把**恢复后的第一帧**放行（唤醒后必须出帧，不能被
/// 「不可见 / 被遮挡」的跳过逻辑永久挡住）。
///
/// 代际只在重建成功后推进：`rebuild_target` 返回 `false`（设备健康但还有在飞工作）时保持
/// 未消费，下一轮重试，恢复不会丢。
///
/// 设备健康但目标**暂时**建不起来（唤醒 / 显示模式切换进行中）不退出应用：退避后重试，最多
/// [`DISPLAY_REBUILD_ATTEMPTS`] 次；只有 `GetDeviceRemovedReason()` 判定真的丢了设备才立刻
/// 冒泡（⇒ GPU 循环走既有的 `GpuCell::recover`），或重试耗尽后按粘性错误上报。
fn recover_display_change(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    state: &mut DisplayRecovery,
) -> Result<bool, EngineError> {
    let epoch = display::epoch();
    if epoch == state.seen {
        return Ok(false);
    }
    // 新一轮恢复（代际变了）：重试计数从零开始。
    if state.pending != Some(epoch) {
        state.pending = Some(epoch);
        state.attempts = 0;
    }
    match renderer.rebuild_target() {
        Ok(true) => {}
        // 设备健康但本窗口还有在飞工作：下一轮再试（不消耗代际）。
        Ok(false) => return Ok(false),
        Err(error) => {
            // R08-3 的判据：只有真正的设备移除才升级成粘性错误（走既有的设备恢复）。
            if let Some(lost) = renderer.check_device_lost() {
                return Err(lost);
            }
            state.attempts += 1;
            if state.attempts > DISPLAY_REBUILD_ATTEMPTS {
                return Err(error);
            }
            // 退避：显示模式切换在唤醒后还要几百毫秒才稳定，立刻重试只会连续失败。
            std::thread::sleep(DISPLAY_REBUILD_BACKOFF);
            return Ok(false);
        }
    }
    state.seen = epoch;
    state.pending = None;
    state.attempts = 0;
    // 目标刚被重建（persist / back buffer 都是空的）⇒ UI 从下一帧开始整帧重发：
    // `FrameSink::reset_epoch` +1 走既有的 `ResyncGate` 握手，恢复后的第一帧自包含。
    shared.note_renderer_rebuilt();
    crate::render::windows::diagnostics::display_recovered();
    Ok(true)
}

fn run_gpu_loop(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
    composition_recovery: &mut CompositionRecovery,
) -> Result<(), EngineError> {
    // 已从共享槽取走、但还没被本帧命令列表消费的图集上传（按 `AtlasKind`）。共享槽标志
    // 要等这里清，UI 才不会在 `Retry` / Idle 帧覆盖掉未录制的上传（DEVIATIONS F-211）。
    let mut atlas_awaiting = [false; 2];
    // 显示 / 电源恢复（R07-14）：平台层推进的进程级代际，本线程消费一次；失败重试与退避
    // 都绑定在代际上（换代际重新计数）。
    let mut display_recovery = DisplayRecovery::new();
    // 清退避（一次性）：恢复后的第一帧不因「不可见 / 被遮挡」退避，见 `recover_display_change`。
    let mut force_present = false;
    // A4：`presented_at` 的证据采样状态（周期实测 + QPC 锚点；GPU 线程独占）。
    let mut statistics = FrameStatisticsClock::new();
    loop {
        // 完成通知的唯一处理点：按提交顺序发布反馈 + 推进 retire（§4.1 / §6.5）。
        drain_completed(renderer, shared, submitted, &mut statistics);
        if shared.shutdown_requested() {
            return Ok(());
        }
        // C2：泵的几何 pass 消费了 `WM_PAINT` 健康检查代际、发现 DComp 设备失效 ⇒ 请求 GPU 线程
        // 重建 visual tree（D3D12 设备 / 交换链 / persist 全部保留，空窗 ≤1–2 帧）。这里再读一次
        // 权威状态：请求可能已过期（DWM 恢复），只有仍然 Invalid 才真的重建。
        if shared.take_composition_rebuild_request() {
            match renderer.composition_state() {
                CompositionState::Valid | CompositionState::WindowClosed => {}
                CompositionState::Invalid => {
                    if !composition_recovery.allow_rebuild() {
                        return Err(composition_rebuild_exhausted());
                    }
                    renderer.rebuild_composition()?;
                    crate::render::windows::diagnostics::composition_recovered();
                    shared.note_renderer_rebuilt();
                    // 重建后 DWM 需要一次 present 才显示内容：本帧不吃遮挡退避。
                    force_present = true;
                }
            }
        }
        // 显示 / 电源恢复：拓扑 / 工作区 / 唤醒之后重建交换链（⇒ reset_epoch 让 UI 整帧重发）
        // 并清一次遮挡退避。
        if recover_display_change(renderer, shared, &mut display_recovery)? {
            force_present = true;
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
        //
        // R07-14：显示 / 电源恢复之后的**第一帧**例外（`force_present`）—— 锁屏 / RDP 断连 /
        // 睡眠回来时 `IsIconic` / `DWMWA_CLOAKED` 可能仍是陈旧值，若照常退避，「唤醒后出帧」
        // 就要等用户再碰一下窗口，恢复等于没做。
        //
        // RBT-10：这一轮是否跳过渲染与「是否参与图集扩层闸门」由**同一个函数**决定
        // （`Shared::publish_atlas_participation`）：跳过 ⇒ 不参与（本循环不会 `take_atlas`，
        // 待上传位会一直亮着，计入 `atlas_idle` 门禁会永久挡住其它窗口的扩层）。
        if shared.publish_atlas_participation(renderer.window_occluded(), force_present) {
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
        // 恢复后的第一帧要在**进入这次呈现尝试时**就清掉退避：遮挡时 `Present1` 会报告
        // `DXGI_STATUS_OCCLUDED`（是时序现象，不是错误），此时若无条件重试会变成
        // 「每秒一次 waitable 超时」的空转 —— 只放行一次，之后回到正常退避。
        force_present = false;
        // I3：先等呈现节奏，再取「此刻最新」的帧 —— 等待期间 UI 的发布覆盖旧请求并合并 damage，
        // 渲染的是最新快照而不是最多等了一个 vsync 的旧快照（§4.2 的「帧是状态不是队列」）。
        if !wait_for_waitable(renderer, shared, submitted, &mut statistics)? {
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
        drain_completed(renderer, shared, submitted, &mut statistics);
        // 等 waitable 期间到达的背景色：与帧一起落地（在录制前换掉 1×1 背景层，不引入新的
        // GPU 调用点）。
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba);
            renderer.refresh_background()?;
        }
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2；像素按所有权移交给 GPU）。
        // 一次 take 里可能有多个层（图片图集逐层上传），同一提交内聚合录制。
        for upload in shared.take_atlas() {
            atlas_awaiting[upload.kind.index() as usize] = true;
            renderer.queue_atlas_upload(
                upload.kind,
                upload.layer,
                upload.spec,
                upload.pixels,
                upload.rects,
            );
        }
        // 内容尺寸变化：buffer 内是纯 CPU 记账（内容 visual 恒 1:1；放大露出的区域由 1×1
        // 背景层填充，UI 只改内容 clip / 背景 scale）；内容越过 buffer 桶时在这里重建持久目标
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
            drain_completed(renderer, shared, submitted, &mut statistics);
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
                    // A4：present 之后立即采样帧统计。此刻「最后一次已上屏内容」必然早于本帧
                    // ⇒ 采样结果恒为本帧真实上屏时刻的下界（见 `FrameStatisticsClock` 文档）。
                    let presented = statistics.observe(renderer.frame_statistics());
                    submitted.push_back(SubmittedFrame {
                        index: request.index,
                        stats,
                        samples,
                        published_at: request.published_at,
                        presented,
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

/// 发布已观察到完成的帧（提交顺序）并推进 retire（A4：`presented_at` 来自帧统计下界证据）。
fn drain_completed(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
    statistics: &mut FrameStatisticsClock,
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
        // A4：本帧的上屏时刻下界 = max(present 后采样, 完成观测点采样)。
        //
        // 两个采样点都满足「最后一次已上屏内容不晚于本帧」：
        // - present 后：本帧刚提交，尚未上屏；
        // - 完成观测点（仅当 `submitted` 已空 = 本帧是最后一次 present）：此后的 present 都还
        //   没有，所以「最后已上屏」要么是本帧、要么更早——两种情况下都 ≤ 本帧真实上屏时刻。
        // 其中 present 后采样的「最后已上屏」严格早于本帧，所以它单独就已经是下界；取 max 只会
        // 更紧，不会越过真实值。
        let mut presented = frame.presented;
        if submitted.is_empty()
            && let Some(refreshed) = statistics.observe(renderer.frame_statistics())
        {
            presented = Some(presented.map_or(refreshed, |known| known.max(refreshed)));
        }
        // 契约：有值 ⇒ 不早于 GPU 完成（真实上屏不可能比 GPU 工作更早；两个下界取 max 仍是下界）。
        let presented_at = clamp_presented_at(presented, completed_at);
        shared.publish_feedback(FrameFeedback {
            index: frame.index,
            stats: frame.stats,
            samples,
            completed_at,
            // UI 发布 → GPU 完成（HUD 的 publish→present）。
            latency: completed_at.saturating_duration_since(frame.published_at),
            // 上屏时刻的下界（`None` = 本拍拿不到可信统计；绝不拿 GPU 完成时刻顶替）。
            presented_at,
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
    statistics: &mut FrameStatisticsClock,
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
                drain_completed(renderer, shared, submitted, statistics);
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
/// 显示 / 电源恢复时目标重建的失败重试上限（R07-14）。
///
/// 唤醒后显示模式可能还要几百毫秒才稳定，一次重建失败不代表设备不可用；超过上限才把错误
/// 升级成粘性、交给 UI（真正丢了设备的情况在此之前就被 `GetDeviceRemovedReason` 判出来了）。
const DISPLAY_REBUILD_ATTEMPTS: u32 = 5;
/// 上面那个重试之间的退避。
const DISPLAY_REBUILD_BACKOFF: Duration = Duration::from_millis(200);
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
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    };

    use smol_str::SmolStr;

    use super::{
        AtlasSlots, CompositionRecovery, DisplayRecovery, FatalDisposition, FrameSink, PanicGuard,
        Shared, State, StickyError, WindowFrameSink, fatal_disposition, pack_size,
        recover_display_change, unpack_size,
    };
    use crate::{
        error::EngineError,
        paint::atlas::AtlasKind,
        render::{
            DirtyRects,
            compositor::Plan,
            frame::LatestFrame,
            windows::{compositor::GpuRenderer, display},
        },
    };

    /// 图集 array 规格（测试页 + 层数）。
    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
    }

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

    #[test]
    fn a_first_atlas_upload_keeps_its_dirty_rects() {
        let mut slots = AtlasSlots::new();
        slots.queue(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            vec![1, 2, 3],
            dirty_rects(4, 8),
        );
        slots.queue(
            AtlasKind::Image,
            1,
            spec(2048, 2),
            vec![4, 5],
            dirty_rects(0, 2),
        );
        let mut uploads = slots.take();
        assert_eq!(uploads.len(), 2, "字形 + 图片第 1 层");
        let image = uploads.pop().expect("image upload");
        assert_eq!(image.kind, AtlasKind::Image);
        assert_eq!(image.layer, 1);
        assert_eq!(image.spec, spec(2048, 2));
        assert_eq!(image.pixels, vec![4, 5]);
        assert_eq!(image.rects, dirty_rects(0, 2));
        let glyph = uploads.pop().expect("glyph upload");
        assert_eq!(glyph.layer, 0);
        assert_eq!(glyph.spec, spec(64, 1));
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
                spec(2048, 4),
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
        slots.queue(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            vec![1, 2, 3],
            dirty_rects(4, 8),
        );
        slots.queue(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            vec![9, 9, 9],
            dirty_rects(16, 20),
        );
    }

    /// `atlas_pending` 只能在**录制完成**后清：`take_atlas` 只把上传搬给 GPU 线程，清早了
    /// UI 会在本帧未录制时入队下一笔并覆盖它（DEVIATIONS F-211 的丢行回归面）。
    #[test]
    fn an_atlas_upload_stays_busy_until_it_is_recorded() {
        let shared = test_shared();
        assert!(shared.atlas_idle(AtlasKind::Glyph));
        assert!(shared.queue_atlas(
            AtlasKind::Glyph,
            0,
            spec(64, 1),
            &[1, 2, 3],
            dirty_rects(4, 8)
        ));

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
                spec(2048, 4),
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
            reset_epoch: AtomicU64::new(0),
            composition_pending: AtomicBool::new(false),
            composition_rebuild: AtomicBool::new(false),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
            atlas_participating: AtomicBool::new(true),
        })
    }

    /// RBT-14：真实接线同源 —— 「本轮跳过渲染」与「不参与图集扩层闸门」由**同一个判据**
    /// 发布（`Shared::publish_atlas_participation`，GPU 循环只调用它）。
    ///
    /// 覆盖三种组合：遮挡+非 force_present ⇒ 跳过且不参与；可见 ⇒ 不跳过且参与；
    /// 遮挡+force_present（R07-14 恢复放行）⇒ 不跳过且参与（与 `take_frame` 一致）。
    ///
    /// 变异 M19（删掉 false 写入 / 恒写 true）⇒ 本测试红。
    #[test]
    fn atlas_participation_follows_the_same_occlusion_decision() {
        let shared = test_shared();
        assert!(
            shared.publish_atlas_participation(true, false),
            "遮挡且非 force_present ⇒ 跳过渲染"
        );
        assert!(
            !shared.atlas_participating(),
            "跳过的窗口必须不参与扩层闸门（否则待上传位永久挡住其它窗口）"
        );
        assert!(
            !shared.publish_atlas_participation(false, false),
            "可见 ⇒ 不跳过"
        );
        assert!(shared.atlas_participating(), "可见窗口必须参与扩层闸门");
        assert!(
            !shared.publish_atlas_participation(true, true),
            "force_present 放行 ⇒ 不跳过（恢复后的第一帧必须出帧）"
        );
        assert!(
            shared.atlas_participating(),
            "放行的那一帧同样参与（与 take_frame 的实际行为一致）"
        );
    }

    /// AR4 P1-1（GPU 侧）：渲染器重建（设备恢复）必须让 UI 通过 `FrameSink::reset_epoch` 看到
    /// 新代际；没有重建时保持 0（Linux / macOS 的默认实现同义：恒 0）。
    ///
    /// UI 侧据此整帧重发实例缓冲（`Window::resync` → `DeltaBuilder::force_full`）；这条链路的
    /// 全量协议验收在 `render::compose` 的镜像字节一致性用例里。
    #[test]
    fn a_renderer_rebuild_bumps_the_reset_epoch_the_sink_exposes() {
        let shared = test_shared();
        let sink = WindowFrameSink {
            shared: Arc::clone(&shared),
        };
        assert_eq!(sink.reset_epoch(), 0, "没有重建时不得要求整帧重发");
        shared.note_renderer_rebuilt();
        assert_eq!(sink.reset_epoch(), 1, "设备恢复必须让 UI 看到新代际");
        shared.note_renderer_rebuilt();
        assert_eq!(sink.reset_epoch(), 2, "连续两次重建必须各 +1");
    }

    /// C1：GPU 线程错误边界必须先复判设备状态，再决定「恢复」还是「致命」。
    ///
    /// 真实故障（TDR 恰落在 `ResizeBuffers` 上）无法在 WARP 里自动触发，因此这里对**判据**做
    /// 穷举：`DeviceLost` 恒恢复；非 `DeviceLost` 但权威判据确认移除 ⇒ 恢复（`ResizeBuffers`
    /// 把设备级 HRESULT 包成 `Swapchain` 的路径）；健康设备上的非 `DeviceLost` 错误保持致命。
    #[test]
    fn the_fatal_boundary_rejudges_a_removed_device_before_publishing() {
        let removed = EngineError::DeviceLost {
            reason: SmolStr::new_static("DXGI_ERROR_DEVICE_REMOVED"),
            log_path: std::path::PathBuf::from("logs/dred.txt"),
        };
        assert_eq!(
            fatal_disposition(&removed, false, false),
            FatalDisposition::DeviceLost,
            "DeviceLost 本身已足够触发恢复"
        );
        assert_eq!(
            fatal_disposition(&removed, true, true),
            FatalDisposition::DeviceLost
        );

        let swapchain = EngineError::Swapchain {
            detail: SmolStr::new_static("ResizeBuffers(800x600): 0x887A0005"),
        };
        assert_eq!(
            fatal_disposition(&swapchain, false, false),
            FatalDisposition::Fatal,
            "设备健康时交换链错误必须保持致命（不无限恢复）"
        );
        assert_eq!(
            fatal_disposition(&swapchain, true, false),
            FatalDisposition::DeviceLost,
            "ResizeBuffers 在设备移除竞态下被包成 Swapchain 时仍必须恢复"
        );

        // C2：D3D12 健康但 DComp 失效 ⇒ 可恢复（只重建 visual tree），不是致命。
        let composition = EngineError::Composition {
            detail: SmolStr::new_static("Device::Commit: 0x8007000E"),
        };
        assert_eq!(
            fatal_disposition(&composition, false, true),
            FatalDisposition::CompositionInvalid
        );
        assert_eq!(
            fatal_disposition(&composition, false, false),
            FatalDisposition::Fatal
        );
        // 设备已丢失的优先级高于 DComp 失效：必须整 renderer 恢复，而不是只重建 visual tree。
        assert_eq!(
            fatal_disposition(&composition, true, true),
            FatalDisposition::DeviceLost
        );

        // review-c RC-4：DComp 恰好 Invalid **不能**把真实的致命错误吞成 visual 重建。
        // `AppInit` / `FramePipeline` / `Swapchain` 即使 `dcomp_invalid == true` 也必须保持 Fatal，
        // 否则原始错误被掩盖，持续 Invalid 后还会以「DComp 重建 3 次失败」误导归因。
        let app_init = EngineError::AppInit {
            detail: SmolStr::new_static("d3d12 persist target missing"),
        };
        assert_eq!(
            fatal_disposition(&app_init, false, true),
            FatalDisposition::Fatal,
            "AppInit 不因 DComp 恰好 Invalid 而降级成 visual 重建"
        );
        let frame_pipeline = EngineError::FramePipeline {
            detail: SmolStr::new_static("compositor thread panicked"),
        };
        assert_eq!(
            fatal_disposition(&frame_pipeline, false, true),
            FatalDisposition::Fatal
        );
        assert_eq!(
            fatal_disposition(&swapchain, false, true),
            FatalDisposition::Fatal,
            "设备健康时交换链错误必须保持致命，即使 DComp 状态异常"
        );
    }

    /// C2：DComp 重建预算 —— 窗口内最多 3 次，超限后由成功重建前的冷却时间复位。
    ///
    /// 直接跑 3 次 `allow_rebuild` 后再跑第 4 次（窗口内）必须拒绝；把窗口“拨快”到冷却之后
    /// 再允许（这里通过时间无法注入，因此用 `COMPOSITION_RECOVERY_WINDOW` 的语义断言：持续
    /// 重建在窗口内也不会无限通过 —— 三次后拒绝，说明预算生效）。
    #[test]
    fn composition_rebuilds_are_bounded_within_the_recovery_window() {
        let mut recovery = CompositionRecovery::new();
        assert!(recovery.allow_rebuild(), "第 1 次重建允许");
        assert!(recovery.allow_rebuild(), "第 2 次重建允许");
        assert!(recovery.allow_rebuild(), "第 3 次重建允许");
        assert!(
            !recovery.allow_rebuild(),
            "同一窗口内第 4 次重建必须被拒绝（否则 DWM 反复失效会变成每帧重建活锁）"
        );
        // 诊断文案必须能归因到「限次」，而不是伪装成普通 Composition 错误。
        let error = super::composition_rebuild_exhausted();
        match error {
            EngineError::Composition { detail } => {
                assert!(detail.contains("3 rebuilds"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// R07-14：平台层的显示 / 电源恢复（拓扑变化、工作区变化、睡眠唤醒）必须让 GPU 线程
    /// **重建目标**（⇒ `reset_epoch` +1，UI 整帧重发）并**清一次遮挡退避**（返回值 = 放行恢复
    /// 后的第一帧）。离屏渲染器没有交换链，重建是 no-op，但「消费代际 → 推进 reset_epoch」
    /// 这条握手与窗口路径完全相同；交换链真被替换的那半只能在真机上验证（WARP 建不了组合链）。
    #[test]
    fn a_display_change_rebuilds_the_target_and_clears_the_backoff() {
        let mut renderer = GpuRenderer::new_warp_offscreen((32, 32)).expect("warp renderer");
        let shared = test_shared();
        let sink = WindowFrameSink {
            shared: Arc::clone(&shared),
        };
        let mut recovery = DisplayRecovery::new();

        display::note_display_change();
        assert!(
            recover_display_change(&mut renderer, &shared, &mut recovery)
                .expect("display recovery"),
            "恢复后的第一帧必须放行（清遮挡退避）"
        );
        assert_eq!(sink.reset_epoch(), 1, "目标重建后 UI 必须整帧重发");
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
