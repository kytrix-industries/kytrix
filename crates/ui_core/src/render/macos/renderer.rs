//! 窗口渲染线程（`design/macos-verification.md` V-114 / V-119）。
//!
//! 主线程只做 UI：布局、绘制场景编码、把一帧的实例/裁剪/绘制序复制成 [`FramePayload`]，
//! 然后投进最新帧邮箱。`GpuRenderer`（设备、图层、命令队列、持久纹理）由 worker 独占，
//! `CAMetalLayer::nextDrawable`、命令编码、`presentDrawable` 都不再经过主线程。
//!
//! 两个关键语义：
//! - **最新帧邮箱**：worker 忙时新帧替换旧帧，并把替代帧提升为全量；被替换的旧帧没有画进
//!   persist，只有全量帧才能保证它的像素不丢。
//! - **拿不到 drawable 不丢帧**：worker 把 job 放回邮箱（强制全量）后短暂等待重试；期间新帧
//!   到来则用新帧替换。这样合成器暂停/恢复不会让 damage 永久丢失。
//!
//! 离屏/快照渲染器不建线程：`Renderer::new_offscreen` 仍走同步路径，测试语义不变。

use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Duration,
};

use objc2::rc::Retained;
use objc2_quartz_core::CAMetalLayer;
use smol_str::format_smolstr;

#[cfg(any(test, feature = "test-support"))]
use crate::render::macos::readback::ReadbackImage;
#[cfg(feature = "hud")]
use crate::render::timer::Section;
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        DirtyRows, RenderStats,
        compositor::{FramePlan, Plan},
        macos::compositor::{FramePayload, GpuRenderer},
        timer::{FrameTimings, GpuTimerSample},
    },
    window::FrameIndex,
};

/// 拿不到 drawable 后的重试间隔。`allowsNextDrawableTimeout` 本身已经会让
/// `nextDrawable` 最多等约 1s，这里只是让关掉的屏幕/暂停的合成器不要空转。
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// macOS 渲染器门面：窗口路径是 render 线程句柄，离屏路径是同步 GPU 渲染器。
pub(crate) struct Renderer {
    target: RenderTarget,
}

enum RenderTarget {
    Offscreen(GpuRenderer),
    Window(WindowRenderer),
}

impl Renderer {
    /// 产品路径（离屏 / 快照）：同步渲染器，不建线程。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_offscreen(size)?),
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
        })
    }

    /// 真实窗口：构造 GPU 渲染器与 `CAMetalLayer`（主线程），随后整体移交给 worker。
    pub(crate) fn new_for_layer(size: (u32, u32), scale: f64) -> Result<Self, EngineError> {
        let gpu = GpuRenderer::new_for_layer(size, scale)?;
        let layer = gpu.layer_handle().ok_or_else(|| EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("metal renderer has no CAMetalLayer"),
        })?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(gpu, layer, size, scale)?),
        })
    }

    /// 绘图层（只给 `NSView::setLayer` 用；绘制属性由 worker 写）。
    pub(crate) fn layer_handle(&self) -> Option<Retained<CAMetalLayer>> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(window.layer.clone()),
        }
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.surface,
        }
    }

    pub(crate) fn is_fresh(&self) -> bool {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.is_fresh(),
            RenderTarget::Window(window) => window.fresh,
        }
    }

    /// 主线程只更新门面的目标尺寸；真正的 `persist` 重建在 worker 的下一帧前完成。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => gpu.resize(size),
            RenderTarget::Window(window) => {
                window.resize(size);
                Ok(())
            }
        }
    }

    /// DPI 变化：主线程只记下新 scale，图层属性由 worker 更新。
    pub(crate) fn set_scale(&mut self, scale: f64) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_scale(scale);
        }
    }

    /// 把图集页交给 worker；与帧消息同一个邮箱，顺序与主线程调用顺序一致。
    pub(crate) fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => gpu.queue_atlas_upload(kind, pixels, rows),
            RenderTarget::Window(window) => window.queue_atlas_upload(kind, pixels, rows),
        }
    }

    pub(crate) fn poll(&mut self) -> Result<(), EngineError> {
        match &mut self.target {
            RenderTarget::Offscreen(_) => Ok(()),
            RenderTarget::Window(window) => window.poll(),
        }
    }

    pub(crate) fn submit(
        &mut self,
        index: FrameIndex,
        buffer: &crate::paint::instance::InstanceBuffer,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => gpu.submit(index, buffer, frame, timings),
            RenderTarget::Window(window) => window.submit(index, buffer, frame, timings),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &crate::paint::instance::InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        match &mut self.target {
            RenderTarget::Offscreen(gpu) => gpu.render_snapshot(buffer),
            RenderTarget::Window(_) => Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "render_snapshot requires an offscreen renderer (window renderer is threaded)",
                ),
            }),
        }
    }

    /// HUD 的分段计时：窗口路径来自 worker 最近完成的一帧。
    #[cfg(feature = "hud")]
    pub(crate) fn timing_sample(&self, section: Section) -> Option<GpuTimerSample> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => window.timing_sample(section),
        }
    }
}

