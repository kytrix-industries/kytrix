use std::{ffi::c_void, ptr::NonNull, sync::Arc, time::Instant};

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLDevice, MTLDrawable, MTLIndexType, MTLLoadAction, MTLOrigin,
    MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLResourceOptions, MTLScissorRect, MTLSize, MTLStoreAction, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage, MTLViewport,
};
use objc2_quartz_core::CAMetalDrawable;

#[cfg(any(test, feature = "test-support"))]
use crate::paint::instance::InstanceBuffer;
#[cfg(any(test, feature = "test-support"))]
use crate::render::macos::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    math::{DevicePoint, DeviceRect, dp},
    paint::{atlas::AtlasKind, instance::FrameOp},
    render::{
        compositor::{DamageRects, FramePlan, MAX_DRAW_PASSES, Plan},
        damage::MAX_DAMAGE_RECTS,
        frame::{FramePayload, FrameSlot, InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        macos::{
            atlas::AtlasTextures,
            core::{GpuCore, PersistLease},
            device::{Device, GpuObject, init_error, on_gpu_thread},
            geometry::GeometryRegistry,
            pipeline::{BlendMode, PipelineSet},
            present,
            queue::{InFlight, Queue},
            swapchain::MAXIMUM_DRAWABLE_COUNT,
        },
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::RootUniform,
    },
    window::FrameIndex,
};

const ROOT_UNIFORM_BYTES: usize = 112;

const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm_sRGB;

pub(crate) struct GpuRenderer {
    core: Arc<GpuCore>,
    persist_lease: PersistLease,
    device: Device,
    queue: Queue,
    pending_slot: Option<FrameSlot>,
    pipeline: Option<PipelineSet>,
    atlas: AtlasTextures,
    persist: Persist,
    drawable_syncs: DrawableSyncs,
    upload: UploadArena,
    mirror: InstanceMirror,
    clear_color: [f32; 4],
    surface: (u32, u32),
    scale: f32,
    geometry: GeometryRegistry,
    fresh: bool,
    last_sequence: u64,
    window_target: bool,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

unsafe impl Send for GpuRenderer {}

impl GpuRenderer {
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        let core = GpuCore::shared()?;
        Self::with_device(
            Device::new()?,
            core,
            Arc::new(InFlight::new()),
            size,
            false,
            [0.0; 4],
        )
    }

    pub(crate) fn new_window(
        device: Device,
        core: Arc<GpuCore>,
        in_flight: Arc<InFlight>,
        size: (u32, u32),
        background_linear: [f32; 4],
    ) -> Result<Self, EngineError> {
        Self::with_device(device, core, in_flight, size, true, background_linear)
    }

    fn with_device(
        device: Device,
        core: Arc<GpuCore>,
        in_flight: Arc<InFlight>,
        size: (u32, u32),
        window_target: bool,
        clear_color: [f32; 4],
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let queue = Queue::new(&device, in_flight)?;
        let mut device = device;
        let atlas = AtlasTextures::new(&mut device)?;
        let persist_lease = PersistLease::new(Arc::clone(&core), size)?;
        let persist = Persist::new(&mut device, size)?;
        let upload = UploadArena::new(&mut device)?;
        let mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        Ok(Self {
            core,
            persist_lease,
            device,
            queue,
            pipeline: None,
            atlas,
            persist,
            drawable_syncs: DrawableSyncs::new(),
            upload,
            mirror,
            clear_color,
            surface: size,
            scale: 1.0,
            geometry: GeometryRegistry::new(),
            pending_slot: None,
            fresh: true,
            last_sequence: 0,
            window_target,
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
    }

    fn ensure_pipeline(&mut self) -> Result<(), EngineError> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let set = self
            .core
            .pipeline_set(TARGET_FORMAT, BlendMode::Premultiplied)?;
        self.pipeline = Some(set);
        Ok(())
    }

    fn resolved_pipeline(&self) -> &PipelineSet {
        self.pipeline
            .as_ref()
            .expect("ensure_pipeline must run before recording a frame")
    }

    fn debug_assert_gpu_thread(&self, entry: &'static str) {
        debug_assert!(
            !self.window_target || on_gpu_thread(),
            "Metal window target: {entry} may only run on the kytrix-metal thread"
        );
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    pub(crate) fn try_acquire_upload_slot(&mut self) -> Option<FrameSlot> {
        self.pending_slot = self.queue.try_acquire();
        self.pending_slot
    }

    pub(crate) fn release_upload_slot(&mut self) {
        if let Some(slot) = self.pending_slot.take() {
            self.queue.release(slot);
        }
    }

    pub(crate) fn prepare_target(&mut self, size: (u32, u32)) -> Result<bool, EngineError> {
        self.debug_assert_gpu_thread("prepare_target");
        let size = (size.0.max(1), size.1.max(1));
        if size == self.surface {
            return Ok(false);
        }
        self.persist_lease.resize(size)?;
        let old = self.persist.recreate(&mut self.device, size)?;
        self.queue.retire(self.last_sequence, old);
        self.surface = size;
        self.fresh = true;
        self.drawable_syncs.entries.clear();
        Ok(true)
    }

    pub(crate) fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    pub(crate) fn set_background_color(&mut self, linear: [f32; 4]) {
        self.debug_assert_gpu_thread("set_background_color");
        self.clear_color = linear;
    }

    pub(crate) fn submit_frame(
        &mut self,
        request: &crate::render::frame::FrameRequest,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        frame: &mut FramePlan,
        completed: impl Fn(Retained<ProtocolObject<dyn MTLCommandBuffer>>) + Send + 'static,
    ) -> Result<(crate::render::RenderStats, Vec<GpuTimerSample>), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "Metal window frames may only be submitted on the kytrix-metal thread"
        );
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        self.scale = request.scale;
        let mut timings = FrameTimings::new();
        timings.reset(request.index);
        let (slot, command, stats) = self.record_frame(
            request.index,
            &request.payload,
            frame,
            Some(drawable),
            &mut timings,
        )?;
        let mtl_drawable: &ProtocolObject<dyn MTLDrawable> = drawable.as_ref();
        present::present(&command, mtl_drawable);
        let present_started = Instant::now();
        self.queue
            .submit(request.index, slot, &command, completed)?;
        self.fresh = false;
        frame.presented();
        self.last_sequence = slot.signal_value;
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        debug_assert!(
            timings.samples().len() <= 3,
            "at most three sections per frame"
        );
        Ok((stats, timings.samples().to_vec()))
    }

    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<crate::render::RenderStats, EngineError> {
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        timings.reset(index);
        let (slot, command, stats) = self.record_frame(index, payload, frame, None, timings)?;
        if let Some(sample) = self
            .queue
            .submit_blocking(index, slot, &command, &mut self.device)?
        {
            timings.push(sample);
        }
        let present_started = Instant::now();
        self.fresh = false;
        frame.presented();
        self.last_sequence = slot.signal_value;
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        Ok(stats)
    }

