use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

use persist::rtv_handle;
use windows::Win32::{
    Foundation::{HANDLE, HWND},
    Graphics::{Direct3D12::*, Dwm::*, Dxgi::DXGI_FRAME_STATISTICS},
    UI::WindowsAndMessaging::{IsIconic, IsWindowVisible},
};

#[cfg(any(test, feature = "test-support"))]
use crate::paint::instance::InstanceBuffer;
#[cfg(any(test, feature = "test-support"))]
use crate::render::compositor::Plan;
#[cfg(any(test, feature = "test-support"))]
use crate::render::windows::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    platform::windows::composition::{self, Composition, CompositionState, buffer_size},
    render::{
        RenderStats,
        compositor::{DamageRects, FramePlan},
        frame::{FramePayload, FrameRequest, FrameSlot, InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample},
        windows::{
            atlas::{AtlasUploads, SharedAtlas},
            device::{DescriptorLease, SharedGpu, init_error_from},
            pipeline::{GeometryRegistry, Pipeline},
            queue::{CommandStream, WaitOutcome},
            swapchain::Swapchain,
            timer::GpuTimer,
        },
    },
    window::FrameIndex,
};

mod persist;
mod plan;
mod record;
mod timings;
mod upload;

pub(crate) use self::upload::GeometryUploadArena;

#[cfg(test)]
mod tests;

use plan::BufferSyncs;
pub(crate) use plan::TargetState;
use record::OpBuckets;
pub(crate) use timings::FrameSubmitError;

#[cfg(test)]
const TEST_ATLAS_PAGE: u32 = 64;