/// worker 侧一帧：所有字段都是主线程复制/构造的纯数据。
struct FrameJob {
    index: FrameIndex,
    payload: FramePayload,
    plan: FramePlan,
    surface: (u32, u32),
    scale: f64,
}

/// worker 完成一帧后的反馈；主线程在 `poll` 中取走。
struct FrameResult {
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
}

#[derive(Default)]
struct Feedback {
    latest: Option<FrameResult>,
    error: Option<EngineError>,
}

struct MailboxState {
    controls: VecDeque<AtlasUpload>,
    frame: Option<FrameJob>,
    shutdown: bool,
    feedback: Feedback,
}

/// 主线程与 render 线程共享的控制/最新帧邮箱。
struct Mailbox {
    state: Mutex<MailboxState>,
    wake: Condvar,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                controls: VecDeque::new(),
                frame: None,
                shutdown: false,
                feedback: Feedback::default(),
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MailboxState> {
        // 锁只在极短的入队/出队区间持有；panic 恢复后状态仍可继续工作，不需要二次 panic。
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push_atlas(&self, upload: AtlasUpload) {
        self.lock().controls.push_back(upload);
        self.wake.notify_one();
    }

    fn submit(&self, job: FrameJob) {
        let mut state = self.lock();
        if state.shutdown {
            return;
        }
        let replaced = state.frame.replace(job);
        if replaced.is_some()
            && let Some(pending) = state.frame.as_mut()
        {
            // 被替换的帧没有画进 persist：替代帧必须整面重画。
            pending.plan = FramePlan::new(Plan::Full, true);
        }
        drop(state);
        self.wake.notify_one();
    }

    fn requeue(&self, mut job: FrameJob) {
        job.plan = FramePlan::new(Plan::Full, true);
        let mut state = self.lock();
        if state.shutdown {
            return;
        }
        if let Some(pending) = state.frame.as_mut() {
            // 旧帧没拿到 drawable，而更新的帧已在邮箱里：更新帧也必须全量。
            pending.plan = FramePlan::new(Plan::Full, true);
        } else {
            state.frame = Some(job);
        }
        drop(state);
        self.wake.notify_one();
    }

    fn publish_frame(&self, result: FrameResult) {
        self.lock().feedback.latest = Some(result);
    }

    fn publish_error(&self, error: EngineError) {
        let mut state = self.lock();
        state.feedback.latest = None;
        state.feedback.error = Some(error);
    }

    fn take_feedback(&self) -> Feedback {
        std::mem::take(&mut self.lock().feedback)
    }

    fn take_work(&self) -> Work {
        let mut state = self.lock();
        while !state.shutdown && state.controls.is_empty() && state.frame.is_none() {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        Work {
            controls: state.controls.drain(..).collect(),
            frame: state.frame.take(),
            shutdown: state.shutdown,
        }
    }

    /// 拿不到 drawable 后等到「新控制 / 关闭 / 超时」，避免 60fps 空转。
    ///
    /// 注意：requeue 之后 `frame` 必然为 `Some`，所以不能拿它当「已有新工作」的提前返回条件。
    fn wait_retry(&self) {
        let state = self.lock();
        if state.shutdown || !state.controls.is_empty() {
            return;
        }
        let _ = self
            .wake
            .wait_timeout(state, RETRY_INTERVAL)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    fn shutdown(&self) {
        self.lock().shutdown = true;
        self.wake.notify_all();
    }
}

struct Work {
    controls: Vec<AtlasUpload>,
    frame: Option<FrameJob>,
    shutdown: bool,
}

struct AtlasUpload {
    kind: AtlasKind,
    pixels: Vec<u8>,
    rows: DirtyRows,
}

/// 窗口渲染器的主线程句柄。
struct WindowRenderer {
    mailbox: Arc<Mailbox>,
    worker: Option<JoinHandle<()>>,
    layer: Retained<CAMetalLayer>,
    surface: (u32, u32),
    scale: f64,
    fresh: bool,
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
}

impl WindowRenderer {
    fn new(
        gpu: GpuRenderer,
        layer: Retained<CAMetalLayer>,
        size: (u32, u32),
        scale: f64,
    ) -> Result<Self, EngineError> {
        let mailbox = Arc::new(Mailbox::new());
        let worker_mailbox = Arc::clone(&mailbox);
        let worker = thread::Builder::new()
            .name("kytrix-metal".to_owned())
            .spawn(move || worker_loop(gpu, worker_mailbox))
            .map_err(|error| EngineError::AppInit {
                detail: format_smolstr!("spawn metal render thread: {error}"),
            })?;
        Ok(Self {
            mailbox,
            worker: Some(worker),
            layer,
            surface: (size.0.max(1), size.1.max(1)),
            scale,
            fresh: true,
            stats: RenderStats::default(),
            samples: Vec::new(),
        })
    }

    fn resize(&mut self, size: (u32, u32)) {
        let size = (size.0.max(1), size.1.max(1));
        if size != self.surface {
            self.surface = size;
            self.fresh = true;
        }
    }

    fn set_scale(&mut self, scale: f64) {
        if (scale - self.scale).abs() > f64::EPSILON {
            self.scale = scale;
            self.fresh = true;
        }
    }

    fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        self.mailbox.push_atlas(AtlasUpload {
            kind,
            pixels: pixels.to_vec(),
            rows,
        });
    }

    fn poll(&mut self) -> Result<(), EngineError> {
        let feedback = self.mailbox.take_feedback();
        if let Some(error) = feedback.error {
            return Err(error);
        }
        if let Some(result) = feedback.latest {
            self.stats = result.stats;
            self.samples = result.samples;
            self.fresh = false;
        }
        Ok(())
    }

    fn submit(
        &mut self,
        index: FrameIndex,
        buffer: &crate::paint::instance::InstanceBuffer,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        self.poll()?;
        // 本帧的 GPU 采样在 worker 完成后才拿得到；主线程不再等待，HUD 读
        // `timing_sample()`（最近完成帧），`timings` 只清空以免显示过期数据。
        timings.reset(index);
        let plan = *frame;
        if plan.is_idle() {
            return Ok(self.stats);
        }
        let payload = FramePayload::from_buffer(buffer);
        self.mailbox.submit(FrameJob {
            index,
            payload,
            plan,
            surface: self.surface,
            scale: self.scale,
        });
        Ok(self.stats)
    }

    #[cfg(feature = "hud")]
    fn timing_sample(&self, section: Section) -> Option<GpuTimerSample> {
        self.samples
            .iter()
            .find(|sample| sample.section == section)
            .copied()
    }
}

impl Drop for WindowRenderer {
    fn drop(&mut self) {
        self.mailbox.shutdown();
        if let Some(worker) = self.worker.take() {
            // worker 可能正停在 nextDrawable（超时约 1s）；这里等它退出，避免图层/设备
            // 在命令缓冲仍被引用时释放。
            let _ = worker.join();
        }
    }
}

fn worker_loop(mut renderer: GpuRenderer, mailbox: Arc<Mailbox>) {
    loop {
        let work = mailbox.take_work();
        if work.shutdown {
            break;
        }
        for upload in work.controls {
            renderer.queue_atlas_upload(upload.kind, &upload.pixels, upload.rows);
        }
        let Some(mut job) = work.frame else {
            continue;
        };
        let result = objc2::rc::autoreleasepool(|_| submit_job(&mut renderer, &mut job));
        match result {
            Ok(Some(result)) => mailbox.publish_frame(result),
            Ok(None) => {
                mailbox.requeue(job);
                mailbox.wait_retry();
            }
            Err(error) => {
                mailbox.publish_error(error);
                break;
            }
        }
    }
}

/// 在 worker 上执行一帧；`None` = 没拿到 drawable，调用方保留 job 重试。
fn submit_job(
    renderer: &mut GpuRenderer,
    job: &mut FrameJob,
) -> Result<Option<FrameResult>, EngineError> {
    if renderer.prepare_frame(job.surface, job.scale)? {
        // 尺寸/scale 变化时主线程可能只看到 partial：worker 必须整面重画。
        job.plan = FramePlan::new(Plan::Full, true);
    }
    let mut timings = FrameTimings::new();
    let Some(stats) =
        renderer.submit_payload(job.index, &job.payload, &mut job.plan, &mut timings)?
    else {
        return Ok(None);
    };
    Ok(Some(FrameResult {
        stats,
        samples: timings.samples().to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{FrameJob, FramePayload, Mailbox};
    use crate::{
        paint::atlas::AtlasKind,
        render::{
            DirtyRows,
            compositor::{DamageRects, FramePlan, Plan},
        },
        window::FrameIndex,
    };

    fn job(index: u32, plan: Plan) -> FrameJob {
        FrameJob {
            index: FrameIndex::from_raw(index),
            payload: FramePayload::empty(),
            plan: FramePlan::new(plan, false),
            surface: (1, 1),
            scale: 1.0,
        }
    }

    #[test]
    fn replacing_a_pending_frame_forces_full() {
        let mailbox = Mailbox::new();
        let partial = Plan::Partial(DamageRects::new());
        mailbox.submit(job(1, partial));
        mailbox.submit(job(2, partial));
        let frame = mailbox.take_work().frame.expect("pending frame");
        assert_eq!(frame.index.get(), 2);
        // 旧帧未画进 persist，替代帧必须全量。
        assert!(frame.plan.is_full());
    }

    #[test]
    fn shutdown_wakes_a_waiting_worker() {
        let mailbox = Arc::new(Mailbox::new());
        let worker_mailbox = Arc::clone(&mailbox);
        let worker = std::thread::spawn(move || worker_mailbox.take_work().shutdown);
        std::thread::sleep(std::time::Duration::from_millis(20));
        mailbox.shutdown();
        assert!(worker.join().expect("worker must finish"));
    }

    #[test]
    fn atlas_control_survives_frame_replacement() {
        let mailbox = Mailbox::new();
        // 控制队列独立于最新帧：即使帧被替换，控制项仍保留。
        mailbox.push_atlas(super::AtlasUpload {
            kind: AtlasKind::Glyph,
            pixels: vec![1, 2, 3],
            rows: DirtyRows::default(),
        });
        mailbox.submit(job(1, Plan::Full));
        mailbox.submit(job(2, Plan::Full));
        let work = mailbox.take_work();
        assert_eq!(work.controls.len(), 1);
        assert_eq!(work.frame.expect("frame").index.get(), 2);
    }
}