    pub(crate) fn on_frame_completed(
        &mut self,
        index: FrameIndex,
        sequence: u64,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<(Option<GpuTimerSample>, Option<FrameIndex>), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "command buffer completions may only be drained on the kytrix-metal thread"
        );
        if buffer.status() == MTLCommandBufferStatus::Error {
            return Err(self.queue.failure(index, buffer, &mut self.device));
        }
        Ok(self
            .queue
            .on_completed(index, sequence, buffer, &mut self.device))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        self.debug_assert_gpu_thread("readback");
        self.readback.capture(
            &self.device,
            &self.queue,
            self.persist.texture(),
            self.surface,
            0,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        let payload = FramePayload::full(buffer, Vec::new(), Vec::new());
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit_snapshot(FrameIndex::from_raw(1), &payload, &mut frame, &mut timings)?;
        self.readback()
    }

    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: Vec<u8>,
        rects: crate::render::atlas::DirtyRects,
    ) {
        self.debug_assert_gpu_thread("queue_atlas_upload");
        self.atlas.queue_upload(kind, layer, spec, pixels, rects);
    }

    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas.pending(kind)
    }

    fn record_frame(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &FramePlan,
        drawable: Option<&ProtocolObject<dyn CAMetalDrawable>>,
        timings: &mut FrameTimings,
    ) -> Result<
        (
            FrameSlot,
            Retained<ProtocolObject<dyn MTLCommandBuffer>>,
            crate::render::RenderStats,
        ),
        EngineError,
    > {
        self.debug_assert_gpu_thread("record_frame");
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        debug_assert!(
            !matches!(frame.plan(), Plan::Partial(damage) if damage.is_empty()),
            "a partial plan must carry a non-empty dirty set"
        );
        let frame_started = Instant::now();
        self.ensure_pipeline()?;
        let slot = match self.pending_slot.take() {
            Some(slot) => slot,
            None => self.queue.try_acquire().ok_or_else(|| EngineError::AppInit {
                detail: smol_str::format_smolstr!(
                    "record_frame could not acquire an upload slot (all {} slots busy on a synchronous path)",
                    crate::render::frame::MAX_FRAMES_IN_FLIGHT
                ),
            })?,
        };
        let command = self.queue.begin()?;
        let (instance_count, _clip_count) = self.upload.write(
            &mut self.device,
            &mut self.queue,
            &mut self.mirror,
            slot.signal_value,
            slot.index,
            payload,
        )?;
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(frame_started),
            gpu_ns: 0,
        });

        let full = frame.is_full();
        let mut draw_calls = 0u32;
        let mut geometry_uploaded_bytes = 0usize;
        let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
        self.geometry.begin_frame();
        let _rebuilt = self
            .atlas
            .prepare(&mut self.device, &mut self.queue, slot.signal_value)?;
        if self.atlas.has_pending() {
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| init_error("blitCommandEncoder(atlas)", "returned nil"))?;
            self.atlas.record_uploads(
                &mut self.device,
                &mut self.queue,
                slot.signal_value,
                slot.index,
                &blit,
            )?;
            blit.endEncoding();
        }
        if !payload.geometry().is_empty() {
            geometry_uploaded_bytes = self.geometry.prepare(
                &mut self.device,
                &command,
                payload.geometry(),
                &mut geometry_retired,
            )?;
        }
        {
            let pass = MTLRenderPassDescriptor::new();
            let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            color.setTexture(Some(self.persist.texture()));
            color.setLoadAction(if full {
                MTLLoadAction::Clear
            } else {
                MTLLoadAction::Load
            });
            color.setStoreAction(MTLStoreAction::Store);
            let [r, g, b, a] = self.clear_color;
            color.setClearColor(MTLClearColor {
                red: f64::from(r),
                green: f64::from(g),
                blue: f64::from(b),
                alpha: f64::from(a),
            });
            let encoder = command
                .renderCommandEncoderWithDescriptor(&pass)
                .ok_or_else(|| init_error("renderCommandEncoder", "returned nil"))?;
            let root = self.bind(&encoder, slot.index);
            let instances = self.upload.slot(slot.index).0.clone();
            let pipeline = self
                .pipeline
                .as_ref()
                .expect("ensure_pipeline must run before recording a frame");
            match full {
                true => {
                    encoder.setScissorRect(whole_scissor(self.surface));
                    draw_calls = draw_ops(
                        &encoder,
                        &instances,
                        payload,
                        instance_count,
                        [0, 0, self.surface.0 as i32, self.surface.1 as i32],
                        root,
                        pipeline,
                        &mut self.geometry,
                    )?;
                }
                false => match frame.plan() {
                    Plan::Idle | Plan::Full => {}
                    Plan::Partial(damage) => {
                        for rect in damage.as_slice() {
                            let Some(scissor) = clipped_rect(rect, self.surface) else {
                                continue;
                            };
                            encoder.setScissorRect(scissor);
                            draw_calls = draw_calls.saturating_add(draw_ops(
                                &encoder,
                                &instances,
                                payload,
                                instance_count,
                                [
                                    rect.x0.value(),
                                    rect.y0.value(),
                                    rect.x1.value(),
                                    rect.y1.value(),
                                ],
                                root,
                                pipeline,
                                &mut self.geometry,
                            )?);
                        }
                    }
                },
            }
            encoder.endEncoding();
        }
        self.geometry.retain_recent(&mut geometry_retired);
        for handle in geometry_retired {
            self.queue.retire(slot.signal_value, handle);
        }
        if let Some(drawable) = drawable {
            let target = drawable.texture();
            let damage = match frame.present_dirty() {
                Some(damage) => FrameDamage::Partial(damage),
                None => FrameDamage::Full,
            };
            let mtl_drawable: &ProtocolObject<dyn MTLDrawable> = drawable.as_ref();
            let copy = self.drawable_syncs.plan(
                mtl_drawable.drawableID(),
                (target.width() as u32, target.height() as u32),
                damage,
                self.surface,
            );
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| init_error("blitCommandEncoder", "returned nil"))?;
            match copy {
                DrawableCopy::Full => blit_persist_rect(
                    &blit,
                    self.persist.texture(),
                    &target,
                    whole_scissor(self.surface),
                ),
                DrawableCopy::Rects(rects) => {
                    for rect in rects.as_slice() {
                        let Some(region) = clipped_rect(rect, self.surface) else {
                            continue;
                        };
                        blit_persist_rect(&blit, self.persist.texture(), &target, region);
                    }
                }
            }
            blit.endEncoding();
        }
        Ok((
            slot,
            command,
            crate::render::RenderStats {
                draw_calls,
                instances: payload.drawn_instances(),
                geometry_uploaded_bytes,
            },
        ))
    }

    fn bind(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        slot: usize,
    ) -> RootUniform {
        encoder.setRenderPipelineState(self.resolved_pipeline().quad());
        let (instances, clips) = self.upload.slot(slot);
        let root = root_uniform(self.surface, self.scale);
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                NonNull::from(root.words()).cast::<c_void>(),
                ROOT_UNIFORM_BYTES,
                2,
            );
            encoder.setVertexBuffer_offset_atIndex(Some(instances), 0, 0);
            encoder.setFragmentBuffer_offset_atIndex(Some(instances), 0, 0);
            encoder.setFragmentBuffer_offset_atIndex(Some(clips), 0, 1);
            encoder.setFragmentTexture_atIndex(Some(self.atlas.texture(AtlasKind::Glyph)), 0);
            encoder.setFragmentTexture_atIndex(Some(self.atlas.texture(AtlasKind::Image)), 1);
            encoder.setFragmentSamplerState_atIndex(Some(self.core.sampler()), 0);
            encoder.setViewport(MTLViewport {
                originX: 0.0,
                originY: 0.0,
                width: f64::from(self.surface.0),
                height: f64::from(self.surface.1),
                znear: 0.0,
                zfar: 1.0,
            });
        }
        root
    }
}

