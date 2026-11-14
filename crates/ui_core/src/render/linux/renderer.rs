//! 窗口渲染门面 + 每窗口一条 `kytrix-vulkan` GPU 线程
//! （`design/async-render-pipeline.md` §4.5 / §7）。
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
//! 关闭握手（§4.5 / §7.7）：`Drop` 里 signal shutdown + 有界等待（`recv_timeout`，2 s）后 join。
//! GPU 线程在退出前自己 drain 在飞帧（`GpuRenderer::drop` 的 `vkDeviceWaitIdle` 允许，退出路径）。
//!
//! WSI 线程模型（§7.5）：`vkCreateWaylandSurfaceKHR` 创建的 WSI 私有 `wl_event_queue` 完全由
//! Mesa 内部 dispatch；应用只 dispatch 自己的默认队列（calloop 的 `WaylandSource`）。

use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smol_str::{SmolStr, format_smolstr};

#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "test-support")]
use crate::render::linux::readback::ReadbackImage;
#[cfg(feature = "hud")]
use crate::render::timer::{GpuTimerSample, Section};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        DirtyRows, RenderStats,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, LatestFrame},
        linux::{
            compositor::{GpuRenderer, Submitted},
            device::enter_gpu_thread,
        },
        timer::FrameTimings,
    },
    window::FrameIndex,
};

/// 关闭握手的等待上限（UI 线程**唯一**允许等 GPU 线程的时刻，§4.5）。
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

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

    /// 测试用离屏渲染器 + 指定图集页（快照必须与 `App` 的分配器同页尺寸）。
    #[cfg(feature = "test-support")]
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

    /// 真实窗口：构造 GPU 对象（UI 线程）后整体移交给 `kytrix-vulkan` 线程。
    ///
    /// # Errors
    /// 设备 / swapchain 创建失败，或 GPU 线程无法启动（`AppInit`）。
    pub(crate) fn new_for_surface(
        display: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        size: (u32, u32),
    ) -> Result<Self, EngineError> {
        let gpu = GpuRenderer::new_for_surface(display, surface_ptr, size)?;
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
    /// GPU 线程退出信号（`ExitSignal` 在退出 / panic 时发送）。
    done: Receiver<()>,
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
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
        });
        let (done, done_rx) = mpsc::channel();
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("kytrix-vulkan".to_owned())
            .spawn(move || gpu_loop(gpu, worker, done))
            .map_err(|error| EngineError::AppInit {
                detail: format_smolstr!("spawn kytrix-vulkan thread: {error}"),
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            done: done_rx,
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
            feedback.samples.len() <= 4,
            "at most four GPU sections per frame"
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
        // UI 线程唯一允许的等待点：signal shutdown 之后有界等待 GPU 线程退出（§4.5）。
        match self.done.recv_timeout(JOIN_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                // 放弃 join（线程可能卡在驱动的等待里）：detach，避免把 UI 线程一起拖死。
                eprintln!(
                    "kytrix: the kytrix-vulkan thread did not exit within {}s; abandoning the join",
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

/// UI 线程与 GPU 线程共享的最新帧 / 图集 / 反馈槽 + 唤醒原语（`condvar.notify`）。
struct Shared {
    state: Mutex<State>,
    work: Condvar,
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

    /// 空闲：睡在 condvar 上直到有帧或关闭（0 自旋唤醒）。
    fn wait_for_work(&self) {
        let mut state = self.lock();
        while !state.shutdown && !state.frame.has_pending() {
            state = self
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
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

/// GPU 线程主循环（§7.3）。
fn gpu_loop(mut renderer: GpuRenderer, shared: Arc<Shared>, done: Sender<()>) {
    enter_gpu_thread();
    // 退出（含 panic）时通知 UI 线程；`Drop` 的 `recv_timeout` 因此有界。
    let _exit = ExitSignal(done);
    // 已从最新帧槽取出、但还没产生 GPU 工作的帧。`NOT_READY` / `OUT_OF_DATE` 时**保留**它：
    // 丢掉会把它的 damage 一起丢掉（persist 里那块像素永远不会被补画，I5）。
    let mut pending: Option<FrameRequest> = None;
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
        if shared.shutdown_requested() {
            break;
        }
        // 非阻塞推进 retire（timeline 只在这里读；旧 swapchain / 旧 persist 在此真正释放）。
        if let Err(error) = renderer.drain_completed() {
            shared.publish_error(error);
            break;
        }
        let Some(request) = pending.as_ref() else {
            shared.wait_for_work();
            continue;
        };
        // 图集像素在本帧录制前落地（上传先于绘制，§4.2）。
        for upload in shared.take_atlas().into_iter().flatten() {
            renderer.queue_atlas_upload(upload.kind, &upload.pixels, upload.rows);
        }
        // §4.4：resize 不是独立命令，`FrameRequest.surface` 就是目标尺寸。
        if request.surface != renderer.surface()
            && let Err(error) = renderer.resize(request.surface)
        {
            shared.publish_error(error);
            break;
        }
        let mut frame = FramePlan::new(request.plan, renderer.is_fresh());
        // 没有像素要画（Idle 且目标未重建）⇒ 不产生 GPU 工作；该序号不会出现在完成通知里（§4.3）。
        if frame.is_idle() {
            pending = None;
            continue;
        }
        match renderer.submit_frame(request, &mut frame) {
            Ok(Submitted::Presented { stats, samples }) => {
                shared.publish_feedback(FrameFeedback {
                    index: request.index,
                    stats,
                    samples,
                    // UI 发布 → GPU 完成（当前发布点在 present 返回后）。
                    latency: Instant::now().saturating_duration_since(request.published_at),
                    // 命令缓冲完成时刻（近似呈现时刻）。
                    completed_at: Instant::now(),
                });
                pending = None;
            }
            // 保留待画帧：下一次 acquire 会阻塞到超时 / 下一张图，不会自旋。
            Ok(Submitted::Retry) => {}
            Err(error) => {
                shared.publish_error(error);
                break;
            }
        }
    }
    shared.mark_terminated();
    // 离开作用域时 `GpuRenderer` 被 drop：`vkDeviceWaitIdle` → 销毁 swapchain / persist / 设备
    // （§7.7 的退出路径）。
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
            published_at: std::time::Instant::now(),
        };
        frame.publish(request(1, Plan::Full));
        frame.publish(request(2, Plan::Idle));
        assert_eq!(frame.take().expect("frame").plan, Plan::Full);
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
}