thread_local! {
    static ON_GPU_THREAD: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn enter_gpu_thread() {
    ON_GPU_THREAD.set(true);
}

fn on_gpu_thread() -> bool {
    ON_GPU_THREAD.get()
}

pub(crate) struct GpuRenderer {
    shared: Arc<SharedGpu>,
    stream: CommandStream,
    descriptors: DescriptorLease,
    atlas_uploads: AtlasUploads,
    persist: Persist,
    upload: UploadArena,
    geometry_upload: GeometryUploadArena,
    mirror: InstanceMirror,
    geometry: GeometryRegistry,
    timer: Option<GpuTimer>,
    swapchain: Option<Swapchain>,
    background_swapchain: Option<Swapchain>,
    background_dirty: bool,
    composition: Option<Arc<Mutex<Composition>>>,
    rtv_heap: ID3D12DescriptorHeap,
    clear_color: [f32; 4],
    buffer: (u32, u32),
    surface: (u32, u32),
    waitable_primed: bool,
    target: TargetState,
    buffer_syncs: BufferSyncs,
    op_buckets: OpBuckets,
    last_sequence: u64,
    scale: f32,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

unsafe impl Send for GpuRenderer {}

impl GpuRenderer {
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_shared(SharedGpu::process()?, size)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_shared(SharedGpu::process_warp()?, size)
    }

    pub(crate) fn new_for_composition(
        composition: Arc<Mutex<Composition>>,
        content: (u32, u32),
        background: [f32; 4],
    ) -> Result<Self, EngineError> {
        Self::build_window(SharedGpu::process()?, composition, content, background)
    }

    fn build_window(
        shared: Arc<SharedGpu>,
        composition: Arc<Mutex<Composition>>,
        content: (u32, u32),
        background: [f32; 4],
    ) -> Result<Self, EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        let buffer = buffer_size(content)?;
        let mut renderer = Self::with_shared(shared, buffer)?;
        renderer.clear_color = background;
        let mut swapchain =
            Swapchain::for_composition(renderer.shared.raw(), renderer.shared.queue(), buffer)?;
        let mut background_swapchain =
            Swapchain::for_composition(renderer.shared.raw(), renderer.shared.queue(), (1, 1))?;
        renderer.prime_detached(&mut background_swapchain)?;
        renderer.prime_detached(&mut swapchain)?;
        {
            let target = composition::lock(&composition);
            target.set_background_content(background_swapchain.raw())?;
            target.set_content(swapchain.raw())?;
            target.commit()?;
        }
        renderer.swapchain = Some(swapchain);
        renderer.background_swapchain = Some(background_swapchain);
        renderer.surface = content;
        renderer.composition = Some(composition);
        renderer.target = TargetState::PersistInvalid;
        Ok(renderer)
    }

    pub(crate) fn recover(self) -> Result<Self, EngineError> {
        let Some(composition) = self.composition.clone() else {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "device recovery requires a composition target",
                ),
            });
        };
        let content = self.surface;
        let background = self.clear_color;
        let stale = Arc::clone(&self.shared);
        drop(self);
        composition::lock(&composition).rebuild()?;
        let shared = SharedGpu::recover(&stale)?;
        Self::build_window(shared, composition, content, background)
    }

    pub(crate) fn composition_state(&self) -> CompositionState {
        match self.composition.as_ref() {
            Some(composition) => composition::lock(composition).check_device_state(),
            None => CompositionState::WindowClosed,
        }
    }

    pub(crate) fn rebuild_composition(&mut self) -> Result<(), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "the DirectComposition tree may only be rebuilt on the kytrix-d3d12 thread"
        );
        let Some(composition) = self.composition.clone() else {
            return Ok(());
        };
        let Some(content) = self.swapchain.as_ref() else {
            return Ok(());
        };
        let mut target = composition::lock(&composition);
        target.rebuild()?;
        target.set_content(content.raw())?;
        if let Some(background) = self.background_swapchain.as_ref() {
            target.set_background_content(background.raw())?;
        }
        let client = target.client_size();
        target.set_geometry(self.surface, client)?;
        target.commit()?;
        Ok(())
    }

    fn with_shared(shared: Arc<SharedGpu>, size: (u32, u32)) -> Result<Self, EngineError> {
        let stream = CommandStream::new(&shared)?;
        let descriptors = shared.window_descriptors()?;
        let rtv_heap: ID3D12DescriptorHeap = unsafe {
            shared
                .raw()
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(RTV)", error))?;
        let rtv = rtv_handle(shared.raw(), &rtv_heap, 0);
        let size = (size.0.max(1), size.1.max(1));
        let persist = Persist::new(&shared, size, rtv)?;
        let upload = UploadArena::new(&shared)?;
        let geometry_upload = GeometryUploadArena::new(&shared)?;
        let mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        let timer = GpuTimer::new(&shared, &stream)?;

        Ok(Self {
            shared,
            stream,
            descriptors,
            atlas_uploads: AtlasUploads::new(),
            persist,
            upload,
            geometry_upload,
            mirror,
            geometry: GeometryRegistry::new(),
            timer,
            swapchain: None,
            background_swapchain: None,
            background_dirty: false,
            composition: None,
            rtv_heap,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            buffer: size,
            surface: size,
            waitable_primed: false,
            target: TargetState::Fresh,
            buffer_syncs: BufferSyncs::new(),
            op_buckets: OpBuckets::new(),
            last_sequence: 0,
            scale: 1.0,
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
    }

    pub(crate) fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    #[cfg(test)]
    pub(crate) fn buffer(&self) -> (u32, u32) {
        self.buffer
    }

    #[cfg(test)]
    pub(crate) fn target_state(&self) -> TargetState {
        self.target
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.target.is_fresh()
    }

    pub(crate) fn waitable(&self) -> Option<HANDLE> {
        debug_assert!(
            on_gpu_thread(),
            "the frame-latency waitable may only be waited on the kytrix-d3d12 thread"
        );
        self.swapchain.as_ref().map(Swapchain::waitable)
    }

    pub(crate) fn take_first_present_skip(&mut self) -> bool {
        if self.swapchain.is_some() && !self.waitable_primed {
            self.waitable_primed = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn acquire_slot(&mut self) -> FrameSlot {
        debug_assert!(
            on_gpu_thread(),
            "window frame slots may only be acquired on the kytrix-d3d12 thread"
        );
        self.stream.acquire_slot()
    }

    pub(crate) fn wait_slot_fence(&mut self, slot: FrameSlot) -> Result<WaitOutcome, EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "window slot fences may only be waited on the kytrix-d3d12 thread"
        );
        self.stream
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))
    }

    pub(crate) fn set_completion_event(
        &mut self,
        signal_value: u64,
        event: HANDLE,
    ) -> Result<(), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "completion events may only be armed on the kytrix-d3d12 thread"
        );
        self.stream
            .set_completion_event(signal_value, event)
            .map_err(|error| self.classify(error))
    }

    pub(crate) fn completed_value(&self) -> u64 {
        debug_assert!(
            on_gpu_thread(),
            "the queue fence may only be observed on the kytrix-d3d12 thread"
        );
        self.stream.completed_value()
    }

    pub(crate) fn frame_statistics(&self) -> Option<DXGI_FRAME_STATISTICS> {
        debug_assert!(
            on_gpu_thread(),
            "frame statistics may only be observed on the kytrix-d3d12 thread"
        );
        self.swapchain.as_ref()?.frame_statistics()
    }

    pub(crate) fn set_content_size(&mut self, content: (u32, u32)) -> Result<(), EngineError> {
        let previous_buffer = self.buffer;
        persist::set_content_size(self, content)?;
        if self.buffer != previous_buffer {
            self.waitable_primed = false;
        }
        Ok(())
    }

    pub(crate) fn resize_target(&mut self) -> Result<(), EngineError> {
        persist::resize_target(self, self.surface)?;
        self.waitable_primed = false;
        Ok(())
    }

    pub(crate) fn rebuild_target(&mut self) -> Result<bool, EngineError> {
        let rebuilt = persist::rebuild_target(self)?;
        if rebuilt {
            self.waitable_primed = false;
        }
        Ok(rebuilt)
    }

    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        persist::resize_offscreen(self, size)
    }

    pub(crate) fn set_background_color(&mut self, rgba: [f32; 4]) {
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "window background changes may only run on the kytrix-d3d12 thread"
        );
        self.clear_color = rgba;
        self.background_dirty = true;
    }

    pub(crate) fn refresh_background(&mut self) -> Result<(), EngineError> {
        persist::refresh_background(self)
    }

    fn prime_detached(&mut self, swapchain: &mut Swapchain) -> Result<(), EngineError> {
        record::prime_detached(self, swapchain)
    }

    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        let slot = self.stream.acquire_slot();
        if !self
            .stream
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))?
            .is_signaled()
        {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "synchronous submit needs an idle frame slot",
                ),
            });
        }
        let (recorded, slot) = self.submit_recorded(slot, index, payload, frame, timings)?;
        self.finish_sync(slot, timings)?;
        Ok(recorded.stats)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn submit_buffer(
        &mut self,
        index: FrameIndex,
        buffer: &InstanceBuffer,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        let payload = FramePayload::full(buffer, Vec::new(), Vec::new());
        self.submit_snapshot(index, &payload, frame, timings)
    }

    fn finish_sync(
        &mut self,
        slot: FrameSlot,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        timings::finish_sync(self, slot, timings)
    }

    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
        slot: FrameSlot,
        replace: bool,
    ) -> Result<(RenderStats, Vec<GpuTimerSample>), timings::FrameSubmitError> {
        timings::submit_frame(self, request, frame, slot, replace)
    }

    pub(crate) fn resolve_frame_timer(&self, slot: usize) -> Option<GpuTimerSample> {
        debug_assert!(
            on_gpu_thread(),
            "GPU timestamps may only be resolved on the kytrix-d3d12 thread"
        );
        self.timer.as_ref()?.resolve(slot)
    }

    pub(crate) fn release_completed(&mut self) {
        debug_assert!(
            on_gpu_thread(),
            "the retire queue may only be advanced on the kytrix-d3d12 thread"
        );
        self.stream.release_completed();
    }

    fn submit_recorded(
        &mut self,
        slot: FrameSlot,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<(RecordedFrame, FrameSlot), EngineError> {
        record::submit_recorded(self, slot, index, payload, frame, timings)
    }

    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: Vec<u8>,
        rects: crate::render::atlas::DirtyRects,
    ) {
        upload::queue_atlas_upload(self, kind, layer, spec, pixels, rects)
    }

    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas_uploads.pending(kind)
    }

    #[cfg(test)]
    pub(crate) fn atlas_pending_layers(&self, kind: AtlasKind) -> Vec<u16> {
        self.atlas_uploads.pending_layers(kind)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        if !self.stream.wait_idle()?.is_signaled() {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static("readback needs an idle command stream"),
            });
        }
        self.readback.capture(
            &self.shared,
            &mut self.stream,
            &self.persist.texture,
            self.surface,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)?;
        self.readback()
    }

    fn classify(&mut self, error: EngineError) -> EngineError {
        self.shared.note_failure().unwrap_or(error)
    }

    pub(crate) fn check_device_lost(&self) -> Option<EngineError> {
        self.shared.note_failure()
    }

    pub(crate) fn window_occluded(&self) -> bool {
        let Some(composition) = self.composition.as_ref() else {
            return false;
        };
        let hwnd = composition::lock(composition).hwnd_raw();
        if hwnd == 0 {
            return false;
        }
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        unsafe {
            let iconic = IsIconic(hwnd).as_bool();
            let visible = IsWindowVisible(hwnd).as_bool();
            let mut cloaked = 0u32;
            let cloaked_ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                std::ptr::from_mut(&mut cloaked).cast(),
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok();
            occluded_from(iconic, visible, cloaked_ok && cloaked != 0)
        }
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        let lost = self.shared.lost_error().is_some();
        if !lost {
            let idle = self.stream.wait_idle().is_ok_and(WaitOutcome::is_signaled);
            if !idle {
                eprintln!(
                    "kytrix: d3d12 window teardown timed out while the device is still present; keeping its GPU resources registered"
                );
                return;
            }
            let _ = self.stream.reset_in_flight_lists();
        }
        let _ = self.stream.release_completed();
        self.stream.drain_retire();
        self.upload.release(&self.shared);
        self.geometry_upload.release(&self.shared);
        self.geometry.release(&self.shared);
        self.atlas_uploads.release(&self.shared);
        let _ = self.shared.release(self.persist.handle);
    }
}