const DRAWABLE_SYNC_SLOTS: usize = MAXIMUM_DRAWABLE_COUNT;

#[derive(Clone, Copy, Debug)]
enum FrameDamage {
    Full,
    Partial(DamageRects),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrawableCopy {
    Full,
    Rects(DamageRects),
}

#[derive(Clone, Copy, Debug)]
struct DrawableSyncState {
    covered: bool,
    pending: DamageRects,
    stale: bool,
}

impl DrawableSyncState {
    const fn new() -> Self {
        Self {
            covered: false,
            pending: DamageRects::new(),
            stale: false,
        }
    }

    fn take_copy(&mut self, frame: FrameDamage, surface: (u32, u32)) -> DrawableCopy {
        let copy = if !self.covered || self.stale {
            DrawableCopy::Full
        } else {
            match frame {
                FrameDamage::Full => DrawableCopy::Full,
                FrameDamage::Partial(damage) => {
                    let mut rects = self.pending;
                    merge_damage(&mut rects, &damage);
                    let surface_area = u64::from(surface.0) * u64::from(surface.1);
                    if copy_area(&rects, surface) >= surface_area {
                        DrawableCopy::Full
                    } else {
                        DrawableCopy::Rects(rects)
                    }
                }
            }
        };
        *self = Self {
            covered: true,
            pending: DamageRects::new(),
            stale: false,
        };
        copy
    }

