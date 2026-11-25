use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use wayland_client::protocol::wl_surface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

#[cfg(feature = "test-support")]
use super::readback::{Readback, ReadbackImage};
use super::{
    atlas::{AtlasUploads, SharedAtlas},
    device::{
        DeferredHandles, DescriptorLease, FrameSync, GpuBuffer, SharedGpu, init_error,
        on_gpu_thread,
    },
    geometry::GeometryRegistry,
    memory::MemoryNeed,
    pipeline::{Pipeline, RENDER_FORMAT},
    swapchain::{Acquire, CreateFailure, Present, Surface, Swapchain},
    timer::GpuTimer,
};
use crate::{
    error::EngineError,
    math::{DevicePoint, DeviceRect, LogicalSize, dp},
    paint::{atlas::AtlasKind, instance::FrameOp},
    platform::linux::dpi,
    render::{
        RenderStats,
        compositor::{CopyPlan, DamageRects, FramePlan, ImageCopyPlan, Plan},
        frame::{FramePayload, FrameRequest, FrameSlot, InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::RootUniform,
    },
    window::FrameIndex,
};

const CLIP_ALIGN: u64 = 256;

const MAX_SUBOPTIMAL_REBUILDS: u32 = 4;

pub(crate) const MAX_RETRY_BACKOFF: Duration = Duration::from_millis(50);

const SURFACE_LOST_BACKOFF: [Duration; 3] = [
    Duration::from_millis(1),
    Duration::from_millis(4),
    Duration::from_millis(16),
];

const REBUILD_COOLDOWN_FRAMES: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Retry {
    after: Duration,
    report: bool,
}

#[derive(Debug, Default)]
struct OutOfDateRecovery {
    out_of_date: u32,
    surface_lost_failures: u32,
}

impl OutOfDateRecovery {
    const fn new() -> Self {
        Self {
            out_of_date: 0,
            surface_lost_failures: 0,
        }
    }

    fn on_out_of_date(&mut self) -> Retry {
        self.out_of_date += 1;
        Retry {
            after: out_of_date_backoff(self.out_of_date),
            report: out_of_date_diagnostic(self.out_of_date),
        }
    }

    fn on_progress(&mut self) -> Option<u32> {
        let recovered = self.out_of_date;
        self.out_of_date = 0;
        self.surface_lost_failures = 0;
        (recovered > 1).then_some(recovered)
    }

    fn on_surface_lost_failure(&mut self, reason: &'static str) -> Result<Duration, EngineError> {
        self.surface_lost_failures += 1;
        SURFACE_LOST_BACKOFF
            .get(self.surface_lost_failures as usize - 1)
            .copied()
            .ok_or_else(|| EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "compositor/connection is still alive but the swapchain surface stayed lost \
                     after {SURFACE_LOST_BACKOFF_LEN} retries ({reason})"
                ),
            })
    }
}

const SURFACE_LOST_BACKOFF_LEN: usize = SURFACE_LOST_BACKOFF.len();

fn out_of_date_backoff(consecutive: u32) -> Duration {
    debug_assert!(consecutive >= 1, "the first out-of-date retry waits 1 ms");
    let exponent = consecutive.saturating_sub(1).min(6);
    Duration::from_millis(1u64 << exponent).min(MAX_RETRY_BACKOFF)
}

fn out_of_date_diagnostic(consecutive: u32) -> bool {
    consecutive == 1 || consecutive == 16 || consecutive.is_multiple_of(64)
}

struct Retired<T> {
    entries: Vec<(u64, T)>,
}

impl<T> Retired<T> {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn retire(&mut self, tag: u64, value: T) {
        self.entries.push((tag, value));
    }

    fn drain_ready(&mut self, completed: u64, mut drop_fn: impl FnMut(T)) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].0 <= completed {
                let (_, value) = self.entries.remove(index);
                drop_fn(value);
            } else {
                index += 1;
            }
        }
    }

    fn take_all(&mut self) -> Vec<(u64, T)> {
        std::mem::take(&mut self.entries)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.entries.iter_mut().map(|(_, value)| value)
    }
}

const MAX_RETIRED_SWAPCHAINS: usize = MAX_FRAMES_IN_FLIGHT + 1;

pub(crate) enum Submitted {
    Presented {
        stats: RenderStats,
        samples: Vec<GpuTimerSample>,
        slot: usize,
        sequence: u64,
    },
    Retry {
        after: Option<Duration>,
    },
}

