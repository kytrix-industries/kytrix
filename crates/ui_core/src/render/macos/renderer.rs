use std::{
    collections::VecDeque,
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
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
use crate::render::timer::GpuTimerSample;
#[cfg(feature = "hud")]
use crate::render::timer::Section;
use crate::{
    error::EngineError,
    paint::{
        atlas::{AtlasKind, MAX_IMAGE_PAGES},
        color::Color,
    },
    platform::dispatcher::Dispatcher,
    render::{
        DirtyRects, RenderStats,
        color::background_linear,
        compositor::FramePlan,
        frame::{FrameFeedback, FrameRequest, FrameSink, LatestFrame},
        macos::{
            compositor::GpuRenderer,
            core::GpuCore,
            device::{Device, enter_gpu_thread},
            queue::InFlight,
            swapchain,
        },
        present_evidence::{FeedbackQueue, PacingGate, pacing_work, presentation_instant},
        timer::FrameTimings,
    },
    window::FrameIndex,
};

const MAX_SURFACE_MISMATCHES: u32 = 4;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

const JOIN_TIMEOUT: Duration = Duration::from_millis(250);

const JOIN_POLL: Duration = Duration::from_millis(5);

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

    pub(crate) fn new_for_layer(
        size: (u32, u32),
        scale: f64,
        max_frame_rate: f32,
        background: Color,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        let core = GpuCore::shared()?;
        let device = Device::new()?;
        let layer = swapchain::new_layer(&device, size, scale, background)?;
        Self::with_layer(
            core,
            device,
            layer,
            size,
            max_frame_rate,
            background,
            dispatcher,
        )
    }

    pub(crate) fn new_with_layer(
        layer: Retained<CAMetalLayer>,
        size: (u32, u32),
        scale: f64,
        max_frame_rate: f32,
        background: Color,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        let core = GpuCore::shared()?;
        let device = Device::new()?;
        swapchain::reuse_layer(&layer, &device, size, scale, background)?;
        Self::with_layer(
            core,
            device,
            layer,
            size,
            max_frame_rate,
            background,
            dispatcher,
        )
    }

    fn with_layer(
        core: Arc<GpuCore>,
        device: Device,
        layer: Retained<CAMetalLayer>,
        size: (u32, u32),
        max_frame_rate: f32,
        background: Color,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
            detail: SmolStr::new_static("CAMetalDisplayLink requires the main thread"),
        })?;
        let in_flight = Arc::new(InFlight::new());
        let shared = Arc::new(Shared::new(Arc::clone(&in_flight)));
        let gpu = GpuRenderer::new_window(
            device,
            Arc::clone(&core),
            in_flight,
            size,
            background_linear(background),
        )?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(
                mtm,
                gpu,
                core,
                layer,
                size,
                max_frame_rate,
                background,
                shared,
                dispatcher,
            )?),
            error: None,
        })
    }

    pub(crate) fn layer_handle(&self) -> Option<Retained<CAMetalLayer>> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(window.layer.clone()),
        }
    }

    pub(crate) fn gpu_core(&self) -> Option<Arc<GpuCore>> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(Arc::clone(&window.core)),
        }
    }

    pub(crate) fn background_color(&self) -> Option<Color> {
        match &self.target {
            RenderTarget::Offscreen(_) => None,
            RenderTarget::Window(window) => Some(window.background),
        }
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
                dispatcher: Arc::clone(&window.dispatcher),
            })),
        }
    }

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

    pub(crate) fn set_layer_scale(&mut self, surface: (u32, u32), scale: f32) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_layer_scale(surface, scale);
        }
    }

    pub(crate) fn set_frame_rate(&mut self, max_frame_rate: f32) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_frame_rate(max_frame_rate);
        }
    }

    pub(crate) fn set_background_color(&mut self, color: Color) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.set_background_color(color);
        }
    }

    pub(crate) fn sync_pacing(&mut self, continuous: bool) {
        if let RenderTarget::Window(window) = &mut self.target {
            window.sync_pacing(continuous);
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

const MIN_FRAME_RATE: f32 = 60.0;

struct DisplayLinkIvars {
    shared: Arc<Shared>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixMetalDisplayLink"]
    #[ivars = DisplayLinkIvars]
    struct DisplayLinkDelegate;

    impl DisplayLinkDelegate {}

    unsafe impl CAMetalDisplayLinkDelegate for DisplayLinkDelegate {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn metal_display_link_needs_update(
            &self,
            _link: &CAMetalDisplayLink,
            update: &CAMetalDisplayLinkUpdate,
        ) {
            let target = update.targetPresentationTimestamp();
            self.ivars().shared.publish_drawable(update.drawable(), target);
        }
    }
);

impl DisplayLinkDelegate {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DisplayLinkIvars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

fn reconcile_pacing(
    continuous: bool,
    gate: &mut PacingGate,
    now: Instant,
    mut sample: impl FnMut() -> (bool, usize, bool),
    mut apply: impl FnMut(bool),
) -> bool {
    let (pending, in_flight, awaiting_presentation) = sample();
    let work = pacing_work(continuous, pending, in_flight, awaiting_presentation);
    let paused = gate.update(work, now);
    apply(paused);
    if !paused {
        return false;
    }
    let (pending, in_flight, awaiting_presentation) = sample();
    if pacing_work(continuous, pending, in_flight, awaiting_presentation) {
        gate.note_work(now);
        apply(false);
        false
    } else {
        true
    }
}
struct WindowFrameSink {
    shared: Arc<Shared>,
    dispatcher: Arc<Dispatcher>,
}

impl FrameSink for WindowFrameSink {
    fn submit(&self, request: FrameRequest) -> bool {
        if !self.shared.alive() {
            return false;
        }
        self.shared.publish_frame(request);
        if self.shared.pacing_paused() {
            self.dispatcher.wake();
        }
        true
    }

    fn is_alive(&self) -> bool {
        self.shared.alive()
    }
}

struct WindowRenderer {
    shared: Arc<Shared>,
    dispatcher: Arc<Dispatcher>,
    thread: Option<JoinHandle<()>>,
    layer: Retained<CAMetalLayer>,
    core: Arc<GpuCore>,
    display_link: Retained<CAMetalDisplayLink>,
    _delegate: Retained<DisplayLinkDelegate>,
    surface: (u32, u32),
    layer_size: (u32, u32),
    layer_scale: f64,
    background: Color,
    frame_rate: f32,
    pacing: PacingGate,
    stats: RenderStats,
    #[cfg(feature = "hud")]
    samples: Vec<GpuTimerSample>,
    last_published: Option<(FrameIndex, Instant)>,
}

impl WindowRenderer {
    fn new(
        mtm: MainThreadMarker,
        gpu: GpuRenderer,
        core: Arc<GpuCore>,
        layer: Retained<CAMetalLayer>,
        size: (u32, u32),
        max_frame_rate: f32,
        background: Color,
        shared: Arc<Shared>,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let delegate = DisplayLinkDelegate::new(mtm, Arc::clone(&shared));
        let display_link =
            CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
        display_link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let target = max_frame_rate.max(MIN_FRAME_RATE);
        display_link.setPreferredFrameRateRange(CAFrameRateRange::new(target, target, target));
        display_link.setPaused(true);
        unsafe {
            display_link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
        }

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
            dispatcher,
            thread: Some(thread),
            layer,
            core,
            display_link,
            _delegate: delegate,
            surface: size,
            layer_size: size,
            layer_scale: 0.0,
            background,
            frame_rate: target,
            pacing: PacingGate::new(),
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
        self.pacing.note_work(Instant::now());
        self.shared.set_pacing_paused(false);
        if self.display_link.isPaused() {
            self.display_link.setPaused(false);
        }
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

    fn set_frame_rate(&mut self, max_frame_rate: f32) {
        let target = max_frame_rate.max(MIN_FRAME_RATE);
        if (target - self.frame_rate).abs() <= f32::EPSILON {
            return;
        }
        self.frame_rate = target;
        self.display_link
            .setPreferredFrameRateRange(CAFrameRateRange::new(target, target, target));
    }

    fn set_background_color(&mut self, color: Color) {
        self.background = color;
        swapchain::set_layer_background(&self.layer, color);
        self.shared.set_background(background_linear(color));
    }

    fn sync_pacing(&mut self, continuous: bool) {
        let shared = Arc::clone(&self.shared);
        let now = Instant::now();
        let Self {
            pacing,
            display_link,
            ..
        } = self;
        let paused = reconcile_pacing(
            continuous,
            pacing,
            now,
            || shared.pacing_state(),
            |paused| {
                if display_link.isPaused() != paused {
                    display_link.setPaused(paused);
                }
                shared.set_pacing_paused(paused);
            },
        );
        debug_assert_eq!(
            paused,
            display_link.isPaused(),
            "sync_pacing 返回后 display link 与决策必须一致"
        );
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
            "presented_at must be a lower bound of the on-screen instant (never before the GPU \
             completed) or None"
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
                    || feedback.latency <= Instant::now().saturating_duration_since(published)),
            "frame latency points past the current instant"
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
        self.display_link.setDelegate(None);
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
    let _ = thread.join();
}

struct SendCommandBuffer(Retained<ProtocolObject<dyn MTLCommandBuffer>>);

unsafe impl Send for SendCommandBuffer {}

struct DrawableUpdate {
    drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>,
    target: CFTimeInterval,
    sequence: u64,
}

unsafe impl Send for DrawableUpdate {}

struct SendLayer {
    _layer: Retained<CAMetalLayer>,
}

unsafe impl Send for SendLayer {}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    atlas_pending: [AtomicU32; 2],
    pacing_paused: AtomicBool,
    in_flight: Arc<InFlight>,
}

struct State {
    frame: LatestFrame,
    published_surface: Option<(u32, u32)>,
    atlas: AtlasSlots,
    drawable: Option<DrawableUpdate>,
    completions: VecDeque<Completion>,
    tick_sequence: u64,
    awaiting_tick: Option<u64>,
    feedback: Option<FrameFeedback>,
    error: Option<EngineError>,
    shutdown: bool,
    shutdown_at: Option<Instant>,
    terminated: bool,
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
            tick_sequence: 0,
            awaiting_tick: None,
            feedback: None,
            error: None,
            shutdown: false,
            shutdown_at: None,
            terminated: false,
            background: None,
        }
    }

    fn has_work(&self, in_flight: usize) -> bool {
        if self.shutdown {
            return in_flight == 0 || !self.completions.is_empty();
        }
        !self.completions.is_empty()
            || self.presentation_evidence()
            || self.background.is_some()
            || (self.frame.has_pending() && self.drawable.is_some())
    }

    fn presentation_evidence(&self) -> bool {
        self.awaiting_tick
            .is_some_and(|awaiting| self.tick_sequence > awaiting)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    Work,
    TimedOut,
}

struct Completion {
    index: FrameIndex,
    sequence: u64,
    buffer: SendCommandBuffer,
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

fn surface_within_tolerance(requested: (u32, u32), drawable: (u32, u32)) -> bool {
    requested.0.abs_diff(drawable.0) <= 1 && requested.1.abs_diff(drawable.1) <= 1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceVerdict {
    Ready,
    Skip,
    Degraded,
}

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

    fn observe(&mut self, requested: Option<(u32, u32)>, drawable: (u32, u32)) -> SurfaceVerdict {
        let Some(requested) = requested else {
            return SurfaceVerdict::Ready;
        };
        if surface_within_tolerance(requested, drawable) {
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
            atlas_pending: [AtomicU32::new(0), AtomicU32::new(0)],
            pacing_paused: AtomicBool::new(true),
            in_flight,
        }
    }

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

    fn set_pacing_paused(&self, paused: bool) {
        self.pacing_paused.store(paused, Ordering::Release);
    }

    fn pacing_paused(&self) -> bool {
        self.pacing_paused.load(Ordering::Acquire)
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

    fn publish_drawable(
        &self,
        drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>,
        target: CFTimeInterval,
    ) {
        let replaced = {
            let mut state = self.lock();
            if state.shutdown || state.terminated {
                return;
            }
            state.tick_sequence += 1;
            let sequence = state.tick_sequence;
            state.drawable.replace(DrawableUpdate {
                drawable,
                target,
                sequence,
            })
        };
        drop(replaced);
        self.wake();
    }

    fn tick_sequence(&self) -> u64 {
        self.lock().tick_sequence
    }

    fn set_awaiting_tick(&self, awaiting: Option<u64>) {
        self.lock().awaiting_tick = awaiting;
    }

    fn push_completion(&self, index: FrameIndex, sequence: u64, buffer: SendCommandBuffer) {
        let mut state = self.lock();
        if state.terminated {
            return;
        }
        state.completions.push_back(Completion {
            index,
            sequence,
            buffer,
        });
        drop(state);
        self.wake();
    }

    fn wait_for_work(&self) -> WaitOutcome {
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

    fn published_surface(&self) -> Option<(u32, u32)> {
        self.lock().published_surface
    }

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

    fn take_atlas(&self) -> Vec<AtlasUpload> {
        self.lock().atlas.take()
    }

    fn atlas_uploaded(&self, kind: AtlasKind) {
        self.atlas_pending[kind.index() as usize].store(0, Ordering::Release);
    }

    fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.atlas_pending[kind.index() as usize].load(Ordering::Acquire) == 0
    }

    fn take_completions(&self) -> Vec<Completion> {
        let mut state = self.lock();
        let completions: Vec<Completion> = state.completions.drain(..).collect();
        self.in_flight.note_drained(completions.len());
        completions
    }

    fn in_flight(&self) -> usize {
        self.in_flight.get()
    }

    fn pacing_state(&self) -> (bool, usize, bool) {
        let state = self.lock();
        (
            state.frame.has_pending(),
            self.in_flight.get(),
            state.awaiting_tick.is_some(),
        )
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

fn with_frame_pool<T>(f: impl FnOnce() -> T) -> T {
    autoreleasepool(|_pool| f())
}

#[cfg(any(test, feature = "test-support"))]
struct InjectionGate {
    consumed: AtomicBool,
}

#[cfg(any(test, feature = "test-support"))]
impl InjectionGate {
    const fn new() -> Self {
        Self {
            consumed: AtomicBool::new(false),
        }
    }

    fn take(&self, armed: bool) -> bool {
        if !armed {
            return false;
        }
        !self.consumed.swap(true, Ordering::AcqRel)
    }
}

#[cfg(any(test, feature = "test-support"))]
fn take_injected_device_lost() -> Option<EngineError> {
    static GATE: InjectionGate = InjectionGate::new();
    static ARMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let armed = *ARMED
        .get_or_init(|| std::env::var("KYTRIX_METAL_INJECT_DEVICE_LOST").is_ok_and(|v| v == "1"));
    if !GATE.take(armed) {
        return None;
    }
    let reason =
        SmolStr::new_static("injected MTLCommandBufferError.DeviceRemoved (test-support hook)");
    let log_path = crate::render::macos::device::write_diagnostics(&reason, 0);
    eprintln!(
        "kytrix: test-support injected a DeviceLost on the kytrix-metal thread \
         (KYTRIX_METAL_INJECT_DEVICE_LOST=1)"
    );
    Some(EngineError::DeviceLost { reason, log_path })
}

fn gpu_loop(mut renderer: GpuRenderer, shared: Arc<Shared>, _keep_alive: SendLayer) {
    enter_gpu_thread();
    let guard = PanicGuard::new(Arc::clone(&shared));
    let mut feedback = FeedbackQueue::new();
    let mut surface = SurfaceMismatch::new();
    let mut frontier: Option<FrameIndex> = None;
    let mut atlas_awaiting = [false; 2];
    loop {
        if let Some(linear) = shared.take_background() {
            renderer.set_background_color(linear);
        }
        if shared.wait_for_work() == WaitOutcome::TimedOut {
            eprintln!(
                "kytrix: metal shutdown waited {} s for {} in-flight frame(s) without a completion callback; exiting the GPU thread",
                SHUTDOWN_GRACE.as_secs(),
                shared.in_flight()
            );
            break;
        }
        let flow = with_frame_pool(|| {
            frame_tick(
                &mut renderer,
                &shared,
                &mut feedback,
                &mut frontier,
                &mut surface,
                &mut atlas_awaiting,
            )
        });
        if flow.is_break() {
            break;
        }
    }
    shared.mark_terminated();
    guard.disarm();
}

fn frame_tick(
    renderer: &mut GpuRenderer,
    shared: &Arc<Shared>,
    feedback: &mut FeedbackQueue,
    frontier: &mut Option<FrameIndex>,
    surface: &mut SurfaceMismatch,
    atlas_awaiting: &mut [bool; 2],
) -> ControlFlow<()> {
    #[cfg(any(test, feature = "test-support"))]
    if shared.has_pending_frame()
        && let Some(error) = take_injected_device_lost()
    {
        shared.publish_error(error);
        return ControlFlow::Break(());
    }
    feedback.observe_tick(shared.tick_sequence());
    if let Err(error) = drain_completions(renderer, shared, feedback, frontier) {
        shared.publish_error(error);
        return ControlFlow::Break(());
    }
    if let Some(frontier) = *frontier {
        feedback.pop_ready(frontier, |frame| shared.publish_feedback(frame));
    }
    shared.set_awaiting_tick(feedback.awaiting_tick());
    if shared.shutdown_requested() {
        if shared.in_flight() == 0 {
            return ControlFlow::Break(());
        }
        return ControlFlow::Continue(());
    }
    if !shared.has_pending_frame() {
        return ControlFlow::Continue(());
    }
    let Some(update) = shared.take_drawable() else {
        return ControlFlow::Continue(());
    };
    let texture = update.drawable.texture();
    let target = (texture.width() as u32, texture.height() as u32);
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
        return ControlFlow::Continue(());
    }
    let degrade = verdict == SurfaceVerdict::Degraded;
    let Some(frame_slot) = renderer.try_acquire_upload_slot() else {
        return ControlFlow::Continue(());
    };
    let Some(request) = shared.take_frame_for_drawable(target, degrade) else {
        renderer.release_upload_slot();
        return ControlFlow::Continue(());
    };
    if let Some(linear) = shared.take_background() {
        renderer.set_background_color(linear);
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
    let resized = match renderer.prepare_target(target) {
        Ok(resized) => resized,
        Err(error) => {
            shared.publish_error(error);
            return ControlFlow::Break(());
        }
    };
    let mut frame = FramePlan::new(request.plan, resized || renderer.is_fresh() || degrade);
    if frame.is_idle() {
        renderer.release_upload_slot();
        return ControlFlow::Continue(());
    }
    let index = request.index;
    let presented_at = presentation_instant(update.target, Instant::now(), CACurrentMediaTime());
    let tick = update.sequence;
    let completed = {
        let shared = Arc::clone(shared);
        let sequence = frame_slot.signal_value;
        move |buffer| shared.push_completion(index, sequence, SendCommandBuffer(buffer))
    };
    match renderer.submit_frame(&request, &update.drawable, &mut frame, completed) {
        Ok((stats, samples)) => {
            for (index, awaiting) in atlas_awaiting.iter_mut().enumerate() {
                if std::mem::take(awaiting) {
                    shared.atlas_uploaded(if index == 0 {
                        AtlasKind::Glyph
                    } else {
                        AtlasKind::Image
                    });
                }
            }
            feedback.push(
                index,
                stats,
                samples,
                request.published_at,
                tick,
                presented_at,
            );
            shared.set_awaiting_tick(feedback.awaiting_tick());
        }
        Err(error) => {
            shared.publish_error(error);
            return ControlFlow::Break(());
        }
    }
    ControlFlow::Continue(())
}
fn drain_completions(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    feedback: &mut FeedbackQueue,
    frontier: &mut Option<FrameIndex>,
) -> Result<(), EngineError> {
    for completion in shared.take_completions() {
        let index = completion.index;
        let (sample, advanced) =
            renderer.on_frame_completed(index, completion.sequence, &completion.buffer.0)?;
        feedback.complete(index, sample);
        if advanced.is_some() {
            *frontier = advanced;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::Arc,
        time::{Duration, Instant},
    };

    use smol_str::SmolStr;

    use super::{
        AtlasSlots, InFlight, InjectionGate, JOIN_TIMEOUT, MAX_SURFACE_MISMATCHES, PanicGuard,
        SHUTDOWN_GRACE, SendCommandBuffer, Shared, StickyError, SurfaceMismatch, SurfaceVerdict,
        WaitOutcome, join_with_deadline, reconcile_pacing, surface_within_tolerance,
        with_frame_pool,
    };
    use crate::{
        error::EngineError,
        paint::atlas::AtlasKind,
        render::{
            DirtyRects,
            compositor::Plan,
            frame::{FrameSink, LatestFrame},
            present_evidence::PacingGate,
        },
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

    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
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
        let uploads = slots.take();
        assert_eq!(uploads.len(), 2, "字形 + 图片第 1 层");
        let glyph = &uploads[0];
        assert_eq!(glyph.kind, AtlasKind::Glyph);
        assert_eq!(glyph.layer, 0);
        assert_eq!(glyph.spec, spec(64, 1));
        assert_eq!(glyph.pixels, vec![1, 2, 3]);
        assert_eq!(glyph.rects, dirty_rects(4, 8));
        let image = &uploads[1];
        assert_eq!(image.kind, AtlasKind::Image);
        assert_eq!(image.layer, 1);
        assert_eq!(image.spec, spec(2048, 2));
        assert_eq!(image.pixels, vec![4, 5]);
        assert_eq!(image.rects, dirty_rects(0, 2));
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
        let shared = Shared::new(Arc::new(InFlight::new()));
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
        let shared = Shared::new(Arc::new(InFlight::new()));
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
        for _ in 0..10 {
            assert_eq!(
                surface.observe(Some((1400, 1040)), (1402, 1040)),
                SurfaceVerdict::Degraded
            );
        }
        assert_eq!(
            surface.observe(Some((1400, 1040)), (1401, 1039)),
            SurfaceVerdict::Ready
        );
        assert!(!surface.degraded);
        assert_eq!(surface.consecutive, 0);
        assert_eq!(
            surface.observe(Some((1400, 1040)), (1402, 1040)),
            SurfaceVerdict::Skip
        );
    }

    #[test]
    fn publishing_while_paused_wakes_the_main_thread() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.set_pacing_paused(true);
        let dispatcher = crate::platform::dispatcher::Dispatcher::new();
        let sink = super::WindowFrameSink {
            shared: Arc::clone(&shared),
            dispatcher: Arc::clone(&dispatcher),
        };
        assert!(sink.submit(request(1, (64, 64))));
        assert!(
            dispatcher.has_pending_wake(),
            "暂停的 display link 收到帧请求后必须唤醒主线程"
        );
    }

    #[test]
    fn publishing_while_running_does_not_wake_the_main_thread() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.set_pacing_paused(false);
        let dispatcher = crate::platform::dispatcher::Dispatcher::new();
        let sink = super::WindowFrameSink {
            shared: Arc::clone(&shared),
            dispatcher: Arc::clone(&dispatcher),
        };
        assert!(sink.submit(request(1, (64, 64))));
        assert!(!dispatcher.has_pending_wake());
    }

    #[test]
    fn a_publish_after_resume_does_not_wake_again() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.set_pacing_paused(true);
        let dispatcher = crate::platform::dispatcher::Dispatcher::new();
        let sink = super::WindowFrameSink {
            shared: Arc::clone(&shared),
            dispatcher: Arc::clone(&dispatcher),
        };

        assert!(sink.submit(request(1, (64, 64))));
        assert!(dispatcher.has_pending_wake());
        assert!(dispatcher.take_frame_wake(), "主线程取走暂停唤醒");

        shared.set_pacing_paused(false);
        assert!(sink.submit(request(2, (64, 64))));
        assert!(
            !dispatcher.has_pending_wake(),
            "恢复后的发布不得再产生主线程唤醒"
        );
    }

    #[test]
    fn sync_pacing_rechecks_for_a_frame_arriving_during_the_pause_transition() {
        let mut gate = PacingGate::new();
        let now = Instant::now();
        let mut samples = 0;
        let paused = reconcile_pacing(
            false,
            &mut gate,
            now,
            || {
                samples += 1;
                if samples == 1 {
                    (false, 0, false)
                } else {
                    (true, 0, false)
                }
            },
            |_| {},
        );
        assert!(!paused, "二次采样看到帧必须恢复，否则丢唤醒");
        assert_eq!(samples, 2, "决定暂停后必须二次采样");
        assert!(!gate.update(false, now), "恢复后必须刷新滞回窗口");
    }

    #[test]
    fn sync_pacing_stays_paused_when_the_second_sample_is_idle() {
        let mut gate = PacingGate::new();
        let mut samples = 0;
        let paused = reconcile_pacing(
            false,
            &mut gate,
            Instant::now(),
            || {
                samples += 1;
                (false, 0, false)
            },
            |_| {},
        );
        assert!(paused);
        assert_eq!(samples, 2);
    }

    #[test]
    fn sync_pacing_skips_the_second_sample_when_work_exists() {
        let mut gate = PacingGate::new();
        let mut samples = 0;
        let paused = reconcile_pacing(
            false,
            &mut gate,
            Instant::now(),
            || {
                samples += 1;
                (true, 0, false)
            },
            |_| {},
        );
        assert!(!paused);
        assert_eq!(samples, 1);
    }

    #[test]
    fn sync_pacing_applies_the_pause_before_the_second_sample() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut gate = PacingGate::new();
        let now = Instant::now();

        let sample_events = Rc::clone(&events);
        let apply_events = Rc::clone(&events);
        let mut samples = 0;
        let paused = reconcile_pacing(
            false,
            &mut gate,
            now,
            || {
                sample_events.borrow_mut().push("sample");
                samples += 1;
                if samples == 1 {
                    (false, 0, false)
                } else {
                    (true, 0, false)
                }
            },
            |paused| {
                apply_events.borrow_mut().push(if paused {
                    "apply(true)"
                } else {
                    "apply(false)"
                });
            },
        );

        assert!(!paused);
        assert_eq!(
            *events.borrow(),
            vec!["sample", "apply(true)", "sample", "apply(false)"],
            "apply 必须夹在两次采样之间；挪到之后会重新打开丢唤醒窗口"
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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_pending_background_color_wakes_the_gpu_loop() {
        let shared = Shared::new(Arc::new(InFlight::new()));
        assert!(!shared.lock().has_work(0), "空闲：没有工作");
        shared.set_background([0.25, 0.5, 0.75, 1.0]);
        assert_eq!(
            shared.wait_for_work(),
            WaitOutcome::Work,
            "待应用背景色必须让 GPU 线程从等待中返回"
        );
        assert_eq!(shared.take_background(), Some([0.25, 0.5, 0.75, 1.0]));
        assert_eq!(shared.take_background(), None, "取一次清空");
    }

    #[test]
    fn awaiting_presentation_never_wakes_the_gpu_thread_on_a_timer() {
        use std::sync::atomic::{AtomicU32, Ordering};

        const OLD_PRESENTED_WAIT: Duration = Duration::from_millis(32);

        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.set_awaiting_tick(Some(0));
        let woke = Arc::new(AtomicU32::new(0));
        let worker = Arc::clone(&shared);
        let counter = Arc::clone(&woke);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = worker.wait_for_work();
            counter.fetch_add(1, Ordering::Release);
            let _ = sender.send(outcome);
        });
        std::thread::sleep(OLD_PRESENTED_WAIT * 5);
        assert_eq!(
            woke.load(Ordering::Acquire),
            0,
            "等上屏证据期间不允许固定间隔空转（旧实现每 32 ms 醒一次）"
        );
        shared.lock().tick_sequence += 1;
        shared.wake();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(WaitOutcome::Work),
            "更新的 tick 到达必须唤醒 GPU 线程"
        );
        assert_eq!(woke.load(Ordering::Acquire), 1, "只醒这一次");
    }

    #[test]
    fn shutdown_wait_is_bounded_when_completions_never_arrive() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
        shared.in_flight.preset(1);
        shared.request_shutdown();
        let worker = Arc::clone(&shared);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(worker.wait_for_work());
        });
        let started = Instant::now();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(6)),
            Ok(WaitOutcome::TimedOut),
            "驱动永不回调完成时 GPU 线程自己的等待必须有界（detach 后它也要能退出）"
        );
        assert!(started.elapsed() >= SHUTDOWN_GRACE, "必须等满 grace 才放弃");
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

    #[test]
    fn a_hung_gpu_thread_is_detached_after_the_bounded_wait() {
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
        let _ = release.send(());
    }

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
        shared.push_completion(index(1), 1, buffer());
        let completions = shared.take_completions();
        assert_eq!(
            completions.len(),
            1,
            "shutdown 期间的完成通知必须入队（关闭握手 drain 在飞帧）"
        );
        assert_eq!(shared.in_flight(), 1, "drain 必须按完成数减少在飞帧");

        shared.mark_terminated();
        shared.push_completion(index(2), 2, buffer());
        assert!(
            shared.take_completions().is_empty(),
            "terminated 之后不再入队（消费者已退出，继续入队只会泄漏命令缓冲）"
        );
    }

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
                ColorResolver,
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

    #[test]
    fn a_panicking_gpu_thread_publishes_an_error() {
        let shared = Arc::new(Shared::new(Arc::new(InFlight::new())));
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
                assert_eq!(detail, "kytrix-metal thread panicked");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            !shared.shutdown_requested(),
            "a panic only terminates the thread, it does not fake a shutdown"
        );
    }

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

    #[test]
    fn the_frame_pool_drains_autoreleased_objects_at_the_end_of_every_tick() {
        use objc2::{
            msg_send,
            rc::{Retained, Weak},
            runtime::NSObject,
        };

        let object = NSObject::new();
        let weak = Weak::from_retained(&object);
        assert!(weak.load().is_some(), "precondition: the object is alive");
        with_frame_pool(move || {
            let raw = Retained::into_raw(object);
            let _: *mut NSObject = unsafe { msg_send![raw, autorelease] };
        });
        assert!(
            weak.load().is_none(),
            "pool 必须在 tick 结束时 drain（否则每帧的 Metal 临时对象全部泄漏）"
        );
    }

    #[test]
    fn the_device_lost_injection_fires_at_most_once_per_process() {
        let gate = InjectionGate::new();
        assert!(!gate.take(false), "未武装时不得注入");
        assert!(gate.take(true), "第一次武装必须注入");
        assert!(
            !gate.take(true),
            "同一进程只能注入一次（否则恢复限次会被烧穿）"
        );
    }

    #[test]
    fn a_break_tick_returns_only_after_the_frame_pool_has_drained() {
        use std::ops::ControlFlow;

        use objc2::{
            msg_send,
            rc::{Retained, Weak},
            runtime::NSObject,
        };

        let object = NSObject::new();
        let weak = Weak::from_retained(&object);
        let flow: ControlFlow<()> = with_frame_pool(move || {
            let raw = Retained::into_raw(object);
            let _: *mut NSObject = unsafe { msg_send![raw, autorelease] };
            ControlFlow::Break(())
        });
        assert!(flow.is_break(), "Break must survive the pool boundary");
        assert!(
            weak.load().is_none(),
            "pool 必须在返回 Break 之前 drain，否则线程退出收尾会跑在 pool 里"
        );
    }
}