    fn record_missed(&mut self, frame: FrameDamage) {
        match frame {
            FrameDamage::Full => {
                self.stale = true;
                self.pending = DamageRects::new();
            }
            FrameDamage::Partial(damage) if !self.stale => merge_damage(&mut self.pending, &damage),
            FrameDamage::Partial(_) => {}
        }
    }
}

fn merge_damage(pending: &mut DamageRects, damage: &DamageRects) {
    for rect in damage.as_slice() {
        if pending.push(*rect) {
            continue;
        }
        pending.coalesce(MAX_DAMAGE_RECTS - MAX_DRAW_PASSES);
        assert!(
            pending.push(*rect),
            "a frame's damage set exceeds the draw-pass budget"
        );
    }
}

fn copy_area(rects: &DamageRects, surface: (u32, u32)) -> u64 {
    rects
        .as_slice()
        .iter()
        .filter_map(|rect| clipped_rect(rect, surface))
        .map(|rect| rect.width as u64 * rect.height as u64)
        .sum()
}

struct DrawableSyncs {
    entries: Vec<DrawableEntry>,
}

struct DrawableEntry {
    id: usize,
    state: DrawableSyncState,
}

impl DrawableSyncs {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(DRAWABLE_SYNC_SLOTS),
        }
    }

    fn plan(
        &mut self,
        id: usize,
        size: (u32, u32),
        frame: FrameDamage,
        surface: (u32, u32),
    ) -> DrawableCopy {
        if size != surface {
            self.entries.clear();
            return DrawableCopy::Full;
        }
        let current = match self.entries.iter().position(|entry| entry.id == id) {
            Some(index) => {
                if index + 1 != self.entries.len() {
                    let entry = self.entries.remove(index);
                    self.entries.push(entry);
                }
                self.entries.len() - 1
            }
            None => {
                if self.entries.len() == DRAWABLE_SYNC_SLOTS {
                    self.entries.remove(0);
                }
                self.entries.push(DrawableEntry {
                    id,
                    state: DrawableSyncState::new(),
                });
                self.entries.len() - 1
            }
        };
        let copy = self.entries[current].state.take_copy(frame, surface);
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if index != current {
                entry.state.record_missed(frame);
            }
        }
        copy
    }
}

struct Persist {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    handle: GpuResourceHandle,
}

impl Persist {
    fn new(device: &mut Device, size: (u32, u32)) -> Result<Self, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        Ok(Self { texture, handle })
    }

    fn recreate(
        &mut self,
        device: &mut Device,
        size: (u32, u32),
    ) -> Result<GpuResourceHandle, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        let old = std::mem::replace(&mut self.handle, handle);
        self.texture = texture;
        Ok(old)
    }

    fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }
}

fn create_target(
    device: &Device,
    size: (u32, u32),
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, EngineError> {
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm_sRGB,
            size.0.max(1) as usize,
            size.1.max(1) as usize,
            false,
        )
    };
    descriptor.setUsage(MTLTextureUsage::RenderTarget);
    descriptor.setStorageMode(objc2_metal::MTLStorageMode::Private);
    device
        .raw()
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| init_error("newTextureWithDescriptor(persist)", "returned nil"))
}

struct UploadArena {
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    instances: Retained<ProtocolObject<dyn MTLBuffer>>,
    instance_handle: GpuResourceHandle,
    instance_capacity: u32,
    clips: Retained<ProtocolObject<dyn MTLBuffer>>,
    clip_handle: GpuResourceHandle,
    clip_capacity: u32,
}