type InFlightSequence = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageSyncStep {
    Full,
    Rects(DamageRects),
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwapchainCopy {
    step: ImageSyncStep,
    hint: Option<DamageRects>,
}

pub(crate) struct GpuRenderer {
    swapchain: Option<Swapchain>,
    vk_surface: Option<Surface>,
    persist: Persist,
    retired_persists: Retired<Persist>,
    retired_swapchains: Retired<Swapchain>,
    current_presented_once: bool,
    descriptors: DescriptorLease,
    written_slots: [Option<(vk::Buffer, u64)>; MAX_FRAMES_IN_FLIGHT],
    atlas_uploads: AtlasUploads,
    upload: UploadArena,
    mirror: InstanceMirror,
    command_pool: vk::CommandPool,
    commands: [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT],
    timer: Option<GpuTimer>,
    sync: FrameSync,
    clear_color: [f32; 4],
    viewport: Option<WpViewport>,
    last_destination: Option<(i32, i32)>,
    wl_surface: Option<wl_surface::WlSurface>,
    last_buffer_scale: i32,
    surface: (u32, u32),
    scale: f32,
    geometry: GeometryRegistry,
    fresh: bool,
    last_sequence: u64,
    submitted: VecDeque<InFlightSequence>,
    completed_sequence: u64,
    image_copy_plan: ImageCopyPlan,
    present_hint: Option<DamageRects>,
    image_sync: ImageSyncStep,
    rebuild_swapchain: bool,
    out_of_date_recovery: OutOfDateRecovery,
    suboptimal_rebuilds: u32,
    last_rebuild_sequence: u64,
    rebuild_abandoned: bool,
    #[cfg(feature = "test-support")]
    snapshot_index: u32,
    #[cfg(feature = "test-support")]
    readback: Readback,
    shared: Arc<SharedGpu>,
}

impl GpuRenderer {
    pub(crate) fn new_for_surface(
        display: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        size: (u32, u32),
        background: [f32; 4],
        viewport: Option<WpViewport>,
        surface: wl_surface::WlSurface,
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let shared = SharedGpu::process_windowed(display, surface_ptr)?;
        let vk_surface = Surface::new(&shared, display, surface_ptr)?;
        let swapchain =
            Swapchain::new(&shared, &vk_surface, size).map_err(CreateFailure::into_error)?;
        Self::with_gpu(
            shared,
            Some(vk_surface),
            Some(swapchain),
            size,
            background,
            viewport,
            Some(surface),
        )
    }

    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let shared = SharedGpu::process_headless()?;
        Self::with_gpu(shared, None, None, size, [0.0; 4], None, None)
    }

    fn with_gpu(
        shared: Arc<SharedGpu>,
        vk_surface: Option<Surface>,
        swapchain: Option<Swapchain>,
        size: (u32, u32),
        clear_color: [f32; 4],
        viewport: Option<WpViewport>,
        wl_surface: Option<wl_surface::WlSurface>,
    ) -> Result<Self, EngineError> {
        let descriptors = shared.window_descriptors()?;
        let target = swapchain.as_ref().map_or(size, |swapchain| {
            let extent = swapchain.extent();
            (extent.width, extent.height)
        });
        let persist = Persist::new(&shared, target)?;
        let swapchain_images = swapchain.as_ref().map_or(1, Swapchain::image_count);
        let upload = UploadArena::new(&shared)?;
        let mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        let (command_pool, commands) = create_commands(&shared)?;
        let timer = GpuTimer::new(&shared)?;
        let sync = FrameSync::new(&shared)?;
        let renderer = Self {
            vk_surface,
            swapchain,
            persist,
            retired_persists: Retired::new(),
            retired_swapchains: Retired::new(),
            current_presented_once: false,
            descriptors,
            atlas_uploads: AtlasUploads::new(),
            upload,
            mirror,
            command_pool,
            commands,
            timer,
            sync,
            written_slots: [None; MAX_FRAMES_IN_FLIGHT],
            clear_color,
            viewport,
            last_destination: None,
            wl_surface,
            last_buffer_scale: 1,
            surface: size,
            scale: 1.0,
            geometry: GeometryRegistry::new(),
            fresh: true,
            last_sequence: 0,
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
            completed_sequence: 0,
            image_copy_plan: ImageCopyPlan::new(swapchain_images),
            present_hint: None,
            image_sync: ImageSyncStep::Full,
            rebuild_swapchain: false,
            out_of_date_recovery: OutOfDateRecovery::new(),
            suboptimal_rebuilds: 0,
            last_rebuild_sequence: 0,
            rebuild_abandoned: false,
            #[cfg(feature = "test-support")]
            snapshot_index: 0,
            #[cfg(feature = "test-support")]
            readback: Readback::new(),
            shared,
        };
        Ok(renderer)
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    pub(crate) fn set_clear_color(&mut self, color: [f32; 4]) {
        self.clear_color = color;
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        debug_assert!(
            self.vk_surface.is_none() || on_gpu_thread(),
            "resizing a window target may only run on the kytrix-vulkan thread"
        );
        let size = (size.0.max(1), size.1.max(1));
        if self.vk_surface.is_some() {
            if size == self.surface && self.swapchain.is_some() {
                return Ok(());
            }
            self.surface = size;
            return match self.try_replace_swapchain(size) {
                Ok(()) => Ok(()),
                Err(CreateFailure::Fatal(error)) => Err(error),
                Err(CreateFailure::OutOfDate | CreateFailure::SurfaceLost) => Ok(()),
            };
        }
        if size == self.surface {
            return Ok(());
        }
        let persist = Persist::new(&self.shared, size)?;
        drop(std::mem::replace(&mut self.persist, persist));
        self.surface = size;
        self.fresh = true;
        Ok(())
    }

    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
    ) -> Result<Submitted, EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "Vulkan 窗口帧只能在 kytrix-vulkan 线程提交"
        );
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        if self.swapchain.is_none()
            && let Some(after) = self.ensure_swapchain("rebuilding the missing swapchain")?
        {
            return Ok(Submitted::Retry { after: Some(after) });
        }
        debug_assert!(
            self.swapchain.is_some(),
            "window frames require a swapchain"
        );
        self.scale = request.scale;
        let slot = self.sync.slot();
        self.sync
            .wait_slot(slot)
            .map_err(|error| self.classify(error))?;
        let acquire = {
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.acquire(&self.shared, self.sync.image_available(slot.index))
        };
        let image_index = match acquire {
            Ok(Acquire::Image { index, suboptimal }) => {
                if suboptimal {
                    self.rebuild_swapchain = true;
                }
                self.note_progress();
                index
            }
            Ok(Acquire::NotReady) => return Ok(Submitted::Retry { after: None }),
            Ok(Acquire::OutOfDate) => {
                return self.retry_out_of_date("acquire reported OUT_OF_DATE");
            }
            Ok(Acquire::SurfaceLost) => {
                return self.recover_surface("acquire reported SURFACE_LOST");
            }
            Err(error) => return Err(self.classify(error)),
        };

        let mut timings = FrameTimings::new();
        let (stats, sequence) = self.record_and_submit(
            request.index,
            slot,
            &request.payload,
            frame,
            Some(image_index),
            &mut timings,
        )?;

        if let Some(viewport) = self.viewport.as_ref()
            && let Some((width, height)) =
                destination_update(self.last_destination, request.surface_local)
        {
            viewport.set_destination(width, height);
            self.last_destination = Some((width, height));
        }

        debug_assert!(
            request.buffer_scale >= 1,
            "buffer_scale must be >= 1, got {}",
            request.buffer_scale
        );
        debug_assert!(
            request.buffer_scale == 1
                || (request
                    .surface
                    .0
                    .is_multiple_of(request.buffer_scale as u32)
                    && request
                        .surface
                        .1
                        .is_multiple_of(request.buffer_scale as u32)),
            "buffer {:?} is not a multiple of buffer_scale {} (wl_surface.invalid_size)",
            request.surface,
            request.buffer_scale
        );
        if let Some(surface) = self.wl_surface.as_ref()
            && request.buffer_scale != self.last_buffer_scale
        {
            surface.set_buffer_scale(request.buffer_scale);
            self.last_buffer_scale = request.buffer_scale;
        }

        let written = frame_written(frame);
        let present_started = Instant::now();
        let presented = {
            let hint = self.present_hint;
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.present(&self.shared, image_index, hint)
        };
        let presented = match presented {
            Ok(presented) => presented,
            Err(error) => return Err(self.classify(error)),
        };
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        match presented {
            Present::Presented | Present::Suboptimal => {
                if matches!(presented, Present::Suboptimal) {
                    self.rebuild_swapchain = true;
                }
                self.note_progress();
                frame.presented();
                self.fresh = false;
                self.current_presented_once = true;
                match self.image_sync {
                    ImageSyncStep::Nothing => {
                        self.image_copy_plan
                            .commit_without_copy(image_index, written);
                    }
                    ImageSyncStep::Full | ImageSyncStep::Rects(_) => {
                        self.image_copy_plan.commit(image_index, written);
                    }
                }
            }
            Present::OutOfDate => {
                self.submitted.push_back(sequence);
                return self.retry_out_of_date("present reported OUT_OF_DATE");
            }
            Present::SurfaceLost => {
                self.submitted.push_back(sequence);
                return self.recover_surface("present reported SURFACE_LOST");
            }
        }
        self.submitted.push_back(sequence);
        let samples = timings.samples().to_vec();

        if self.rebuild_swapchain {
            self.rebuild_swapchain = false;
            self.request_rebuild("present reported SUBOPTIMAL")?;
        }
        Ok(Submitted::Presented {
            stats,
            samples,
            slot: slot.index,
            sequence,
        })
    }

    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        debug_assert!(
            self.swapchain.is_none(),
            "the snapshot path must be offscreen"
        );
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        let slot = self.sync.slot();
        self.sync.wait_slot(slot)?;
        let (stats, sequence) =
            self.record_and_submit(index, slot, payload, frame, None, timings)?;
        self.finish_sync(slot, sequence, timings)?;
        self.fresh = false;
        frame.presented();
        Ok(stats)
    }

    pub(crate) fn drain_completed(&mut self) -> Result<u64, EngineError> {
        let value = self.sync.completed_value()?;
        while let Some(sequence) = self.submitted.front().copied() {
            if sequence > value {
                break;
            }
            self.submitted.pop_front();
            self.completed_sequence = sequence;
        }
        if self.completed_sequence != 0 {
            self.sync.drain_retire(self.completed_sequence);
            self.retired_persists
                .drain_ready(self.completed_sequence, drop);
        }
        self.drain_retired_swapchains()?;
        Ok(value)
    }

    pub(crate) fn resolve_frame_timer(&self, slot: usize) -> Option<GpuTimerSample> {
        self.timer.as_ref().and_then(|timer| timer.resolve(slot))
    }

    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: Vec<u8>,
        rects: crate::render::atlas::DirtyRects,
    ) {
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "window atlas uploads may only be queued on the kytrix-vulkan thread"
        );
        let queued_page = self.shared.atlas_page(kind);
        self.atlas_uploads
            .queue_upload(kind, layer, spec, queued_page, pixels, rects);
    }

    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas_uploads.pending(kind)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        let command = self.commands[0];
        let image = self.persist.image;
        let size = self.persist.size;
        unsafe {
            self.shared
                .raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .map_err(|error| {
                    init_error("reset_command_buffer(readback)", format!("{error:?}"))
                })?;
            self.shared
                .raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| {
                    init_error("begin_command_buffer(readback)", format!("{error:?}"))
                })?;
        }
        self.readback.record(&self.shared, command, image, size)?;
        unsafe {
            self.shared
                .raw()
                .end_command_buffer(command)
                .map_err(|error| {
                    init_error("end_command_buffer(readback)", format!("{error:?}"))
                })?;
        }
        let fence = unsafe {
            self.shared
                .raw()
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .map_err(|error| init_error("create_fence(readback)", format!("{error:?}")))?;
        let outcome = (|| -> Result<(), EngineError> {
            let commands = [command];
            let submit = vk::SubmitInfo::default().command_buffers(&commands);
            self.shared
                .with_queue(|queue| unsafe {
                    self.shared.raw().queue_submit(queue, &[submit], fence)
                })
                .map_err(|error| {
                    self.shared.note_failure(error, 0).unwrap_or_else(|| {
                        init_error("queue_submit(readback)", format!("{error:?}"))
                    })
                })?;
            self.shared.wait_fence(fence, "wait_for_fences(readback)")
        })();
        unsafe { self.shared.raw().destroy_fence(fence, None) };
        outcome?;
        self.readback.fetch(size)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &crate::paint::instance::InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        self.snapshot_index += 1;
        let index = FrameIndex::from_raw(self.snapshot_index);
        let payload = FramePayload::full(buffer, Vec::new(), Vec::new());
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit_snapshot(index, &payload, &mut frame, &mut timings)?;
        self.readback()
    }

    fn record_and_submit(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        image_index: Option<u32>,
        timings: &mut FrameTimings,
    ) -> Result<(RenderStats, u64), EngineError> {
        timings.reset(index);
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        debug_assert!(
            slot.signal_value > self.last_sequence,
            "timeline signal values must be strictly increasing (R11-22)"
        );
        let shared = Arc::clone(&self.shared);
        let upload_started = Instant::now();
        let mut retired: Vec<GpuResourceHandle> = Vec::new();
        let upload = self
            .upload
            .write(&mut self.mirror, slot.index, payload, &mut retired);
        let (_instance_bytes, clip_offset, _clip_bytes) = match upload {
            Ok(values) => values,
            Err(error) => {
                for handle in retired {
                    self.sync.retire(slot.signal_value, handle);
                }
                return Err(error);
            }
        };
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(upload_started),
            gpu_ns: 0,
        });
        self.geometry.begin_frame();
        self.image_sync = ImageSyncStep::Full;
        let recorded = self.record_frame(
            &shared,
            slot,
            payload,
            frame,
            image_index,
            clip_offset,
            &mut retired,
        );
        for handle in retired {
            self.sync.retire(slot.signal_value, handle);
        }
        let (draw_calls, geometry_uploaded_bytes) =
            recorded.map_err(|error| self.classify(error))?;
        Ok((
            RenderStats {
                draw_calls,
                instances: payload.drawn_instances(),
                geometry_uploaded_bytes,
            },
            slot.signal_value,
        ))
    }

    fn record_frame(
        &mut self,
        shared: &SharedGpu,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        image_index: Option<u32>,
        clip_offset: u64,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(u32, usize), EngineError> {
        let full = frame.is_full();
        let rebuild = {
            let atlas = shared.atlas();
            self.atlas_uploads.reconcile_pages([
                atlas.spec(AtlasKind::Glyph),
                atlas.spec(AtlasKind::Image),
                atlas.spec(AtlasKind::ColorGlyph),
            ])?
        };
        if !rebuild.is_empty() {
            let mut atlas = shared.atlas_write();
            shared
                .rebuild_atlas_pages_locked(&mut atlas, &rebuild)
                .map_err(|error| self.classify(error))?;
            return self.record_locked(
                shared,
                &atlas,
                slot,
                payload,
                frame,
                image_index,
                clip_offset,
                retired,
                full,
            );
        }
        let atlas = shared.atlas();
        self.record_locked(
            shared,
            &atlas,
            slot,
            payload,
            frame,
            image_index,
            clip_offset,
            retired,
            full,
        )
    }

    fn record_locked(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        image_index: Option<u32>,
        clip_offset: u64,
        retired: &mut Vec<GpuResourceHandle>,
        full: bool,
    ) -> Result<(u32, usize), EngineError> {
        let upload_buffer = self.upload.buffer(slot.index);
        let previous = self.written_slots[slot.index];
        if previous != Some((upload_buffer, clip_offset)) {
            shared.write_slot(
                self.descriptors.sets(),
                slot.index,
                upload_buffer,
                clip_offset,
            );
            self.written_slots[slot.index] = Some((upload_buffer, clip_offset));
        }
        let command = self.commands[slot.index];
        unsafe {
            shared
                .raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .map_err(|error| init_error("reset_command_buffer", format!("{error:?}")))?;
            shared
                .raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| init_error("begin_command_buffer", format!("{error:?}")))?;
        }
        if let Some(timer) = self.timer.as_ref() {
            timer.begin(command, slot.index);
        }
        let atlas_retired = self
            .atlas_uploads
            .record_uploads(shared, atlas, slot.index, command)?;
        retired.extend(atlas_retired);
        let geometry_uploaded_bytes =
            self.geometry
                .prepare(shared, command, payload.geometry(), retired)?;
        let (draw_calls, geometry_retired) =
            self.draw(shared, command, slot.index, payload, frame, full)?;
        retired.extend(geometry_retired);
        if let Some(image_index) = image_index {
            self.copy_to_swapchain(shared, command, image_index, frame_written(frame))?;
        }
        if let Some(timer) = self.timer.as_ref() {
            timer.end(command, slot.index);
        }
        unsafe {
            shared
                .raw()
                .end_command_buffer(command)
                .map_err(|error| init_error("end_command_buffer", format!("{error:?}")))?;
        }
        self.submit_commands(shared, slot, command, slot.signal_value, image_index)?;
        self.sync.advance(slot);
        self.last_sequence = slot.signal_value;
        Ok((draw_calls, geometry_uploaded_bytes))
    }
    fn finish_sync(
        &mut self,
        slot: FrameSlot,
        sequence: u64,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        self.sync.wait_slot(slot)?;
        if let Some(sample) = self
            .timer
            .as_ref()
            .and_then(|timer| timer.resolve(slot.index))
        {
            timings.push(sample);
        }
        let present_started = Instant::now();
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        self.completed_sequence = sequence;
        self.sync.drain_retire(sequence);
        self.retired_persists.drain_ready(sequence, drop);
        Ok(())
    }

    fn recreate_swapchain(&mut self) -> Result<(), EngineError> {
        let size = self.surface;
        match self.try_replace_swapchain(size) {
            Ok(()) => Ok(()),
            Err(CreateFailure::Fatal(error)) => Err(error),
            Err(CreateFailure::OutOfDate | CreateFailure::SurfaceLost) => Ok(()),
        }
    }

    fn try_replace_swapchain(&mut self, size: (u32, u32)) -> Result<(), CreateFailure> {
        let old = self.swapchain.take();
        let Some(surface) = self.vk_surface.as_ref() else {
            if let Some(old) = old {
                self.retire_swapchain(old);
            }
            return Err(CreateFailure::Fatal(EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static(
                    "swapchain replacement requires a window surface",
                ),
            }));
        };
        let created = match old.as_ref() {
            Some(old) => Swapchain::replace(&self.shared, surface, size, old.raw()),
            None => Swapchain::new(&self.shared, surface, size),
        };
        match created {
            Ok(swapchain) => self
                .install_swapchain(swapchain, old, size)
                .map_err(CreateFailure::Fatal),
            Err(failure) => {
                if let Some(old) = old {
                    self.retire_swapchain(old);
                }
                Err(failure)
            }
        }
    }

    fn ensure_swapchain(&mut self, reason: &'static str) -> Result<Option<Duration>, EngineError> {
        if self.swapchain.is_some() {
            return Ok(None);
        }
        match self.try_replace_swapchain(self.surface) {
            Ok(()) => Ok(None),
            Err(CreateFailure::Fatal(error)) => Err(error),
            Err(CreateFailure::OutOfDate) => Ok(Some(self.wait_for_out_of_date(reason))),
            Err(CreateFailure::SurfaceLost) => {
                self.recreate_vk_surface()?;
                match self.try_replace_swapchain(self.surface) {
                    Ok(()) => Ok(None),
                    Err(CreateFailure::Fatal(error)) => Err(error),
                    Err(CreateFailure::OutOfDate) => Ok(Some(self.wait_for_out_of_date(reason))),
                    Err(CreateFailure::SurfaceLost) => {
                        Ok(Some(self.wait_for_surface_lost_retry(reason)?))
                    }
                }
            }
        }
    }

    fn retry_out_of_date(&mut self, reason: &'static str) -> Result<Submitted, EngineError> {
        self.rebuild_swapchain = false;
        let after = self.wait_for_out_of_date(reason);
        if self.swapchain.is_some() {
            match self.try_replace_swapchain(self.surface) {
                Ok(()) | Err(CreateFailure::OutOfDate | CreateFailure::SurfaceLost) => {}
                Err(CreateFailure::Fatal(error)) => return Err(error),
            }
        }
        Ok(Submitted::Retry { after: Some(after) })
    }

    fn wait_for_out_of_date(&mut self, reason: &'static str) -> Duration {
        let retry = self.out_of_date_recovery.on_out_of_date();
        if retry.report {
            eprintln!(
                "kytrix: vulkan swapchain stayed OUT_OF_DATE ({} consecutive rebuilds, {reason}); \
                 retrying in {} ms (extent={:?})",
                self.out_of_date_recovery.out_of_date,
                retry.after.as_millis(),
                self.swapchain.as_ref().map(Swapchain::extent),
            );
        }
        retry.after
    }

    fn wait_for_surface_lost_retry(
        &mut self,
        reason: &'static str,
    ) -> Result<Duration, EngineError> {
        let attempts = self.out_of_date_recovery.surface_lost_failures + 1;
        let after = self.out_of_date_recovery.on_surface_lost_failure(reason)?;
        eprintln!(
            "kytrix: vulkan compositor/connection is alive but the swapchain surface is still lost \
             (recovery {attempts}/{SURFACE_LOST_BACKOFF_LEN}); retrying in {} ms",
            after.as_millis()
        );
        Ok(after)
    }

    fn note_progress(&mut self) {
        if let Some(recovered) = self.out_of_date_recovery.on_progress() {
            eprintln!(
                "kytrix: vulkan swapchain recovered after {recovered} consecutive OUT_OF_DATE rebuilds"
            );
        }
    }

    fn recover_surface(&mut self, reason: &'static str) -> Result<Submitted, EngineError> {
        if self.vk_surface.is_none() {
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("{reason}: no window surface"),
            });
        }
        eprintln!("kytrix: vulkan {reason}: recreating VkSurfaceKHR");
        if let Some(old) = self.swapchain.take() {
            self.retire_swapchain(old);
        }
        self.recreate_vk_surface()?;
        match self.try_replace_swapchain(self.surface) {
            Ok(()) => Ok(Submitted::Retry { after: None }),
            Err(CreateFailure::Fatal(error)) => Err(error),
            Err(CreateFailure::OutOfDate) => Ok(Submitted::Retry {
                after: Some(self.wait_for_out_of_date(reason)),
            }),
            Err(CreateFailure::SurfaceLost) => Ok(Submitted::Retry {
                after: Some(self.wait_for_surface_lost_retry(reason)?),
            }),
        }
    }

    fn install_swapchain(
        &mut self,
        swapchain: Swapchain,
        old: Option<Swapchain>,
        size: (u32, u32),
    ) -> Result<(), EngineError> {
        if let Some(old) = old {
            self.retire_swapchain(old);
        }
        self.image_copy_plan.reset(swapchain.image_count());
        let extent = swapchain.extent();
        self.swapchain = Some(swapchain);
        self.fresh = true;
        self.surface = size;
        self.current_presented_once = false;
        self.present_hint = None;
        if self.persist.size != (extent.width, extent.height) {
            let persist = Persist::new(&self.shared, (extent.width, extent.height))?;
            let previous = std::mem::replace(&mut self.persist, persist);
            self.retired_persists.retire(self.last_sequence, previous);
        }
        Ok(())
    }

    fn retire_swapchain(&mut self, old: Swapchain) {
        self.retired_swapchains.retire(self.last_sequence, old);
    }

    fn request_rebuild(&mut self, reason: &'static str) -> Result<(), EngineError> {
        if self.rebuild_abandoned {
            return Ok(());
        }
        let since_last = self
            .last_sequence
            .saturating_sub(self.last_rebuild_sequence);
        if self.suboptimal_rebuilds > 0 && since_last < REBUILD_COOLDOWN_FRAMES {
            return Ok(());
        }
        if since_last >= REBUILD_COOLDOWN_FRAMES {
            self.suboptimal_rebuilds = 0;
        }
        self.suboptimal_rebuilds += 1;
        if self.suboptimal_rebuilds > MAX_SUBOPTIMAL_REBUILDS {
            self.rebuild_abandoned = true;
            eprintln!(
                "kytrix: vulkan swapchain keeps reporting {reason} after \
                 {MAX_SUBOPTIMAL_REBUILDS} rebuilds; keeping the current swapchain \
                 (extent={:?} format={:?} color_space={:?})",
                self.swapchain.as_ref().map(Swapchain::extent),
                self.swapchain
                    .as_ref()
                    .map(|swapchain| swapchain.key().format),
                self.swapchain
                    .as_ref()
                    .map(|swapchain| swapchain.key().color_space),
            );
            return Ok(());
        }
        self.last_rebuild_sequence = self.last_sequence;
        self.recreate_swapchain()
    }

    fn recreate_vk_surface(&mut self) -> Result<(), EngineError> {
        debug_assert!(
            self.swapchain.is_none(),
            "surface recreation requires the current swapchain to be taken out first"
        );
        self.destroy_retired_swapchains_now()?;
        let Some(surface) = self.vk_surface.as_mut() else {
            return Err(EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static(
                    "surface recreation requires a window surface",
                ),
            });
        };
        surface.recreate()
    }

    fn destroy_retired_swapchains_now(&mut self) -> Result<(), EngineError> {
        if self.retired_swapchains.is_empty() {
            return Ok(());
        }
        self.sync
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        self.shared
            .wait_queue_idle()
            .map_err(|error| self.classify(error))?;
        let retired = self.retired_swapchains.take_all();
        debug_assert!(self.retired_swapchains.is_empty());
        drop(retired);
        Ok(())
    }

    fn drain_retired_swapchains(&mut self) -> Result<(), EngineError> {
        if self.retired_swapchains.is_empty() {
            return Ok(());
        }
        if self.retired_swapchains.len() > MAX_RETIRED_SWAPCHAINS {
            return self.destroy_retired_swapchains_now();
        }
        if !self.current_presented_once {
            return Ok(());
        }
        let completed = self.completed_sequence;
        self.retired_swapchains.drain_ready(completed, drop);
        Ok(())
    }

    fn extent(&self) -> vk::Extent2D {
        match self.swapchain.as_ref() {
            Some(swapchain) => swapchain.extent(),
            None => vk::Extent2D {
                width: self.surface.0,
                height: self.surface.1,
            },
        }
    }

    fn draw(
        &mut self,
        shared: &SharedGpu,
        command: vk::CommandBuffer,
        slot: usize,
        payload: &FramePayload,
        frame: &FramePlan,
        full: bool,
    ) -> Result<(u32, Vec<GpuResourceHandle>), EngineError> {
        let mut draw_calls = 0u32;
        let extent = self.extent();
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: self.clear_color,
            },
        };
        let clears = [clear];
        let framebuffer = self.persist.framebuffer(full);
        let root = root_uniform(extent, self.scale);
        let pipeline = shared.pipeline();
        let sets = self.descriptors.sets();
        unsafe {
            shared.raw().cmd_begin_render_pass(
                command,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(pipeline.render_pass(full))
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    })
                    .clear_values(&clears),
                vk::SubpassContents::INLINE,
            );
            shared.raw().cmd_bind_pipeline(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.quad(),
            );
            shared.raw().cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout(),
                0,
                &[sets[slot]],
                &[],
            );
            let push_bytes = bytemuck::cast_slice::<f32, u8>(root.words());
            shared.raw().cmd_push_constants(
                command,
                pipeline.layout(),
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push_bytes,
            );
            shared.raw().cmd_set_viewport(
                command,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
        }

        let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
        match full {
            true => {
                unsafe {
                    shared.raw().cmd_set_scissor(
                        command,
                        0,
                        &[vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent,
                        }],
                    );
                }
                draw_calls = draw_ops(
                    shared,
                    command,
                    pipeline,
                    &mut self.geometry,
                    payload,
                    root,
                    [0, 0, extent.width as i32, extent.height as i32],
                )?;
            }
            false => {
                if let Plan::Partial(damage) = frame.plan() {
                    let clamped = damage.clamped(extent.width, extent.height);
                    for rect in clamped.as_slice() {
                        let scissor = vk::Rect2D {
                            offset: vk::Offset2D {
                                x: rect.x0.value(),
                                y: rect.y0.value(),
                            },
                            extent: vk::Extent2D {
                                width: rect.width().value() as u32,
                                height: rect.height().value() as u32,
                            },
                        };
                        unsafe {
                            shared.raw().cmd_set_scissor(command, 0, &[scissor]);
                        }
                        draw_calls = draw_calls.saturating_add(draw_ops(
                            shared,
                            command,
                            pipeline,
                            &mut self.geometry,
                            payload,
                            root,
                            [
                                rect.x0.value(),
                                rect.y0.value(),
                                rect.x1.value(),
                                rect.y1.value(),
                            ],
                        )?);
                    }
                }
            }
        }

        unsafe {
            shared.raw().cmd_end_render_pass(command);
        }
        self.geometry.retain_recent(&mut geometry_retired);
        Ok((draw_calls, geometry_retired))
    }

    fn copy_to_swapchain(
        &mut self,
        shared: &SharedGpu,
        command: vk::CommandBuffer,
        image_index: u32,
        written: Option<DamageRects>,
    ) -> Result<(), EngineError> {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return Ok(());
        };
        let extent = swapchain.extent();
        let target = swapchain.image(image_index);
        let copy = plan_swapchain_copy(
            self.image_copy_plan.plan(image_index, written),
            extent,
            self.present_hint,
        );
        self.image_sync = copy.step;
        self.present_hint = copy.hint;
        let copies = match copy.step {
            ImageSyncStep::Nothing => return Ok(()),
            ImageSyncStep::Full => vec![image_copy(0, 0, extent.width, extent.height)],
            ImageSyncStep::Rects(regions) => regions
                .as_slice()
                .iter()
                .map(|rect| {
                    image_copy(
                        rect.x0.value() as u32,
                        rect.y0.value() as u32,
                        rect.width().value() as u32,
                        rect.height().value() as u32,
                    )
                })
                .collect(),
        };
        debug_assert!(!copies.is_empty(), "a copy step must move pixels");
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let discards_content = matches!(copy.step, ImageSyncStep::Full);
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(if discards_content {
                vk::ImageLayout::UNDEFINED
            } else {
                vk::ImageLayout::PRESENT_SRC_KHR
            })
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(target)
            .subresource_range(range)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let to_present = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .image(target)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let to_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(self.persist.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        let back = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(self.persist.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
        unsafe {
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            shared.raw().cmd_copy_image(
                command,
                self.persist.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copies,
            );
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
        }
        Ok(())
    }

    fn submit_commands(
        &mut self,
        shared: &SharedGpu,
        slot: FrameSlot,
        command: vk::CommandBuffer,
        sequence: u64,
        image_index: Option<u32>,
    ) -> Result<(), EngineError> {
        let commands = [command];
        let waits: Vec<vk::Semaphore> = image_index
            .map(|_| self.sync.image_available(slot.index))
            .into_iter()
            .collect();
        let stages = [vk::PipelineStageFlags::TRANSFER];
        let mut signals = Vec::with_capacity(2);
        if let Some(image_index) = image_index {
            let swapchain = self.swapchain.as_ref().expect("image implies swapchain");
            signals.push(swapchain.render_finished(image_index));
        }
        signals.push(self.sync.timeline());
        let wait_values = vec![0u64; waits.len()];
        let signal_values: Vec<u64> = {
            let mut values = vec![0u64; signals.len()];
            if let Some(last) = values.last_mut() {
                *last = sequence;
            }
            values
        };
        let mut timeline_info = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wait_values)
            .signal_semaphore_values(&signal_values);
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&waits)
            .wait_dst_stage_mask(&stages[..waits.len().min(stages.len())])
            .command_buffers(&commands)
            .signal_semaphores(&signals)
            .push_next(&mut timeline_info);
        self.sync.reset_fence(slot.index)?;
        let result = shared.with_queue(|queue| unsafe {
            shared
                .raw()
                .queue_submit(queue, &[submit], self.sync.fence(slot.index))
        });
        if let Err(error) = result {
            if let Some(lost) = shared.note_failure(error, sequence) {
                return Err(lost);
            }
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("queue_submit: {error:?}"),
            });
        }
        Ok(())
    }

    fn classify(&mut self, error: EngineError) -> EngineError {
        if matches!(error, EngineError::DeviceLost { .. })
            && let Some(sticky) = self
                .shared
                .note_failure(vk::Result::ERROR_DEVICE_LOST, self.last_sequence)
        {
            return sticky;
        }
        error
    }

    fn leak_gpu_handles(&mut self) {
        if let Some(swapchain) = self.swapchain.as_mut() {
            swapchain.leak();
        }
        if let Some(surface) = self.vk_surface.as_mut() {
            surface.leak();
        }
        self.persist.leak();
        for persist in self.retired_persists.iter_mut() {
            persist.leak();
        }
        for swapchain in self.retired_swapchains.iter_mut() {
            swapchain.leak();
        }
        self.sync.leak();
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        let timeline_idle = self.sync.wait_idle();
        let queue_idle = self.shared.wait_queue_idle();
        let teardown_safe = timeline_idle.is_ok() && queue_idle.is_ok();
        if !teardown_safe {
            let reason = timeline_idle
                .err()
                .or(queue_idle.err())
                .map(|error| error.to_string())
                .unwrap_or_default();
            eprintln!(
                "kytrix: vulkan teardown could not prove the GPU is idle ({reason}); \
                 leaking this window's deferred objects instead of destroying in-flight handles"
            );
            self.leak_gpu_handles();
            self.retired_persists.take_all();
            self.retired_swapchains.take_all();
            return;
        }
        unsafe {
            self.shared
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
        self.retired_persists.take_all();
        self.retired_swapchains.take_all();
        self.geometry.release(&self.shared);
        self.upload.release(&self.shared);
        self.sync.drain_all_retired();
    }
}