struct RecordedFrame {
    stats: RenderStats,
    present: Option<DamageRects>,
}

struct FrameRecorder<'a> {
    shared: &'a SharedGpu,
    stream: &'a mut CommandStream,
    pipeline: &'a Pipeline,
    descriptors: &'a DescriptorLease,
    atlas: &'a SharedAtlas,
    atlas_uploads: &'a mut AtlasUploads,
    persist: &'a Persist,
    upload: &'a mut UploadArena,
    geometry_upload: &'a mut GeometryUploadArena,
    mirror: &'a mut InstanceMirror,
    geometry: &'a mut GeometryRegistry,
    timer: Option<&'a GpuTimer>,
    swapchain: Option<&'a Swapchain>,
    clear_color: [f32; 4],
    content: (u32, u32),
    scale: f32,
    target: &'a mut TargetState,
    syncs: &'a mut BufferSyncs,
    buckets: &'a mut OpBuckets,
    prime: bool,
}

impl FrameRecorder<'_> {
    fn record(
        &mut self,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
        frame_started: std::time::Instant,
    ) -> Result<RecordedFrame, EngineError> {
        record::record(self, slot, payload, frame, timings, frame_started)
    }
}

struct Persist {
    texture: ID3D12Resource,
    handle: GpuResourceHandle,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    size: (u32, u32),
}