impl UploadArena {
    fn new(device: &mut Device) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let instances = create_upload(device, 256 * 128)?;
            let instance_handle = device.allocate(GpuObject::Buffer(instances.clone()));
            let clips = create_upload(device, 8 * 48)?;
            let clip_handle = device.allocate(GpuObject::Buffer(clips.clone()));
            slots.push(UploadSlot {
                instances,
                instance_handle,
                instance_capacity: 256,
                clips,
                clip_handle,
                clip_capacity: 8,
            });
        }
        Ok(Self { slots })
    }

    fn slot(
        &self,
        slot: usize,
    ) -> (
        &Retained<ProtocolObject<dyn MTLBuffer>>,
        &Retained<ProtocolObject<dyn MTLBuffer>>,
    ) {
        let slot = &self.slots[slot];
        (&slot.instances, &slot.clips)
    }

    fn write(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        mirror: &mut InstanceMirror,
        sequence: u64,
        slot: usize,
        payload: &FramePayload,
    ) -> Result<(u32, u32), EngineError> {
        let instance_count = payload.instance_count();
        let clips = payload.clips();
        let clip_count = clips.len() as u32;
        if instance_count > self.slots[slot].instance_capacity {
            let capacity = instance_count.next_power_of_two().max(256);
            let buffer = create_upload(device, u64::from(capacity) * 128)?;
            let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
            let old = self.slots[slot].instance_handle;
            self.slots[slot].instances = buffer;
            self.slots[slot].instance_handle = handle;
            self.slots[slot].instance_capacity = capacity;
            queue.retire(sequence, old);
            mirror.invalidate(slot);
        }
        if clip_count > self.slots[slot].clip_capacity {
            let capacity = clip_count.next_power_of_two().max(8);
            let buffer = create_upload(device, u64::from(capacity) * 48)?;
            let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
            let old = self.slots[slot].clip_handle;
            self.slots[slot].clips = buffer;
            self.slots[slot].clip_handle = handle;
            self.slots[slot].clip_capacity = capacity;
            queue.retire(sequence, old);
        }
        let buffer = &self.slots[slot].instances;
        let full = mirror.prepare(slot, payload);
        if full {
            write_upload(buffer, &mirror.bytes()[..instance_count as usize * 128]);
        } else {
            for range in mirror.writes() {
                let start = range.start as usize * 128;
                let end = range.end as usize * 128;
                write_upload_at(buffer, start, &mirror.bytes()[start..end]);
            }
        }
        mirror.finish(slot);
        write_upload(&self.slots[slot].clips, bytemuck::cast_slice(clips));
        Ok((instance_count, clip_count))
    }
}

fn create_upload(
    device: &Device,
    bytes: u64,
) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, EngineError> {
    device
        .raw()
        .newBufferWithLength_options(bytes as usize, MTLResourceOptions::StorageModeShared)
        .ok_or_else(|| init_error("newBufferWithLength_options(upload)", "returned nil"))
}

fn write_upload(buffer: &ProtocolObject<dyn MTLBuffer>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            buffer.contents().as_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
}

fn write_upload_at(buffer: &ProtocolObject<dyn MTLBuffer>, offset: usize, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            buffer.contents().as_ptr().cast::<u8>().add(offset),
            bytes.len(),
        );
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

fn root_uniform(surface: (u32, u32), scale: f32) -> RootUniform {
    RootUniform::frame(surface.0 as f32, surface.1 as f32, scale)
}

fn scrolled_bounds(op: FrameOp, scroll: &[[f32; 2]]) -> DeviceRect {
    let offset = crate::paint::instance::scroll_offset(scroll, op);
    op.bounds().translate(DevicePoint::new(
        dp(offset[0].round() as i32),
        dp(offset[1].round() as i32),
    ))
}

fn draw_ops(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    instances: &Retained<ProtocolObject<dyn MTLBuffer>>,
    payload: &FramePayload,
    instance_count: u32,
    scissor: [i32; 4],
    root: RootUniform,
    pipeline: &PipelineSet,
    geometry: &mut GeometryRegistry,
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
                debug_assert!(start.saturating_add(len) <= instance_count);
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
                    encoder.setScissorRect(scissor_rect(rect));
                }
                unsafe {
                    encoder.setRenderPipelineState(pipeline.quad());
                    encoder.setVertexBytes_length_atIndex(
                        NonNull::from(uniform.words()).cast::<c_void>(),
                        ROOT_UNIFORM_BYTES,
                        2,
                    );
                    encoder.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                        MTLPrimitiveType::TriangleStrip,
                        0usize,
                        4usize,
                        len as usize,
                        0usize,
                    );
                }
                if narrowed.is_some() {
                    encoder.setScissorRect(scissor_rect(scissor));
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
                debug_assert_eq!(args.base_vertex, 0, "Metal 没有 base vertex 参数");
                let index_offset = args.first_index as usize * entry.index_type().size() as usize;
                let index_count = args.count as usize;
                unsafe {
                    encoder.setRenderPipelineState(pipeline.geometry(entry.pipeline()));
                    encoder.setVertexBytes_length_atIndex(
                        NonNull::from(uniform.words()).cast::<c_void>(),
                        ROOT_UNIFORM_BYTES,
                        2,
                    );
                    encoder.setVertexBuffer_offset_atIndex(Some(entry.vertex()), 0, 0);
                    encoder.setFragmentBytes_length_atIndex(
                        NonNull::from(uniform.words()).cast::<c_void>(),
                        ROOT_UNIFORM_BYTES,
                        2,
                    );
                    encoder
                        .drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset(
                            metal_topology(entry.topology()),
                            index_count,
                            metal_index_type(entry.index_type()),
                            entry.index(),
                            index_offset,
                        );
                    encoder.setVertexBuffer_offset_atIndex(Some(instances), 0, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok(draw_calls)
}

fn metal_topology(topology: crate::paint::geometry::Topology) -> MTLPrimitiveType {
    match topology {
        crate::paint::geometry::Topology::TriangleList => MTLPrimitiveType::Triangle,
        crate::paint::geometry::Topology::TriangleStrip => MTLPrimitiveType::TriangleStrip,
    }
}

fn metal_index_type(kind: crate::paint::geometry::IndexType) -> MTLIndexType {
    match kind {
        crate::paint::geometry::IndexType::U16 => MTLIndexType::UInt16,
        crate::paint::geometry::IndexType::U32 => MTLIndexType::UInt32,
    }
}

fn blit_persist_rect(
    blit: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    persist: &ProtocolObject<dyn MTLTexture>,
    target: &ProtocolObject<dyn MTLTexture>,
    region: MTLScissorRect,
) {
    let origin = MTLOrigin {
        x: region.x,
        y: region.y,
        z: 0,
    };
    let size = MTLSize {
        width: region.width,
        height: region.height,
        depth: 1,
    };
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
            persist, 0, 0, origin, size, target, 0, 0, origin,
        );
    }
}

