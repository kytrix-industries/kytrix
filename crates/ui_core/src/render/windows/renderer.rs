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

    #[cfg(feature = "test-support")]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Ok(Self {
            target: RenderTarget::Offscreen(GpuRenderer::new_warp_offscreen(size)?),
            error: None,
        })
    }

    pub(crate) fn new_for_hwnd(
        composition: Arc<Mutex<Composition>>,
        size: (u32, u32),
        background: Color,
    ) -> Result<Self, EngineError> {
        let hwnd = composition::lock(&composition).hwnd_raw();
        let gpu =
            GpuRenderer::new_for_composition(composition, size, background_linear(background))?;
        Ok(Self {
            target: RenderTarget::Window(WindowRenderer::new(gpu, size, hwnd)?),
            error: None,
        })
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.surface,
        }
    }

    pub(crate) fn presented_surface(&self) -> (u32, u32) {
        match &self.target {
            RenderTarget::Offscreen(gpu) => gpu.surface(),
            RenderTarget::Window(window) => window.presented_surface(),
        }
    }

    pub(crate) fn take_composition_pending(&self) -> bool {
        match &self.target {
            RenderTarget::Offscreen(_) => false,
            RenderTarget::Window(window) => window.take_composition_pending(),
        }
    }

    pub(crate) fn request_composition_rebuild(&self) -> bool {
        match &self.target {
            RenderTarget::Offscreen(_) => false,
            RenderTarget::Window(window) => window.shared.request_composition_rebuild(),
        }
    }

    pub(crate) fn diagnostics(&self) -> Diagnostics {
        crate::render::windows::diagnostics::snapshot()
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
        match &self.target {
            RenderTarget::Offscreen(_) => true,
            RenderTarget::Window(window) => window.atlas_participating(),
        }
    }

    #[cfg(test)]
    pub(crate) fn atlas_pending_layers(&self, kind: AtlasKind) -> Option<Vec<u16>> {
        match &self.target {
            RenderTarget::Offscreen(gpu) => Some(gpu.atlas_pending_layers(kind)),
            RenderTarget::Window(_) => None,
        }
    }

    pub(crate) fn set_background_color(&mut self, color: Color) {
        match &self.target {
            RenderTarget::Offscreen(_) => {}
            RenderTarget::Window(window) => {
                window.set_background_color(background_linear(color));
            }
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
        self.shared.reset_epoch.load(Ordering::Acquire)
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
    fn new(gpu: GpuRenderer, size: (u32, u32), hwnd: isize) -> Result<Self, EngineError> {
        let work = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW(work)", error))?;
        let completion = match unsafe { CreateEventW(None, false, false, None) } {
            Ok(event) => event,
            Err(error) => {
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

    fn presented_surface(&self) -> (u32, u32) {
        unpack_size(self.shared.presented.load(Ordering::Acquire))
    }

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
        join_with_message_pump(thread);
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
    work: HANDLE,
    completion: HANDLE,
    hwnd: isize,
    thread: u32,
    presented: AtomicU64,
    reset_epoch: AtomicU64,
    composition_pending: AtomicBool,
    composition_rebuild: AtomicBool,
    atlas_pending: [AtomicU32; 2],
    atlas_participating: AtomicBool,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
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
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wake(&self) {
        unsafe {
            let _ = SetEvent(self.work);
        }
    }

    fn alive(&self) -> bool {
        let state = self.lock();
        !state.shutdown && !state.terminated
    }

    fn set_atlas_participating(&self, participating: bool) {
        self.atlas_participating
            .store(participating, Ordering::Release);
    }

    fn atlas_participating(&self) -> bool {
        self.atlas_participating.load(Ordering::Acquire)
    }

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

    fn set_background(&self, rgba: [f32; 4]) {
        let mut state = self.lock();
        if state.shutdown || state.terminated {
            return;
        }
        state.background = Some(rgba);
        drop(state);
        self.wake();
    }

    fn take_background(&self) -> Option<[f32; 4]> {
        self.lock().background.take()
    }

    fn request_composition_rebuild(&self) -> bool {
        if !self.alive() {
            return false;
        }
        self.composition_rebuild.store(true, Ordering::Release);
        self.wake();
        true
    }

    fn take_composition_rebuild_request(&self) -> bool {
        self.composition_rebuild.swap(false, Ordering::AcqRel)
    }

    fn publish_presented(&self, surface: (u32, u32)) {
        let packed = pack_size(surface);
        if self.presented.swap(packed, Ordering::AcqRel) != packed {
            self.notify_ui();
        }
    }

    fn note_renderer_rebuilt(&self) {
        self.reset_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn notify_ui(&self) {
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
            let posted = unsafe {
                PostThreadMessageW(self.thread, WM_APP_COMPOSITION_WAKE, WPARAM(0), LPARAM(0))
            };
            if posted.is_ok() {
                return;
            }
        }
        self.composition_pending.store(true, Ordering::Release);
    }

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
        unsafe {
            let _ = SetEvent(self.completion);
        }
        self.wake();
    }
}

struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
    published_at: Instant,
    presented: Option<Instant>,
    signal_value: u64,
    slot: usize,
    surface: (u32, u32),
}

struct FrameStatisticsClock {
    frequency: i64,
    anchor: OnceLock<(i64, Instant)>,
    last_sync: Option<(u32, i64)>,
    period_qpc: Option<i64>,
    samples: u64,
    failures: u64,
    warned: bool,
}

impl FrameStatisticsClock {
    fn new() -> Self {
        let mut frequency = 0i64;
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

    fn anchor(&self) -> Option<(i64, Instant)> {
        if let Some(anchor) = self.anchor.get() {
            return Some(*anchor);
        }
        let mut counter = 0i64;
        unsafe { QueryPerformanceCounter(&mut counter) }.ok()?;
        let anchor = (counter, Instant::now());
        Some(*self.anchor.get_or_init(|| anchor))
    }

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

fn device_lost_for_boundary(renderer: Option<&GpuRenderer>) -> bool {
    renderer.is_some_and(|current| current.check_device_lost().is_some())
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FatalDisposition {
    Fatal,
    DeviceLost,
    CompositionInvalid,
}

const COMPOSITION_RECOVERY_WINDOW: Duration = Duration::from_secs(5);
const MAX_COMPOSITION_REBUILDS: usize = 3;

struct CompositionRecovery {
    rebuilds: VecDeque<Instant>,
}

impl CompositionRecovery {
    fn new() -> Self {
        Self {
            rebuilds: VecDeque::with_capacity(MAX_COMPOSITION_REBUILDS),
        }
    }

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

fn composition_rebuild_exhausted() -> EngineError {
    EngineError::Composition {
        detail: format_smolstr!(
            "DirectComposition device stayed invalid after {MAX_COMPOSITION_REBUILDS} rebuilds within {COMPOSITION_RECOVERY_WINDOW:?}"
        ),
    }
}

fn gpu_loop(renderer: GpuRenderer, shared: Arc<Shared>) {
    enter_gpu_thread();
    let guard = PanicGuard::new(Arc::clone(&shared));
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
                let device_lost = match &error {
                    EngineError::DeviceLost { .. } => true,
                    _ => device_lost_for_boundary(renderer.as_ref()),
                };
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
                        submitted.clear();
                        let current = renderer.take().expect("renderer is present until teardown");
                        match current.recover() {
                            Ok(recovered) => {
                                crate::render::windows::diagnostics::device_recovered();
                                renderer = Some(recovered);
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
                                shared.note_renderer_rebuilt();
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

struct DisplayRecovery {
    seen: u64,
    pending: Option<u64>,
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

fn recover_display_change(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    state: &mut DisplayRecovery,
) -> Result<bool, EngineError> {
    let epoch = display::epoch();
    if epoch == state.seen {
        return Ok(false);
    }
    if state.pending != Some(epoch) {
        state.pending = Some(epoch);
        state.attempts = 0;
    }
    match renderer.rebuild_target() {
        Ok(true) => {}
        Ok(false) => return Ok(false),
        Err(error) => {
            if let Some(lost) = renderer.check_device_lost() {
                return Err(lost);
            }
            state.attempts += 1;
            if state.attempts > DISPLAY_REBUILD_ATTEMPTS {
                return Err(error);
            }
            std::thread::sleep(DISPLAY_REBUILD_BACKOFF);
            return Ok(false);
        }
    }
    state.seen = epoch;
    state.pending = None;
    state.attempts = 0;
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
    let mut atlas_awaiting = [false; 2];
    let mut display_recovery = DisplayRecovery::new();
    let mut force_present = false;
    let mut statistics = FrameStatisticsClock::new();
    loop {
        drain_completed(renderer, shared, submitted, &mut statistics);
        if shared.shutdown_requested() {
            return Ok(());
        }
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
                    force_present = true;
                }
            }
        }
        if recover_display_change(renderer, shared, &mut display_recovery)? {
            force_present = true;
        }
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba);
            renderer.refresh_background()?;
        }
        if shared.publish_atlas_participation(renderer.window_occluded(), force_present) {
            crate::render::windows::diagnostics::occluded_skipped();
            wait_for_work_or_completion(shared, Some(OCCLUDED_POLL_INTERVAL))?;
            continue;
        }
        if !shared.has_pending_frame() {
            wait_for_work_or_completion(shared, None)?;
            continue;
        }
        force_present = false;
        if !wait_for_waitable(renderer, shared, submitted, &mut statistics)? {
            continue;
        }
        if shared.shutdown_requested() {
            return Ok(());
        }
        let Some(request) = shared.take_frame() else {
            debug_assert!(false, "waitable consumed without a pending frame");
            continue;
        };
        drain_completed(renderer, shared, submitted, &mut statistics);
        if let Some(rgba) = shared.take_background() {
            renderer.set_background_color(rgba);
            renderer.refresh_background()?;
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
        renderer.set_content_size(request.surface)?;
        renderer.set_scale(request.scale);
        let replace = shared.has_pending_frame();
        let mut draw_plan = request.plan;
        let mut attempts = 0u32;
        let mut full_records = 0u32;
        loop {
            let slot = renderer.acquire_slot();
            match renderer.wait_slot_fence(slot)? {
                WaitOutcome::Signaled => {}
                WaitOutcome::TimedOut => {
                    attempts += 1;
                    if attempts >= MAX_FRAMES_IN_FLIGHT as u32 {
                        break;
                    }
                    continue;
                }
            }
            drain_completed(renderer, shared, submitted, &mut statistics);
            let mut frame = FramePlan::new(draw_plan, renderer.is_fresh());
            if frame.is_idle() {
                debug_assert!(false, "idle frame consumed a waitable token");
                break;
            }
            match renderer.submit_frame(&request, &mut frame, slot, replace) {
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
                    renderer.set_completion_event(slot.signal_value, shared.completion)?;
                    break;
                }
                Err(FrameSubmitError::NeedsResize) => {
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
        let completed_at = Instant::now();
        let mut presented = frame.presented;
        if submitted.is_empty()
            && let Some(refreshed) = statistics.observe(renderer.frame_statistics())
        {
            presented = Some(presented.map_or(refreshed, |known| known.max(refreshed)));
        }
        let presented_at = clamp_presented_at(presented, completed_at);
        shared.publish_feedback(FrameFeedback {
            index: frame.index,
            stats: frame.stats,
            samples,
            completed_at,
            latency: completed_at.saturating_duration_since(frame.published_at),
            presented_at,
        });
        shared.publish_presented(frame.surface);
        observed = true;
    }
    if observed {
        renderer.release_completed();
    }
}

fn wait_for_work_or_completion(
    shared: &Shared,
    timeout: Option<Duration>,
) -> Result<(), EngineError> {
    let millis = timeout.map(|timeout| {
        u32::try_from(timeout.as_millis())
            .unwrap_or(u32::MAX)
            .max(1)
    });
    let wait = unsafe {
        WaitForMultipleObjects(
            &[shared.work, shared.completion],
            false,
            millis.unwrap_or(INFINITE),
        )
    };
    match wait {
        WAIT_OBJECT_0 => Ok(()),
        wait if wait.0 == WAIT_OBJECT_0.0 + 1 => Ok(()),
        WAIT_TIMEOUT => Ok(()),
        _ => Err(EngineError::AppInit {
            detail: format_smolstr!(
                "WaitForMultipleObjects(work, completion) failed: {wait:?} ({})",
                std::io::Error::last_os_error()
            ),
        }),
    }
}

fn wait_for_waitable(
    renderer: &mut GpuRenderer,
    shared: &Shared,
    submitted: &mut VecDeque<SubmittedFrame>,
    statistics: &mut FrameStatisticsClock,
) -> Result<bool, EngineError> {
    let Some(waitable) = renderer.waitable() else {
        return Ok(true);
    };
    if renderer.take_first_present_skip() {
        return Ok(true);
    }
    loop {
        match wait_for_pair(waitable, shared.completion, "waitable, completion")? {
            Some(0) => return Ok(true),
            Some(_) => {
                drain_completed(renderer, shared, submitted, statistics);
                if shared.shutdown_requested() || renderer.window_occluded() {
                    return Ok(false);
                }
            }
            None => {
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

fn wait_for_pair(low: HANDLE, high: HANDLE, label: &str) -> Result<Option<usize>, EngineError> {
    let wait = unsafe { WaitForMultipleObjects(&[low, high], false, PRESENT_TIMEOUT_MS) };
    match wait.0 {
        0 => Ok(Some(0)),
        1 => Ok(Some(1)),
        timeout if timeout == WAIT_TIMEOUT.0 => Ok(None),
        _ => Err(EngineError::AppInit {
            detail: format_smolstr!(
                "WaitForMultipleObjects({label}) failed: {wait:?} ({})",
                std::io::Error::last_os_error()
            ),
        }),
    }
}

const PRESENT_TIMEOUT_MS: u32 = 1_000;
const PRESENT_STALL_BACKOFF: Duration = Duration::from_millis(16);
const OCCLUDED_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DISPLAY_REBUILD_ATTEMPTS: u32 = 5;
const DISPLAY_REBUILD_BACKOFF: Duration = Duration::from_millis(200);
const RESIZE_ATTEMPTS: u32 = 2;
const FULL_RECORD_ATTEMPTS: u32 = 1;

const JOIN_TIMEOUT_MS: u32 = 250;
const JOIN_HANG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn join_with_message_pump(thread: JoinHandle<()>) {
    let handle = HANDLE(thread.as_raw_handle());
    let mut quit = None;
    let deadline = std::time::Instant::now() + JOIN_HANG_TIMEOUT;
    loop {
        let wait = unsafe {
            MsgWaitForMultipleObjects(Some(&[handle]), false, JOIN_TIMEOUT_MS, QS_ALLINPUT)
        };
        if wait == WAIT_OBJECT_0 {
            let _ = thread.join();
            break;
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            quit = pump_messages().or(quit);
            continue;
        }
        if wait.0 == WAIT_TIMEOUT.0 {
            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "kytrix: kytrix-d3d12 thread did not exit within {:?}; detaching it (GPU likely lost)",
                    JOIN_HANG_TIMEOUT
                );
                break;
            }
            quit = pump_messages().or(quit);
            continue;
        }
        eprintln!(
            "kytrix: MsgWaitForMultipleObjects while joining the kytrix-d3d12 thread failed: {:?} ({})",
            wait,
            std::io::Error::last_os_error()
        );
        break;
    }
    if let Some(code) = quit {
        unsafe { PostQuitMessage(code) };
    }
}

fn pump_messages() -> Option<i32> {
    let mut message = MSG::default();
    loop {
        let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
        if !has_message.as_bool() {
            return None;
        }
        if message.message == WM_QUIT {
            return Some(message.wParam.0 as i32);
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn pack_size(surface: (u32, u32)) -> u64 {
    (u64::from(surface.0) << 32) | u64::from(surface.1)
}

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
        math::dp,
        paint::atlas::AtlasKind,
        render::{
            DirtyRects,
            compositor::Plan,
            frame::LatestFrame,
            windows::{compositor::GpuRenderer, display},
        },
    };

    fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
        crate::render::AtlasArraySpec { page, layers }
    }

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

        let sticky = StickyError::capture(EngineError::Composition {
            detail: SmolStr::new_static("set_geometry: 0x8007000E"),
        });
        let text = sticky.to_error().to_string();
        assert!(text.contains("DirectComposition"), "{text}");
        assert!(text.contains("set_geometry"), "{text}");
    }

    fn test_shared() -> Arc<Shared> {
        let work =
            unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
                .expect("work event");
        let completion =
            unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
                .expect("completion event");
        Arc::new(Shared {
            state: Mutex::new(State::new()),
            work,
            completion,
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
        assert_eq!(
            fatal_disposition(&composition, true, true),
            FatalDisposition::DeviceLost
        );

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
        let error = super::composition_rebuild_exhausted();
        match error {
            EngineError::Composition { detail } => {
                assert!(detail.contains("3 rebuilds"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

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
        let shared = test_shared();
        shared.request_shutdown();
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
