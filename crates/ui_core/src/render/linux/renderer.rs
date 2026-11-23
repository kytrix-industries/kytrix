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

const FEEDBACK_POLL: Duration = Duration::from_millis(4);

pub(crate) struct Renderer {
    target: RenderTarget,
    error: Option<StickyError>,
}

enum RenderTarget {
    Offscreen(GpuRenderer),
    Window(WindowRenderer),
}

impl Renderer {
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_offscreen(size)?),
            error: None,
        })
    }

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

    pub(crate) fn surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.surface,
        }
    }

    pub(crate) fn note_published_surface(&mut self, surface: (u32, u32)) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.surface = surface;
        }
    }

    pub(crate) fn frame_sink(&self) -> Option<Arc<dyn FrameSink>> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(Arc::new(WindowFrameSink {
                shared: Arc::clone(&window.shared),
            })),
        }
    }

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

    pub(crate) fn atlas_idle(&self, kind: AtlasKind) -> bool {
        match &self.target {
            RenderTarget::Offscreen(gpu) => !gpu.atlas_pending(kind),
            RenderTarget::Window(window) => window.atlas_idle(kind),
        }
    }

    pub(crate) fn atlas_participating(&self) -> bool {
        true
    }

    pub(crate) fn set_background_color(&mut self, color: Color) {
        match &self.target {
            RenderTarget::Offscreen(_) => {}
            RenderTarget::Window(window) => window.set_background_color(color),
        }
    }

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
    surface: (u32, u32),
    stats: RenderStats,
    #[cfg(feature = "hud")]
    samples: Vec<GpuTimerSample>,
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
        spec: crate::render::AtlasArraySpec,
        pixels: &[u8],
        rects: DirtyRects,
    ) -> bool {
        self.shared.queue_atlas(kind, layer, spec, pixels, rects)
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.shared.atlas_idle(kind)
    }

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
        let _ = thread.join();
    }
}

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

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    atlas_pending: [AtomicU32; 2],
}

struct State {
    frame: LatestFrame,
    atlas: AtlasSlots,
    feedback: Option<FrameFeedback>,
    error: Option<EngineError>,
    shutdown: bool,
    terminated: bool,
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

    fn has_work(&self) -> bool {
        self.shutdown || self.frame.has_pending() || self.background.is_some()
    }
}

struct AtlasSlots {
    glyph: Option<AtlasUpload>,
    image: [Option<AtlasUpload>; MAX_IMAGE_PAGES as usize],
}

struct AtlasUpload {
    kind: AtlasKind,
    layer: u16,
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

    fn take(&mut self) -> Vec<AtlasUpload> {
        let mut uploads = Vec::with_capacity(1 + usize::from(MAX_IMAGE_PAGES));
        uploads.extend(self.glyph.take());
        uploads.extend(self.image.iter_mut().filter_map(Option::take));
        uploads
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self) {
        self.work.notify_all();
    }

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
        let pixels = pixels.to_vec();
        self.atlas_pending[index].fetch_or(bit, Ordering::Release);
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            self.atlas_pending[index].fetch_and(!bit, Ordering::Release);
            return false;
        }
        state.atlas.queue(kind, layer, spec, pixels, rects);
        true
    }

    fn set_background(&self, linear: [f32; 4]) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.background = Some(linear);
        drop(state);
        self.wake();
    }

    fn take_background(&self) -> Option<[f32; 4]> {
        self.lock().background.take()
    }

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
        self.lock().atlas.take()
    }

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

struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
    published_at: Instant,
    sequence: u64,
    slot: usize,
}

fn gpu_loop(renderer: GpuRenderer, shared: Arc<Shared>, presentation: Option<PresentationHandle>) {
    enter_gpu_thread();
    let guard = PanicGuard::new(Arc::clone(&shared));
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
    let mut pending: Option<FrameRequest> = None;
    let mut feedback_requested: Option<FrameIndex> = None;
    let mut atlas_awaiting = [false; 2];
    loop {
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
        drain_completed(renderer, shared, submitted)?;
        if shared.shutdown_requested() {
            return Ok(());
        }
        if let Some(linear) = shared.take_background() {
            renderer.set_clear_color(linear);
        }
        let Some(request) = pending.as_ref() else {
            shared.wait_for_work(!submitted.is_empty());
            continue;
        };
        if let Some(linear) = shared.take_background() {
            renderer.set_clear_color(linear);
        }
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
        if request.surface != renderer.surface() {
            renderer.resize(request.surface)?;
        }
        let mut frame = FramePlan::new(request.plan, renderer.is_fresh());
        if frame.is_idle() {
            pending = None;
            continue;
        }
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

fn drain_completed(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
) -> Result<(), EngineError> {
    let completed = renderer.drain_completed()?;
    for frame in completed_frames(submitted, completed) {
        let mut samples = frame.samples;
        if let Some(sample) = renderer.resolve_frame_timer(frame.slot) {
            samples.push(sample);
        }
        let completed_at = Instant::now();
        shared.publish_feedback(FrameFeedback {
            index: frame.index,
            stats: frame.stats,
            samples,
            completed_at,
            latency: completed_at.saturating_duration_since(frame.published_at),
            presented_at: None,
        });
    }
    Ok(())
}

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
        math::dp,
        paint::atlas::AtlasKind,
        render::{DirtyRects, RenderStats, compositor::Plan, frame::LatestFrame},
        window::FrameIndex,
    };

    fn dirty_rects(y0: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(crate::math::DeviceRect::new(
            dp(0),
            dp(y0 as i32),
            dp(i32::MAX),
            dp(y1 as i32),
        ));
        rects
    }

    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
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
        assert_eq!(glyph.kind, AtlasKind::Glyph);
        assert_eq!(glyph.layer, 0);
        assert_eq!(glyph.spec, spec(64, 1));
        assert_eq!(glyph.pixels, vec![1, 2, 3]);
        assert_eq!(glyph.rects, dirty_rects(4, 8));
        assert!(slots.take().is_empty(), "取走后槽位为空");
    }

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
    fn a_background_change_is_taken_exactly_once() {
        let shared = Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
        };
        assert!(!shared.lock().has_work(), "空槽不是 GPU 线程的工作");

        shared.set_background([0.25, 0.5, 0.75, 1.0]);
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
        let mut frame = LatestFrame::new();
        assert!(!frame.has_pending());
        let request = |index: u32, plan: Plan| crate::render::frame::FrameRequest {
            index: crate::window::FrameIndex::from_raw(index),
            payload: crate::render::frame::FramePayload::empty(),
            plan,
            surface: (1, 1),
            scale: 1.0,
            surface_local: crate::math::LogicalSize::new(px(1.0), px(1.0)),
            buffer_scale: 1,
            published_at: std::time::Instant::now(),
        };
        frame.publish(request(1, Plan::Full));
        frame.publish(request(2, Plan::Idle));
        assert_eq!(frame.take().expect("frame").plan, Plan::Full);
    }

    #[test]
    fn only_frames_whose_timeline_value_completed_are_published() {
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