fn scissor_rect(rect: [i32; 4]) -> MTLScissorRect {
    MTLScissorRect {
        x: rect[0].max(0) as usize,
        y: rect[1].max(0) as usize,
        width: (rect[2] - rect[0]).max(0) as usize,
        height: (rect[3] - rect[1]).max(0) as usize,
    }
}

fn whole_scissor(surface: (u32, u32)) -> MTLScissorRect {
    MTLScissorRect {
        x: 0,
        y: 0,
        width: surface.0 as usize,
        height: surface.1 as usize,
    }
}

fn clipped_rect(rect: &crate::math::DeviceRect, surface: (u32, u32)) -> Option<MTLScissorRect> {
    let limit_x = surface.0.min(i32::MAX as u32) as i32;
    let limit_y = surface.1.min(i32::MAX as u32) as i32;
    let x0 = rect.x0.value().clamp(0, limit_x);
    let y0 = rect.y0.value().clamp(0, limit_y);
    let x1 = rect.x1.value().clamp(0, limit_x);
    let y1 = rect.y1.value().clamp(0, limit_y);
    let width = x1.saturating_sub(x0) as u32;
    let height = y1.saturating_sub(y0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(MTLScissorRect {
        x: x0 as usize,
        y: y0 as usize,
        width: width as usize,
        height: height as usize,
    })
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::{
        DRAWABLE_SYNC_SLOTS, DrawableCopy, DrawableSyncState, DrawableSyncs, FrameDamage,
        clipped_rect, merge_damage, whole_scissor,
    };
    use crate::{
        math::{DeviceRect, dp},
        render::{
            compositor::{DamageRects, MAX_DRAW_PASSES},
            damage::MAX_DAMAGE_RECTS,
        },
    };

    fn damage(rects: &[(i32, i32, i32, i32)]) -> DamageRects {
        let mut set = DamageRects::new();
        for &(x0, y0, x1, y1) in rects {
            assert!(set.push(DeviceRect::new(dp(x0), dp(y0), dp(x1), dp(y1))));
        }
        set
    }

    fn present(
        states: &mut [DrawableSyncState; 3],
        current: usize,
        frame: FrameDamage,
        surface: (u32, u32),
    ) -> DrawableCopy {
        let copy = states[current].take_copy(frame, surface);
        for (index, state) in states.iter_mut().enumerate() {
            if index != current {
                state.record_missed(frame);
            }
        }
        copy
    }

    #[test]
    fn damage_inside_the_surface_is_preserved() {
        let rect =
            clipped_rect(&DeviceRect::new(dp(4), dp(6), dp(40), dp(60)), (100, 80)).expect("rect");
        assert_eq!(rect.x, 4);
        assert_eq!(rect.y, 6);
        assert_eq!(rect.width, 36);
        assert_eq!(rect.height, 54);
    }

    #[test]
    fn one_pixel_dpi_rounding_clips_damage_to_the_drawable() {
        let rect = clipped_rect(
            &DeviceRect::new(dp(1398), dp(10), dp(1401), dp(20)),
            (1399, 1040),
        )
        .expect("rect");
        assert_eq!(rect.x, 1398);
        assert_eq!(rect.y, 10);
        assert_eq!(rect.width, 1, "只保留落在 drawable 内的 1 px");
        assert_eq!(rect.height, 10);
    }

    #[test]
    fn a_partial_damage_rect_past_the_edge_is_skipped_not_left_empty() {
        let surface = (1399, 1040);
        assert!(
            clipped_rect(
                &DeviceRect::new(dp(1399), dp(0), dp(1400), dp(1040)),
                surface
            )
            .is_none(),
            "整列都在 drawable 之外"
        );
        let rect = clipped_rect(
            &DeviceRect::new(dp(1398), dp(0), dp(1400), dp(1040)),
            surface,
        )
        .expect("rect");
        assert_eq!((rect.x, rect.width), (1398, 1));
    }

    #[test]
    fn a_damage_rect_entirely_outside_the_surface_has_no_region() {
        let surface = (1400, 1040);
        for rect in [
            DeviceRect::new(dp(1400), dp(0), dp(1420), dp(10)),
            DeviceRect::new(dp(-20), dp(0), dp(0), dp(10)),
            DeviceRect::new(dp(0), dp(1040), dp(10), dp(1060)),
            DeviceRect::new(dp(0), dp(-20), dp(10), dp(0)),
            DeviceRect::new(dp(2000), dp(2000), dp(2010), dp(2010)),
        ] {
            assert!(
                clipped_rect(&rect, surface).is_none(),
                "{rect:?} 在目标之外必须没有区域"
            );
        }
    }

    #[test]
    fn damage_straddling_the_origin_is_clamped_to_zero() {
        let rect =
            clipped_rect(&DeviceRect::new(dp(-8), dp(-4), dp(6), dp(10)), (100, 50)).expect("rect");
        assert_eq!((rect.x, rect.y), (0, 0));
        assert_eq!((rect.width, rect.height), (6, 10));
    }

    #[test]
    fn the_full_frame_region_covers_the_target() {
        let rect = whole_scissor((1400, 1040));
        assert_eq!((rect.x, rect.y), (0, 0));
        assert_eq!((rect.width, rect.height), (1400, 1040));
    }

    #[test]
    fn a_drawable_without_a_painted_region_copies_the_whole_surface() {
        let surface = (100, 80);
        let mut state = DrawableSyncState::new();
        assert!(
            !state.covered,
            "新 drawable 的已绘制区域为空：未写像素的内容未定义"
        );
        assert_eq!(
            state.take_copy(FrameDamage::Partial(damage(&[(10, 10, 20, 20)])), surface),
            DrawableCopy::Full
        );
        assert!(state.covered, "整面拷之后已绘制区域覆盖整面");
        assert!(state.pending.is_empty(), "补拷集已被消费");
    }

    #[test]
    fn a_covered_drawable_only_copies_this_frames_damage() {
        let surface = (100, 80);
        let mut state = DrawableSyncState {
            covered: true,
            pending: DamageRects::new(),
            stale: false,
        };
        let current = damage(&[(10, 10, 20, 20)]);
        assert_eq!(
            state.take_copy(FrameDamage::Partial(current), surface),
            DrawableCopy::Rects(current)
        );
    }

    #[test]
    fn a_rotating_drawable_catches_up_every_damage_it_missed() {
        let surface = (100, 80);
        let mut states = [DrawableSyncState::new(); 3];
        assert_eq!(
            present(&mut states, 0, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        let second = damage(&[(0, 0, 10, 10)]);
        assert_eq!(
            present(&mut states, 1, FrameDamage::Partial(second), surface),
            DrawableCopy::Full
        );
        let third = damage(&[(10, 0, 20, 10)]);
        assert_eq!(
            present(&mut states, 2, FrameDamage::Partial(third), surface),
            DrawableCopy::Full
        );
        let fourth = damage(&[(20, 0, 30, 10)]);
        let copy = present(&mut states, 0, FrameDamage::Partial(fourth), surface);
        assert_eq!(
            copy,
            DrawableCopy::Rects(damage(&[(0, 0, 10, 10), (10, 0, 20, 10), (20, 0, 30, 10),]))
        );
        let fifth = damage(&[(30, 0, 40, 10)]);
        assert_eq!(
            present(&mut states, 0, FrameDamage::Partial(fifth), surface),
            DrawableCopy::Rects(fifth)
        );
    }

    #[test]
    fn a_full_frame_forces_every_other_drawable_to_copy_the_whole_surface() {
        let surface = (100, 80);
        let mut states = [DrawableSyncState::new(); 3];
        assert_eq!(
            present(&mut states, 0, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        let first = damage(&[(0, 0, 10, 10)]);
        assert_eq!(
            present(&mut states, 1, FrameDamage::Partial(first), surface),
            DrawableCopy::Full
        );
        let second = damage(&[(10, 0, 20, 10)]);
        assert_eq!(
            present(&mut states, 0, FrameDamage::Partial(second), surface),
            DrawableCopy::Rects(damage(&[(0, 0, 10, 10), (10, 0, 20, 10)]))
        );
        assert_eq!(
            present(&mut states, 2, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        for current in [0, 1] {
            assert_eq!(
                present(
                    &mut states,
                    current,
                    FrameDamage::Partial(damage(&[(5, 5, 6, 6)])),
                    surface
                ),
                DrawableCopy::Full
            );
        }
    }

    #[test]
    fn accumulated_damage_covering_the_whole_surface_falls_back_to_a_full_copy() {
        let surface = (100, 80);
        let mut state = DrawableSyncState::new();
        assert_eq!(
            state.take_copy(FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        state.record_missed(FrameDamage::Partial(damage(&[(0, 0, 100, 48)])));
        assert_eq!(
            state.take_copy(FrameDamage::Partial(damage(&[(0, 32, 100, 80)])), surface),
            DrawableCopy::Full
        );
    }

    #[test]
    fn missed_damage_stays_within_the_fixed_rect_budget() {
        let surface = (1000, 1000);
        let mut state = DrawableSyncState::new();
        assert_eq!(
            state.take_copy(FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        for frame in 0..MAX_DAMAGE_RECTS {
            let mut frame_damage = DamageRects::new();
            for pass in 0..MAX_DRAW_PASSES {
                let x = ((frame * MAX_DRAW_PASSES + pass) * 2) as i32;
                assert!(frame_damage.push(DeviceRect::new(dp(x), dp(0), dp(x + 1), dp(1))));
            }
            state.record_missed(FrameDamage::Partial(frame_damage));
            assert!(
                state.pending.as_slice().len() <= MAX_DAMAGE_RECTS,
                "累计集不能超过固定容量"
            );
        }
        match state.take_copy(
            FrameDamage::Partial(damage(&[(900, 900, 901, 901)])),
            surface,
        ) {
            DrawableCopy::Rects(rects) => {
                assert!(rects.as_slice().len() <= MAX_DAMAGE_RECTS, "补拷矩形数有界");
            }
            other => panic!("expected a bounded rect copy, got {other:?}"),
        }
    }

    #[test]
    fn the_sync_table_keys_on_the_drawable_id() {
        let surface = (100, 80);
        let mut syncs = DrawableSyncs::new();
        assert_eq!(
            syncs.plan(7, surface, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        let second = damage(&[(0, 0, 10, 10)]);
        assert_eq!(
            syncs.plan(7, surface, FrameDamage::Partial(second), surface),
            DrawableCopy::Rects(second),
            "同一张 drawable 已经整面写过：只补本帧 damage"
        );
        assert_eq!(
            syncs.plan(
                8,
                surface,
                FrameDamage::Partial(damage(&[(0, 0, 10, 10)])),
                surface
            ),
            DrawableCopy::Full,
            "不同的 drawable 已绘制区域为空：必须整帧拷"
        );
    }

    #[test]
    fn the_sync_table_evicts_the_least_recently_used_drawable() {
        let surface = (100, 80);
        let mut syncs = DrawableSyncs::new();
        for id in 0..DRAWABLE_SYNC_SLOTS {
            assert_eq!(
                syncs.plan(id, surface, FrameDamage::Full, surface),
                DrawableCopy::Full
            );
        }
        assert_eq!(syncs.entries.len(), DRAWABLE_SYNC_SLOTS);
        assert_eq!(
            syncs.plan(DRAWABLE_SYNC_SLOTS, surface, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        assert_eq!(syncs.entries.len(), DRAWABLE_SYNC_SLOTS, "表容量固定");
        assert!(
            !syncs.entries.iter().any(|entry| entry.id == 0),
            "最久未用的条目被淘汰"
        );
        assert_eq!(
            syncs.plan(
                0,
                surface,
                FrameDamage::Partial(damage(&[(1, 1, 2, 2)])),
                surface
            ),
            DrawableCopy::Full,
            "淘汰后重现的 drawable 必须整帧拷"
        );
    }

    #[test]
    fn a_drawable_size_mismatch_invalidates_the_table() {
        let surface = (100, 80);
        let mut syncs = DrawableSyncs::new();
        assert_eq!(
            syncs.plan(1, surface, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        assert_eq!(
            syncs.plan(2, surface, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        assert_eq!(
            syncs.plan(
                1,
                (90, 70),
                FrameDamage::Partial(damage(&[(0, 0, 1, 1)])),
                surface
            ),
            DrawableCopy::Full
        );
        assert!(syncs.entries.is_empty(), "尺寸不符时整表作废");
        assert_eq!(
            syncs.plan(
                1,
                surface,
                FrameDamage::Partial(damage(&[(0, 0, 1, 1)])),
                surface
            ),
            DrawableCopy::Full,
            "作废之后旧状态不能复活"
        );
    }

    #[test]
    fn merging_into_a_full_set_keeps_every_rect_covered() {
        let mut pending = DamageRects::new();
        let mut originals = Vec::with_capacity(MAX_DAMAGE_RECTS + 2);
        for index in 0..MAX_DAMAGE_RECTS {
            let x = index as i32 * 2;
            let rect = DeviceRect::new(dp(x), dp(0), dp(x + 1), dp(1));
            assert!(pending.push(rect));
            originals.push(rect);
        }
        let extra = damage(&[(100, 100, 101, 101), (102, 100, 103, 101)]);
        originals.extend(extra.as_slice());
        merge_damage(&mut pending, &extra);
        assert!(pending.as_slice().len() <= MAX_DAMAGE_RECTS);
        for rect in originals {
            assert!(
                pending.as_slice().iter().any(|union| {
                    union.x0 <= rect.x0
                        && union.y0 <= rect.y0
                        && union.x1 >= rect.x1
                        && union.y1 >= rect.y1
                }),
                "{rect:?} 必须被某个累计矩形覆盖"
            );
        }
    }
}