impl Persist {
    fn new(
        shared: &SharedGpu,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<Self, EngineError> {
        persist::create_persist(shared, size, rtv)
    }

    fn recreate(
        &mut self,
        shared: &SharedGpu,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<GpuResourceHandle, EngineError> {
        persist::recreate_persist(self, shared, size, rtv)
    }
}

struct UploadArena {
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    instances: ID3D12Resource,
    instance_handle: GpuResourceHandle,
    instance_capacity: u32,
    instance_ptr: *mut u8,
    clips: ID3D12Resource,
    clip_handle: GpuResourceHandle,
    clip_capacity: u32,
    clip_ptr: *mut u8,
}

unsafe impl Send for UploadSlot {}

impl UploadArena {
    fn new(shared: &SharedGpu) -> Result<Self, EngineError> {
        upload::create_arena(shared)
    }

    fn slot(&self, slot: usize) -> &UploadSlot {
        &self.slots[slot]
    }

    fn release(&self, shared: &SharedGpu) {
        for slot in &self.slots {
            let _ = shared.release(slot.instance_handle);
            let _ = shared.release(slot.clip_handle);
        }
    }

    fn write(
        &mut self,
        mirror: &mut InstanceMirror,
        shared: &SharedGpu,
        stream: &mut CommandStream,
        sequence: u64,
        slot: usize,
        payload: &FramePayload,
    ) -> Result<(u32, u32), EngineError> {
        upload::write_uploads(self, mirror, shared, stream, sequence, slot, payload)
    }
}

impl UploadSlot {
    fn instances(&self) -> &ID3D12Resource {
        &self.instances
    }

    fn clips(&self) -> &ID3D12Resource {
        &self.clips
    }
}

fn occluded_from(iconic: bool, visible: bool, cloaked: bool) -> bool {
    iconic || !visible || cloaked
}

#[cfg(test)]
mod occlusion_tests {
    use super::occluded_from;

    #[test]
    fn any_occlusion_signal_stops_frame_production() {
        assert!(!occluded_from(false, true, false));
        assert!(occluded_from(true, true, false));
        assert!(occluded_from(false, true, true));
        assert!(occluded_from(false, false, false));
        assert!(occluded_from(true, false, true));
        assert!(!occluded_from(false, true, false));
    }
}