struct Persist {
    device: ash::Device,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    framebuffer_clear: vk::Framebuffer,
    framebuffer_load: vk::Framebuffer,
    size: (u32, u32),
    handles: DeferredHandles,
}

impl Persist {
    fn new(shared: &SharedGpu, size: (u32, u32)) -> Result<Self, EngineError> {
        let device = shared.raw();
        let pipeline = shared.pipeline();
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(RENDER_FORMAT)
                    .extent(vk::Extent3D {
                        width: size.0,
                        height: size.1,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | vk::ImageUsageFlags::TRANSFER_SRC
                            | vk::ImageUsageFlags::SAMPLED,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(|error| init_error("create_image(persist)", format!("{error:?}")))?;
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let memory_type = shared
            .memory_type(requirements.memory_type_bits, MemoryNeed::DEVICE_IMAGE)
            .ok_or_else(|| init_error("persist memory", "no device-local memory type"))?;
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(persist)", format!("{error:?}")))?;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|error| init_error("bind_image_memory(persist)", format!("{error:?}")))?;
        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(RENDER_FORMAT)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .map_err(|error| init_error("create_image_view(persist)", format!("{error:?}")))?;
        let attachments = [view];
        let framebuffer_clear =
            create_framebuffer(device, pipeline.render_pass(true), &attachments, size)?;
        let framebuffer_load =
            create_framebuffer(device, pipeline.render_pass(false), &attachments, size)?;
        Ok(Self {
            device: device.clone(),
            image,
            view,
            memory,
            framebuffer_clear,
            framebuffer_load,
            size,
            handles: DeferredHandles::default(),
        })
    }

    fn leak(&mut self) {
        self.handles.leak();
    }

    fn framebuffer(&self, clear: bool) -> vk::Framebuffer {
        if clear {
            self.framebuffer_clear
        } else {
            self.framebuffer_load
        }
    }
}

impl Drop for Persist {
    fn drop(&mut self) {
        if self.handles.leaked() {
            return;
        }
        unsafe {
            self.device
                .destroy_framebuffer(self.framebuffer_clear, None);
            self.device.destroy_framebuffer(self.framebuffer_load, None);
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn create_framebuffer(
    device: &ash::Device,
    pass: vk::RenderPass,
    attachments: &[vk::ImageView],
    size: (u32, u32),
) -> Result<vk::Framebuffer, EngineError> {
    unsafe {
        device.create_framebuffer(
            &vk::FramebufferCreateInfo::default()
                .render_pass(pass)
                .attachments(attachments)
                .width(size.0)
                .height(size.1)
                .layers(1),
            None,
        )
    }
    .map_err(|error| init_error("create_framebuffer", format!("{error:?}")))
}

struct UploadArena {
    shared: Arc<SharedGpu>,
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    buffer: GpuBuffer,
    instance_capacity: u32,
    clip_capacity: u32,
    clip_offset: u64,
}

impl UploadArena {
    fn new(shared: &Arc<SharedGpu>) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            slots.push(UploadSlot::new(shared, 256, 8)?);
        }
        Ok(Self {
            shared: Arc::clone(shared),
            slots,
        })
    }

    fn buffer(&self, slot: usize) -> vk::Buffer {
        self.slots[slot].buffer.buffer
    }

    fn write(
        &mut self,
        mirror: &mut InstanceMirror,
        slot: usize,
        payload: &FramePayload,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(u32, u64, u64), EngineError> {
        let instance_count = payload.instance_count();
        let clips = payload.clips();
        let clip_count = clips.len() as u32;
        if instance_count > self.slots[slot].instance_capacity
            || clip_count > self.slots[slot].clip_capacity
        {
            let instances_capacity = instance_count.next_power_of_two().max(256);
            let clips_capacity = clip_count.next_power_of_two().max(64);
            let replacement = UploadSlot::new(&self.shared, instances_capacity, clips_capacity)?;
            let old = std::mem::replace(&mut self.slots[slot], replacement);
            retired.push(old.buffer.handle);
            mirror.invalidate(slot);
        }
        let slot_ref = &self.slots[slot];
        let Some(base) = slot_ref.buffer.pointer else {
            return Err(init_error(
                "upload memory",
                "suballocated upload buffer has no host mapping",
            ));
        };
        let full = mirror.prepare(slot, payload);
        let instance_bytes = instance_count as usize * 128;
        let clip_offset = slot_ref.clip_offset;
        unsafe {
            if full {
                let bytes = &mirror.bytes()[..instance_bytes];
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len());
            } else {
                for range in mirror.writes() {
                    let start = range.start as usize * 128;
                    let end = range.end as usize * 128;
                    std::ptr::copy_nonoverlapping(
                        mirror.bytes()[start..end].as_ptr(),
                        base.add(start),
                        end - start,
                    );
                }
            }
            if !clips.is_empty() {
                std::ptr::copy_nonoverlapping(
                    bytemuck::cast_slice::<crate::paint::instance::GpuClipEntry, u8>(clips)
                        .as_ptr(),
                    base.add(clip_offset as usize),
                    clips.len() * 48,
                );
            }
        }
        mirror.finish(slot);
        Ok((instance_count, clip_offset, clips.len() as u64))
    }

    fn release(&self, shared: &SharedGpu) {
        for slot in &self.slots {
            let _ = shared.release(slot.buffer.handle);
        }
    }
}

impl UploadSlot {
    fn new(
        shared: &SharedGpu,
        instance_capacity: u32,
        clip_capacity: u32,
    ) -> Result<Self, EngineError> {
        let instance_bytes = u64::from(instance_capacity) * 128;
        let clip_offset = instance_bytes.div_ceil(CLIP_ALIGN) * CLIP_ALIGN;
        let size = clip_offset + u64::from(clip_capacity) * 48;
        let buffer = shared.allocate_buffer(
            size,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            MemoryNeed::HOST_WRITABLE,
        )?;
        if buffer.pointer.is_none() {
            let _ = shared.release(buffer.handle);
            return Err(init_error(
                "upload memory",
                "suballocated upload buffer has no host mapping",
            ));
        }
        Ok(Self {
            buffer,
            instance_capacity,
            clip_capacity,
            clip_offset,
        })
    }
}

fn scissor_rect(rect: [i32; 4]) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D {
            x: rect[0].max(0),
            y: rect[1].max(0),
        },
        extent: vk::Extent2D {
            width: (rect[2] - rect[0]).max(0) as u32,
            height: (rect[3] - rect[1]).max(0) as u32,
        },
    }
}

fn scrolled_bounds(op: FrameOp, scroll: &[[f32; 2]]) -> DeviceRect {
    let offset = crate::paint::instance::scroll_offset(scroll, op);
    op.bounds().translate(DevicePoint::new(
        dp(offset[0].round() as i32),
        dp(offset[1].round() as i32),
    ))
}

fn draw_ops(
    shared: &SharedGpu,
    command: vk::CommandBuffer,
    pipeline: &Pipeline,
    geometry: &mut GeometryRegistry,
    payload: &FramePayload,
    root: RootUniform,
    scissor: [i32; 4],
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    let scissor_bounds = DeviceRect::from_edges(
        dp(scissor[0]),
        dp(scissor[1]),
        dp(scissor[2]),
        dp(scissor[3]),
    );
    for op in payload.ops() {
        if !scrolled_bounds(*op, payload.scroll()).overlaps_inclusive(scissor_bounds) {
            continue;
        }
        match *op {
            FrameOp::Instances {
                start, len, scroll, ..
            } => {
                if len == 0 {
                    continue;
                }
                let scroll_offset = payload
                    .scroll()
                    .get(scroll as usize)
                    .copied()
                    .unwrap_or([0.0, 0.0]);
                let uniform = root.with_instance_base(start).with_scroll(scroll_offset);
                let narrowed = crate::paint::instance::scrolled_scissor(
                    payload.scroll_clip(),
                    scroll,
                    scissor,
                );
                if let Some(rect) = narrowed {
                    if rect[0] >= rect[2] || rect[1] >= rect[3] {
                        continue;
                    }
                    unsafe {
                        shared
                            .raw()
                            .cmd_set_scissor(command, 0, &[scissor_rect(rect)]);
                    }
                }
                let push_bytes = bytemuck::cast_slice::<f32, u8>(uniform.words());
                unsafe {
                    shared.raw().cmd_bind_pipeline(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.quad(),
                    );
                    shared.raw().cmd_push_constants(
                        command,
                        pipeline.layout(),
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push_bytes,
                    );
                    shared.raw().cmd_draw(command, 4, len, 0, 0);
                }
                if narrowed.is_some() {
                    unsafe {
                        shared
                            .raw()
                            .cmd_set_scissor(command, 0, &[scissor_rect(scissor)]);
                    }
                }
                draw_calls = draw_calls.saturating_add(1);
            }
            FrameOp::Geometry {
                index,
                clip_index,
                clip_rect,
                ..
            } => {
                let draw = payload.geometry().get(index as usize).ok_or_else(|| {
                    EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!("geometry op index {index} out of range"),
                    }
                })?;
                geometry.touch(draw.id);
                let entry = geometry
                    .get(draw.id)
                    .ok_or_else(|| EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!(
                            "geometry {}:{} was not prepared before drawing",
                            draw.id.index(),
                            draw.id.generation()
                        ),
                    })?;
                let args = draw.draw_indexed(entry.index_count())?;
                let uniform = geometry_root(root, draw, clip_index, clip_rect);
                let push_bytes = bytemuck::cast_slice::<f32, u8>(uniform.words());
                unsafe {
                    shared.raw().cmd_bind_pipeline(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.geometry(entry.pipeline(), entry.topology()),
                    );
                    shared.raw().cmd_push_constants(
                        command,
                        pipeline.layout(),
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push_bytes,
                    );
                    shared
                        .raw()
                        .cmd_bind_vertex_buffers(command, 0, &[entry.vertex()], &[0]);
                    shared.raw().cmd_bind_index_buffer(
                        command,
                        entry.index(),
                        0,
                        entry.index_type(),
                    );
                    shared.raw().cmd_draw_indexed(
                        command,
                        args.count,
                        1,
                        args.first_index,
                        args.base_vertex,
                        0,
                    );
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok(draw_calls)
}

fn create_commands(
    shared: &SharedGpu,
) -> Result<(vk::CommandPool, [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT]), EngineError> {
    unsafe {
        let pool = shared
            .raw()
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(shared.queue_family())
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
            .map_err(|error| init_error("create_command_pool", format!("{error:?}")))?;
        let buffers = shared
            .raw()
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
            )
            .map_err(|error| init_error("allocate_command_buffers", format!("{error:?}")))?;
        let mut commands = [vk::CommandBuffer::null(); MAX_FRAMES_IN_FLIGHT];
        for (target, source) in commands.iter_mut().zip(buffers) {
            *target = source;
        }
        Ok((pool, commands))
    }
}

fn geometry_root(
    frame: RootUniform,
    draw: &crate::paint::instance::GeometryDrawGpu,
    clip_index: u32,
    clip_rect: [u32; 4],
) -> RootUniform {
    let rect = if clip_index == 0 {
        [0.0, 0.0, frame.words()[0], frame.words()[1]]
    } else {
        let x0 = f32::from_bits(clip_rect[0]);
        let y0 = f32::from_bits(clip_rect[1]);
        let x1 = f32::from_bits(clip_rect[2]);
        let y1 = f32::from_bits(clip_rect[3]);
        [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
    };
    RootUniform::geometry(frame, draw.transform, draw.color, clip_index, rect)
}

fn root_uniform(extent: vk::Extent2D, scale: f32) -> RootUniform {
    RootUniform::frame(extent.width as f32, extent.height as f32, scale)
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

fn plan_swapchain_copy(
    plan: CopyPlan,
    extent: vk::Extent2D,
    previous_hint: Option<DamageRects>,
) -> SwapchainCopy {
    let Some(regions) = plan.regions() else {
        return SwapchainCopy {
            step: ImageSyncStep::Full,
            hint: None,
        };
    };
    let clamped = regions.clamped(extent.width, extent.height);
    if clamped.is_empty() {
        return SwapchainCopy {
            step: ImageSyncStep::Nothing,
            hint: previous_hint,
        };
    }
    SwapchainCopy {
        step: ImageSyncStep::Rects(clamped),
        hint: Some(clamped),
    }
}

fn frame_written(frame: &FramePlan) -> Option<DamageRects> {
    if frame.is_full() {
        return None;
    }
    match frame.plan() {
        Plan::Partial(damage) => Some(damage),
        Plan::Idle | Plan::Full => None,
    }
}

fn image_copy(x: u32, y: u32, width: u32, height: u32) -> vk::ImageCopy {
    let layers = vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    vk::ImageCopy::default()
        .src_subresource(layers)
        .dst_subresource(layers)
        .src_offset(vk::Offset3D {
            x: x as i32,
            y: y as i32,
            z: 0,
        })
        .dst_offset(vk::Offset3D {
            x: x as i32,
            y: y as i32,
            z: 0,
        })
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
}

fn destination_update(last: Option<(i32, i32)>, surface_local: LogicalSize) -> Option<(i32, i32)> {
    let destination = dpi::viewport_destination(surface_local);
    (last != Some(destination)).then_some(destination)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use ash::vk;

    use super::{
        GpuRenderer, ImageSyncStep, MAX_RETIRED_SWAPCHAINS, MAX_RETRY_BACKOFF, OutOfDateRecovery,
        Retired, destination_update, out_of_date_backoff, out_of_date_diagnostic,
        plan_swapchain_copy,
    };
    use crate::{
        math::{DeviceRect, LogicalSize, dp, px},
        paint::atlas::AtlasKind,
        render::{
            DirtyRects,
            compositor::{CopyPlan, DamageRects, FramePlan, Plan},
            frame::FramePayload,
            linux::device::{SharedGpu, gpu_test_lock},
            timer::FrameTimings,
        },
        window::FrameIndex,
    };

    fn rects(list: &[DeviceRect]) -> DamageRects {
        let mut rects = DamageRects::new();
        for rect in list {
            assert!(rects.push(*rect));
        }
        rects
    }

    #[test]
    fn only_a_full_surface_copy_may_discard_the_image_content() {
        let extent = vk::Extent2D {
            width: 64,
            height: 64,
        };
        let region = rects(&[DeviceRect::new(dp(4), dp(8), dp(12), dp(16))]);
        let full = plan_swapchain_copy(CopyPlan::Full, extent, Some(region));
        assert_eq!(full.step, ImageSyncStep::Full);
        assert_eq!(full.hint, None, "整面拷贝不挂 region（整面语义）");
        let partial = plan_swapchain_copy(CopyPlan::Regions(region), extent, None);
        assert_eq!(
            partial.step,
            ImageSyncStep::Rects(region),
            "差分帧只允许 `PRESENT_SRC_KHR`（内容保留）作 oldLayout"
        );
        assert_eq!(partial.hint, Some(region), "hint 与实际拷贝的矩形逐字一致");
    }

    #[test]
    fn damage_entirely_outside_the_extent_copies_nothing_and_keeps_the_previous_hint() {
        let extent = vk::Extent2D {
            width: 32,
            height: 32,
        };
        let outside = rects(&[DeviceRect::new(dp(40), dp(0), dp(60), dp(20))]);
        let previous = rects(&[DeviceRect::new(dp(0), dp(0), dp(8), dp(8))]);
        let step = plan_swapchain_copy(CopyPlan::Regions(outside), extent, Some(previous));
        assert_eq!(step.step, ImageSyncStep::Nothing, "没有像素要搬，不碰布局");
        assert_eq!(
            step.hint,
            Some(previous),
            "沿用上一次的 hint，而不是声称整张 image 都变了"
        );
        let step = plan_swapchain_copy(CopyPlan::Regions(outside), extent, None);
        assert_eq!(step.step, ImageSyncStep::Nothing);
        assert_eq!(step.hint, None);
        let straddling = rects(&[DeviceRect::new(dp(28), dp(0), dp(40), dp(8))]);
        let clamped = rects(&[DeviceRect::new(dp(28), dp(0), dp(32), dp(8))]);
        let step = plan_swapchain_copy(CopyPlan::Regions(straddling), extent, None);
        assert_eq!(step.step, ImageSyncStep::Rects(clamped));
        assert_eq!(step.hint, Some(clamped));
    }

    #[test]
    fn a_headless_frame_produces_no_validation_errors() {
        let _guard = gpu_test_lock();
        let mut renderer = GpuRenderer::new_offscreen((32, 32)).expect("offscreen renderer");
        if !renderer.shared.caps().validation {
            eprintln!(
                "skip: Vulkan validation layer is not enabled \
                 (release build or KYTRIX_VK_VALIDATION=off)"
            );
            return;
        }
        let (errors_before, _) = crate::render::linux::instance::validation_counts();
        let mut frame = FramePlan::new(Plan::Full, true);
        let mut timings = FrameTimings::new();
        renderer
            .submit_snapshot(
                FrameIndex::from_raw(1),
                &FramePayload::empty(),
                &mut frame,
                &mut timings,
            )
            .expect("headless frame");
        let (errors_after, _) = crate::render::linux::instance::validation_counts();
        assert_eq!(
            errors_after, errors_before,
            "验证层报了 ERROR 级消息（VUID 见 stderr；strict 模式会直接 panic）"
        );
    }

    #[test]
    fn an_unsafe_teardown_marks_the_handles_leaked_before_releasing_the_ledgers() {
        let source = include_str!("compositor.rs");
        let leak = source
            .find("self.leak_gpu_handles();")
            .expect("不安全 teardown 必须调用 leak_gpu_handles()");
        let release = source
            .find("self.retired_persists.take_all();")
            .expect("延迟销毁台账仍必须被释放（只放 CPU 内存）");
        assert!(
            leak < release,
            "必须先标记泄漏再释放台账：否则 take_all 的 drop 会在泄漏标记之前销毁在飞句柄"
        );
        for (file, source) in [
            ("compositor.rs", source),
            ("swapchain.rs", include_str!("swapchain.rs")),
            ("device.rs", include_str!("device.rs")),
        ] {
            assert!(
                source.contains("handles.leaked()"),
                "{file}: 持有 Vulkan 句柄的 Drop 必须有 leaked() 守卫"
            );
        }
    }

    #[test]
    fn retired_entries_are_released_only_after_their_own_submission_completed() {
        let mut retired: Retired<u32> = Retired::new();
        assert!(retired.is_empty());
        assert_eq!(retired.len(), 0);
        retired.retire(7, 70);
        let mut released = Vec::new();
        retired.drain_ready(6, |value| released.push(value));
        assert_eq!(
            released,
            Vec::<u32>::new(),
            "tag-1：提交 7 仍在飞，不得释放"
        );
        assert_eq!(retired.len(), 1, "未完成的条目必须留在台账里（否则 UAF）");
        retired.drain_ready(7, |value| released.push(value));
        assert_eq!(released, vec![70], "completed == tag 才释放");
        assert!(retired.is_empty());
        retired.drain_ready(7, |value| released.push(value));
        assert_eq!(released, vec![70]);
    }

    #[test]
    fn multiple_retired_entries_are_released_in_fifo_order() {
        let mut retired: Retired<u32> = Retired::new();
        for (tag, value) in [(3, 30), (5, 50), (9, 90)] {
            retired.retire(tag, value);
        }
        let mut released = Vec::new();
        retired.drain_ready(5, |value| released.push(value));
        assert_eq!(released, vec![30, 50], "已完成的条目按入账顺序释放");
        assert_eq!(retired.len(), 1);
        released.clear();
        retired.drain_ready(8, |value| released.push(value));
        assert!(released.is_empty(), "completed=8 不覆盖 tag=9");
        retired.drain_ready(9, |value| released.push(value));
        assert_eq!(released, vec![90]);
        assert!(retired.is_empty());
    }

    #[test]
    fn take_all_returns_every_entry_in_order_and_empties_the_ledger() {
        let mut retired: Retired<u32> = Retired::new();
        assert!(retired.take_all().is_empty(), "空台账是 no-op");
        retired.retire(1, 10);
        retired.retire(4, 40);
        retired.retire(2, 20);
        let entries = retired.take_all();
        assert_eq!(entries, vec![(1, 10), (4, 40), (2, 20)]);
        assert!(retired.is_empty());
        assert_eq!(retired.len(), 0);
        assert!(retired.take_all().is_empty(), "再次调用仍是 no-op");
    }

    #[test]
    fn the_leak_path_visits_every_retired_entry() {
        let mut retired: Retired<u32> = Retired::new();
        retired.retire(1, 10);
        retired.retire(2, 20);
        for value in retired.iter_mut() {
            *value += 1;
        }
        assert_eq!(retired.take_all(), vec![(1, 11), (2, 21)]);
    }

    #[test]
    fn the_surface_is_recreated_only_after_its_swapchains_are_force_destroyed() {
        let source = include_str!("compositor.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("production code precedes the test module")
            .0;
        assert_eq!(
            production.matches("surface.recreate()").count(),
            1,
            "运行期 surface 重建必须只有一个入口（GpuRenderer::recreate_vk_surface）"
        );
        let entry = production
            .find("fn recreate_vk_surface(&mut self)")
            .expect("唯一入口必须存在");
        let entry = &production[entry..];
        assert!(
            entry[..entry
                .find("self.destroy_retired_swapchains_now()")
                .expect("先强制排空")]
                .contains("self.swapchain.is_none()"),
            "入口必须断言当前 swapchain 已被取出（没有第三份所有权）"
        );
        let forced_drain = entry
            .find("self.destroy_retired_swapchains_now()")
            .expect("重建 surface 之前必须先强制排空该 surface 的 swapchain");
        let recreate = entry
            .find("surface.recreate()")
            .expect("唯一调用点必须在入口里");
        assert!(
            forced_drain < recreate,
            "顺序违反 VUID-vkDestroySurfaceKHR-surface-01266：必须先销毁 swapchain 再销毁 surface"
        );
        let drain_entry = production
            .find("fn destroy_retired_swapchains_now(&mut self)")
            .expect("强制排空函数必须存在");
        let drain = &production[drain_entry..];
        let drain_end = drain.find("\n    fn ").unwrap_or(drain.len());
        let drain = &drain[..drain_end];
        let timeline = drain.find(".wait_idle(").expect("timeline 有界等待");
        let queue = drain.find(".wait_queue_idle(").expect("队列 idle 有界等待");
        let take = drain.find("take_all").expect("取出全部条目");
        assert!(
            timeline < take && queue < take,
            "必须先证明队列空闲（有界）再销毁链，否则是 R11-3 的同步违规"
        );
        let swapchain_field = production
            .find("\n    swapchain: Option<Swapchain>,\n")
            .expect("GpuRenderer 必须按声明顺序先销毁 swapchain");
        let surface_field = production
            .find("\n    vk_surface: Option<Surface>,\n")
            .expect("GpuRenderer 必须按声明顺序后销毁 surface");
        assert!(
            swapchain_field < surface_field,
            "字段顺序即销毁顺序：swapchain 字段必须声明在 vk_surface 之前"
        );
    }

    fn production_source() -> &'static str {
        include_str!("compositor.rs")
            .split_once("#[cfg(test)]")
            .expect("production code precedes the test module")
            .0
    }

    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} must exist"));
        let body = &source[start..];
        let end = body.find("\n    fn ").unwrap_or(body.len());
        &body[..end]
    }

    #[test]
    fn the_present_hot_path_and_the_ledger_drain_never_wait_for_the_queue() {
        let production = production_source();
        let presented = production
            .find("Present::Presented | Present::Suboptimal =>")
            .expect("present 成功分支必须存在");
        let out_of_date = production
            .find("Present::OutOfDate =>")
            .expect("present OOD 分支必须存在");
        let hot_path = &production[presented..out_of_date];
        for forbidden in [
            "wait_idle(",
            "wait_queue_idle(",
            "drain_retired_swapchains(",
            "destroy_retired_swapchains_now(",
        ] {
            assert!(
                !hot_path.contains(forbidden),
                "present 热路径不得出现 `{forbidden}`（C9：旧链销毁必须非阻塞）"
            );
        }
        let drain_ready = function_body(production, "fn drain_ready(&mut self");
        assert!(
            !drain_ready.contains("wait_idle") && !drain_ready.contains("wait_queue_idle"),
            "drain_ready 只做非阻塞的 timeline 比较：{drain_ready}"
        );
        assert!(
            drain_ready.contains("<= completed"),
            "释放判据必须是 `tag <= completed`（改 `<` 会泄漏、无条件释放是 UAF）"
        );
    }

    #[test]
    fn retired_swapchains_are_tagged_with_the_submission_sequence_not_a_frame_index() {
        let production = production_source();
        let retire = function_body(production, "fn retire_swapchain(&mut self");
        assert!(
            retire.contains("self.last_sequence"),
            "retire tag 必须取提交序号：{retire}"
        );
        assert!(
            !retire.contains("FrameIndex"),
            "禁止用 FrameIndex 当延迟销毁的 tag（R11-1）"
        );
        let install = function_body(production, "fn install_swapchain(");
        assert!(
            install.contains("self.retire_swapchain(old)"),
            "install_swapchain 必须经唯一 retire 点入账"
        );
        let replace = function_body(production, "fn try_replace_swapchain(");
        assert_eq!(
            replace.matches("self.retire_swapchain(old)").count(),
            2,
            "建链失败时旧链已 retired，两处都必须入账（R11-3）"
        );
    }

    #[test]
    fn the_retired_swapchain_ledger_has_a_hard_capacity_cap() {
        let production = production_source();
        assert_eq!(
            MAX_RETIRED_SWAPCHAINS,
            crate::render::frame::MAX_FRAMES_IN_FLIGHT + 1,
            "安全阀阈值 = MAX_FRAMES_IN_FLIGHT + 1"
        );
        let drain = function_body(production, "fn drain_retired_swapchains(");
        let gate = drain
            .find("if !self.current_presented_once")
            .expect("新链首 present 门必须存在");
        let valve = drain
            .find("MAX_RETIRED_SWAPCHAINS")
            .expect("容量安全阀必须存在");
        let forced = drain
            .find("destroy_retired_swapchains_now(")
            .expect("超限必须走 C7 的强制入口");
        assert!(valve < forced, "安全阀必须调用强制入口：{drain}");
        assert!(
            valve < gate,
            "先检查容量安全阀，再看新链首 present 门：{drain}"
        );
        let leak = function_body(production, "fn leak_gpu_handles(");
        for ledger in [
            "self.retired_persists.iter_mut()",
            "self.retired_swapchains.iter_mut()",
        ] {
            assert!(
                leak.contains(ledger),
                "leak 兜底必须遍历 `{ledger}`（否则无法证明空闲时会逐个 vkDestroy*）"
            );
        }
    }

    #[test]
    fn a_thousand_consecutive_out_of_date_events_never_escalate_to_a_fatal_error() {
        let mut recovery = OutOfDateRecovery::new();
        for _ in 0..1000 {
            let retry = recovery.on_out_of_date();
            assert!(
                retry.after > Duration::ZERO && retry.after <= MAX_RETRY_BACKOFF,
                "每次 OOD 都必须退避且不超过上限：{retry:?}"
            );
        }
        assert_eq!(recovery.out_of_date, 1000, "计数只用于诊断，不是错误阈值");
        let retry = recovery.on_out_of_date();
        assert!(retry.after <= MAX_RETRY_BACKOFF);
    }

    #[test]
    fn the_out_of_date_backoff_doubles_up_to_the_fifty_millisecond_cap() {
        let expected = [1u64, 2, 4, 8, 16, 32, 50, 50, 50];
        for (offset, millis) in expected.into_iter().enumerate() {
            let consecutive = offset as u32 + 1;
            assert_eq!(
                out_of_date_backoff(consecutive),
                Duration::from_millis(millis),
                "第 {consecutive} 次连续 OOD"
            );
        }
        assert_eq!(MAX_RETRY_BACKOFF, Duration::from_millis(50));
        for consecutive in 1..=u32::from(u16::MAX) {
            let after = out_of_date_backoff(consecutive);
            assert!(after > Duration::ZERO && after <= MAX_RETRY_BACKOFF);
        }
    }

    #[test]
    fn progress_resets_both_the_out_of_date_and_surface_lost_budgets() {
        let mut recovery = OutOfDateRecovery::new();
        assert_eq!(recovery.on_progress(), None, "没有 OOD 时没有恢复诊断");
        for _ in 0..5 {
            recovery.on_out_of_date();
        }
        assert_eq!(
            recovery.on_progress(),
            Some(5),
            "连续 5 次 OOD 后恢复：打一行低频诊断"
        );
        assert_eq!(recovery.out_of_date, 0);
        assert_eq!(recovery.on_out_of_date().after, Duration::from_millis(1));
        for expected in [1u64, 4, 16] {
            assert_eq!(
                recovery
                    .on_surface_lost_failure("test")
                    .expect("预算内的恢复失败必须返回退避"),
                Duration::from_millis(expected)
            );
        }
        recovery.on_progress();
        assert_eq!(
            recovery
                .on_surface_lost_failure("test")
                .expect("progress 之后预算重新开始"),
            Duration::from_millis(1),
        );
    }

    #[test]
    fn the_surface_lost_budget_reports_only_after_three_failed_retries() {
        let mut recovery = OutOfDateRecovery::new();
        for expected in [1u64, 4, 16] {
            assert_eq!(
                recovery
                    .on_surface_lost_failure("acquire reported SURFACE_LOST")
                    .expect("预算内的恢复失败必须返回退避"),
                Duration::from_millis(expected)
            );
        }
        let error = recovery
            .on_surface_lost_failure("acquire reported SURFACE_LOST")
            .expect_err("3 次重试之后必须上报");
        match error {
            crate::error::EngineError::Swapchain { detail } => {
                assert!(
                    detail.contains("still alive"),
                    "文案必须区分「连接还活着但 surface 不可用」：{detail}"
                );
                assert!(detail.contains("3 retries"), "{detail}");
                assert!(detail.contains("SURFACE_LOST"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn out_of_date_diagnostics_are_low_frequency() {
        let reported: Vec<u32> = (1..=200)
            .filter(|count| out_of_date_diagnostic(*count))
            .collect();
        assert_eq!(
            reported,
            vec![1, 16, 64, 128, 192],
            "第 1 次、第 16 次，之后每 64 次"
        );
        let lines = (1..=1000)
            .filter(|count| out_of_date_diagnostic(*count))
            .count();
        assert!(
            (16..=17).contains(&lines),
            "1000 次连续 OOD 的诊断行数应当 ≈17，实际 {lines}"
        );
    }

    #[test]
    fn a_destination_is_written_only_when_it_changes() {
        assert_eq!(
            destination_update(None, LogicalSize::new(px(800.0), px(600.0))),
            Some((800, 600))
        );
        assert_eq!(
            destination_update(Some((800, 600)), LogicalSize::new(px(800.0), px(600.0))),
            None,
            "逻辑尺寸没变就不下发"
        );
        assert_eq!(
            destination_update(Some((800, 600)), LogicalSize::new(px(701.0), px(501.0))),
            Some((701, 501))
        );
    }

    #[test]
    fn the_destination_is_the_logical_size_rounded_up() {
        assert_eq!(
            destination_update(None, LogicalSize::new(px(701.0), px(501.0))),
            Some((701, 501))
        );
        assert_eq!(
            destination_update(None, LogicalSize::new(px(700.2), px(500.8))),
            Some((701, 501)),
            "小数逻辑尺寸向上取整，否则最右 / 最下 1 px 被裁掉"
        );
        assert_eq!(
            destination_update(None, LogicalSize::new(px(0.0), px(0.0))),
            Some((1, 1)),
            "退化尺寸至少 1 px"
        );
    }

    #[test]
    fn dropping_a_renderer_releases_its_shared_device_resources() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        assert_eq!(shared.live_resources(), 0, "共享 GPU 创建时不注册资源");
        {
            let mut renderer = GpuRenderer::with_gpu(
                Arc::clone(&shared),
                None,
                None,
                (16, 16),
                [0.0; 4],
                None,
                None,
            )
            .expect("renderer");
            let rects = DirtyRects::full();
            renderer.queue_atlas_upload(
                AtlasKind::Glyph,
                0,
                crate::render::AtlasArraySpec {
                    page: 64,
                    layers: 1,
                },
                vec![0u8; (64 * 64) as usize],
                rects,
            );
            let mut frame = FramePlan::new(Plan::Full, true);
            let mut timings = FrameTimings::new();
            renderer
                .submit_snapshot(
                    FrameIndex::from_raw(1),
                    &FramePayload::empty(),
                    &mut frame,
                    &mut timings,
                )
                .expect("submit");
            assert!(
                shared.live_resources() > 0,
                "渲染器必须持有上传缓冲 / 图集 staging 资源"
            );
        }
        assert_eq!(
            shared.live_resources(),
            0,
            "关窗后渲染器注册的所有资源句柄必须注销"
        );
    }

    #[test]
    fn offscreen_renderers_share_the_process_gpu() {
        let _guard = gpu_test_lock();
        let first = GpuRenderer::new_offscreen((8, 8)).expect("first renderer");
        let second = GpuRenderer::new_offscreen((8, 8)).expect("second renderer");
        assert!(
            Arc::ptr_eq(&first.shared, &second.shared),
            "两个窗口必须共享同一个 SharedGpu（device / queue / PSO / 图集纹理）"
        );
        assert_ne!(
            first.descriptors.sets()[0],
            second.descriptors.sets()[0],
            "每个窗口在共享描述符池里必须有各自的段"
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn two_renderers_render_concurrently_on_one_shared_gpu() {
        use crate::{
            math::{DevicePixelRatio, LogicalRect, px},
            paint::{
                color::{Color, ColorResolver},
                instance::InstanceEncoder,
                primitive::{Primitive, Quad, QuadPaint},
                scene::Scene,
            },
        };

        fn red_quad_scene() -> Scene {
            let mut scene = Scene::empty();
            let lane = scene.new_lane();
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(px(2.0), px(2.0), px(2.0), px(2.0)),
                    corner_radius: [px(0.0); 4],
                    paint: QuadPaint::Solid {
                        fill: Color::srgb(1.0, 0.0, 0.0, 1.0),
                        border: None,
                    },
                }),
            ));
            scene.set_order(&[lane]);
            scene
        }

        fn pixel(image: &super::ReadbackImage, width: u32, x: u32, y: u32) -> [u8; 4] {
            let index = ((y * width + x) * 4) as usize;
            [
                image.rgba8[index],
                image.rgba8[index + 1],
                image.rgba8[index + 2],
                image.rgba8[index + 3],
            ]
        }

        let _guard = gpu_test_lock();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::with_capacity(2);
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let mut renderer = GpuRenderer::new_offscreen((8, 8)).expect("shared renderer");
                let rects = crate::render::DirtyRects::full();
                renderer.queue_atlas_upload(
                    AtlasKind::Glyph,
                    0,
                    crate::render::AtlasArraySpec {
                        page: 64,
                        layers: 1,
                    },
                    vec![0x40u8; 64 * 64],
                    rects,
                );
                let scene = red_quad_scene();
                let mut encoder = InstanceEncoder::new();
                let buffer = encoder
                    .encode(
                        &scene,
                        DevicePixelRatio::ONE,
                        ColorResolver,
                        &crate::paint::geometry_store::GeometryStore::new(),
                    )
                    .expect("encode");
                barrier.wait();
                let image = renderer
                    .render_snapshot(buffer)
                    .expect("concurrent snapshot");
                assert_eq!(pixel(&image, 8, 3, 3), [255, 0, 0, 255]);
                assert_eq!(pixel(&image, 8, 0, 0), [0, 0, 0, 0]);
            }));
        }
        for thread in threads {
            thread.join().expect("concurrent gpu thread must not panic");
        }
    }
}
