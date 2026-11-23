use std::{
    collections::VecDeque,
    ffi::c_void,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        Weak,
        atomic::{AtomicU8, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use smol_str::SmolStr;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Graphics::{
            Direct3D::{
                D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
                D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_2,
            },
            Direct3D12::*,
            Dxgi::{
                Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
                *,
            },
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::{HRESULT, Interface},
};

use super::{
    atlas::{SharedAtlas, row_pitch_for},
    debug,
    hresult::{is_device_removal, removal_reason_label},
    queue::{FENCE_TIMEOUT_MS, StreamWatermark},
};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        AtlasArraySpec, AtlasPageRequest,
        atlas::{AtlasRebuilt, texel_bytes},
        retire::GpuResourceHandle,
        windows::pipeline::{ATLAS_GENERATIONS, DescriptorArena, MAX_WINDOWS, Pipeline, SrvHeap},
    },
};

pub(crate) const D8_REQUIRED: &str = "FL 12_0 / SM 6.2 / ResourceBindingTier >= 2 / CastingFullyTypedFormat / FLIP_SEQUENTIAL / BufferCount >= 2";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    pub adapter: SmolStr,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram_mib: u64,
    pub feature_level: i32,
    pub shader_model: i32,
    pub resource_binding_tier: i32,
    pub resource_heap_tier: i32,
    pub tiled_resources_tier: i32,
    pub min_precision_support: i32,
    pub native_16bit_shader_ops: bool,
    pub root_signature_max: i32,
    pub max_sampler_descriptor_heap: u32,
    pub casting_fully_typed: bool,
    pub debug_layer: bool,
    pub warp: bool,
}

impl GpuCaps {
    pub(crate) fn baseline_ok(&self) -> bool {
        self.feature_level >= FEATURE_LEVEL_12_0
            && self.shader_model >= SHADER_MODEL_6_2
            && self.resource_binding_tier >= 2
            && self.casting_fully_typed
    }

    pub(crate) fn actual(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "FL {} / SM {} / BindingTier {} / CastSrgb {}",
            fl_name(self.feature_level),
            sm_name(self.shader_model),
            self.resource_binding_tier,
            self.casting_fully_typed
        )
    }

    pub(crate) fn describe(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "adapter={:?} vendor=0x{:04X} device=0x{:04X} vram={}MiB FL={} SM={} binding={} heap={} tiled={} min_precision={} native16={} root_sig={} max_sampler_heap={} cast_srgb={} debug_layer={} warp={}",
            self.adapter,
            self.vendor_id,
            self.device_id,
            self.dedicated_vram_mib,
            fl_name(self.feature_level),
            sm_name(self.shader_model),
            self.resource_binding_tier,
            self.resource_heap_tier,
            self.tiled_resources_tier,
            self.min_precision_support,
            self.native_16bit_shader_ops,
            self.root_signature_max,
            self.max_sampler_descriptor_heap,
            self.casting_fully_typed,
            self.debug_layer,
            self.warp
        )
    }

    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.baseline_ok() {
            Ok(())
        } else {
            Err(EngineError::UnsupportedGpu {
                os: "windows",
                required: D8_REQUIRED,
                actual: self.actual(),
            })
        }
    }
}

pub(crate) const FEATURE_LEVEL_12_0: i32 = 49_152;
pub(crate) const SHADER_MODEL_6_2: i32 = 98;

pub(crate) fn fl_name(level: i32) -> &'static str {
    match level {
        45_056 => "11_0",
        45_312 => "11_1",
        49_152 => "12_0",
        49_408 => "12_1",
        49_664 => "12_2",
        _ => "unknown",
    }
}

pub(crate) fn sm_name(shader_model: i32) -> &'static str {
    match shader_model {
        96 => "6_0",
        97 => "6_1",
        98 => "6_2",
        99 => "6_3",
        100 => "6_4",
        101 => "6_5",
        102 => "6_6",
        103 => "6_7",
        104 => "6_8",
        105 => "6_9",
        _ => "unknown",
    }
}

const HANDLE_KIND_OBJECT: u8 = 1;

pub(crate) fn init_error(context: &str, hr: HRESULT) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!(
            "d3d12 {context}: {:#010X} {}",
            hr.0 as u32,
            hr.message()
        ),
    }
}

pub(crate) fn init_error_from(context: &str, error: windows::core::Error) -> EngineError {
    init_error(context, error.code())
}

pub(crate) struct Device {
    raw: ID3D12Device,
    resources: ResourceTable,
    lost: Option<LostInfo>,
    caps: GpuCaps,
}

struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

impl Device {
    pub(crate) fn new() -> Result<Self, EngineError> {
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .map_err(|error| init_error_from("CreateDXGIFactory2", error))?;
        let adapter = pick_hardware_adapter(&factory)?;
        Self::from_adapter(&adapter, false)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp() -> Result<Self, EngineError> {
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .map_err(|error| init_error_from("CreateDXGIFactory2", error))?;
        let adapter: IDXGIAdapter = unsafe { factory.EnumWarpAdapter() }
            .map_err(|error| init_error_from("EnumWarpAdapter", error))?;
        let adapter: IDXGIAdapter1 = adapter
            .cast()
            .map_err(|error| init_error_from("cast IDXGIAdapter1", error))?;
        Self::from_adapter(&adapter, true)
    }

    fn from_adapter(adapter: &IDXGIAdapter1, warp: bool) -> Result<Self, EngineError> {
        debug::enable_debug_layer();
        enable_dred();
        let mut device: Option<ID3D12Device> = None;
        unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, &mut device) }
            .map_err(|error| init_error_from("D3D12CreateDevice", error))?;
        let raw = device.ok_or_else(|| EngineError::AppInit {
            detail: SmolStr::new_static("d3d12 D3D12CreateDevice returned no device"),
        })?;
        debug::configure_info_queue(&raw);

        let caps = unsafe { collect_caps(&raw, adapter, warp) };
        caps.check()?;
        debug_assert!(
            !caps.describe().is_empty(),
            "gpu description must not be empty"
        );

        Ok(Self {
            raw,
            resources: ResourceTable::new(),
            lost: None,
            caps,
        })
    }

    pub(crate) fn raw(&self) -> &ID3D12Device {
        &self.raw
    }

    pub(crate) fn caps(&self) -> &GpuCaps {
        &self.caps
    }

    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.resources
            .slots
            .iter()
            .filter(|slot| slot.is_some())
            .count()
    }

    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    pub(crate) fn allocate(&mut self, resource: ID3D12Resource) -> GpuResourceHandle {
        self.resources.insert(resource)
    }

    pub(crate) fn release(&mut self, handle: GpuResourceHandle) -> bool {
        self.resources.release(handle)
    }

    pub(crate) fn note_failure(&mut self) -> Option<EngineError> {
        if self.lost.is_some() {
            return self.lost_error();
        }
        let reason = match unsafe { self.raw.GetDeviceRemovedReason() } {
            Ok(()) => return None,
            Err(error) => error.code(),
        };
        if !is_device_removal(reason) {
            return None;
        }
        let log_path = self.write_dred_log();
        let lost = LostInfo {
            reason: smol_str::format_smolstr!(
                "{:#010X} {} ({})",
                reason.0 as u32,
                reason.message(),
                removal_reason_label(reason)
            ),
            log_path,
        };
        let error = EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        };
        self.lost = Some(lost);
        Some(error)
    }

    fn write_dred_log(&self) -> PathBuf {
        let path = self.dred_dir();
        let mut body = String::from("kytrix d3d12 device removed diagnostics\n");
        if let Err(error) = unsafe { self.raw.GetDeviceRemovedReason() } {
            let _ = std::fmt::write(
                &mut body,
                format_args!(
                    "GetDeviceRemovedReason: {:#010X} {} ({})\n",
                    error.code().0 as u32,
                    error.code().message(),
                    removal_reason_label(error.code())
                ),
            );
        }
        let messages = debug::drain_messages(&self.raw);
        let _ = std::fmt::write(
            &mut body,
            format_args!(
                "debug layer messages: warnings={} errors={} corruption={}\n",
                messages.warning, messages.error, messages.corruption
            ),
        );
        match self.raw.cast::<ID3D12DeviceRemovedExtendedData>() {
            Ok(dred) => {
                match unsafe { dred.GetAutoBreadcrumbsOutput() } {
                    Ok(output) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!(
                                "breadcrumbs: head_null={}\n",
                                output.pHeadAutoBreadcrumbNode.is_null()
                            ),
                        );
                    }
                    Err(error) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!("breadcrumbs: failed {:#010X}\n", error.code().0 as u32),
                        );
                    }
                }
                match unsafe { dred.GetPageFaultAllocationOutput() } {
                    Ok(output) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!("page_fault: va=0x{:X}\n", output.PageFaultVA),
                        );
                    }
                    Err(error) => {
                        let _ = std::fmt::write(
                            &mut body,
                            format_args!("page_fault: failed {:#010X}\n", error.code().0 as u32),
                        );
                    }
                }
            }
            Err(_) => body.push_str(
                "DRED interface unavailable (requires the Graphics Tools optional feature)\n",
            ),
        }
        let _ = std::fs::write(&path, body);
        path
    }

    fn dred_dir(&self) -> PathBuf {
        std::env::temp_dir()
            .join(smol_str::format_smolstr!("kytrix-dred-{}.txt", std::process::id()).as_str())
    }
}

pub(crate) struct SharedGpu {
    raw: ID3D12Device,
    device: Mutex<Device>,
    queue: ID3D12CommandQueue,
    pipeline: Pipeline,
    srv_heap: SrvHeap,
    srv_arena: Mutex<DescriptorArena>,
    atlas: RwLock<SharedAtlas>,
    atlas_pages: [AtomicU32; 2],
    watermarks: Mutex<Vec<Weak<StreamWatermark>>>,
    atlas_generation: AtomicU8,
    atlas_switch: Mutex<AtlasSwitch>,
}

struct AtlasSwitch {
    retired_textures: [Vec<ID3D12Resource>; ATLAS_GENERATIONS as usize],
}

impl AtlasSwitch {
    fn new() -> Self {
        Self {
            retired_textures: [Vec::new(), Vec::new()],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AtlasRebuildOutcome {
    Applied,
    Deferred,
}

unsafe impl Send for SharedGpu {}
unsafe impl Sync for SharedGpu {}

impl SharedGpu {
    pub(crate) fn process() -> Result<Arc<Self>, EngineError> {
        let mut cell = gpu_cell();
        if let Some(gpu) = cell.current.as_ref().and_then(Weak::upgrade)
            && gpu.lost_error().is_none()
        {
            return Ok(gpu);
        }
        let gpu = Arc::new(Self::create(Device::new()?)?);
        cell.current = Some(Arc::downgrade(&gpu));
        cell.generation += 1;
        Ok(gpu)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn process_warp() -> Result<Arc<Self>, EngineError> {
        static GPU: OnceLock<Arc<SharedGpu>> = OnceLock::new();
        static INIT: Mutex<()> = Mutex::new(());
        shared_instance(&GPU, &INIT, || Self::create(Device::new_warp()?))
    }

    #[cfg(test)]
    pub(crate) fn new_warp() -> Result<Arc<Self>, EngineError> {
        Ok(Arc::new(Self::create(Device::new_warp()?)?))
    }

    pub(crate) fn recover(stale: &Arc<Self>) -> Result<Arc<Self>, EngineError> {
        let mut cell = gpu_cell();
        if let Some(current) = cell.current.as_ref().and_then(Weak::upgrade)
            && !Arc::ptr_eq(&current, stale)
            && current.lost_error().is_none()
        {
            return Ok(current);
        }
        let now = Instant::now();
        cell.recoveries
            .retain(|at| now.duration_since(*at) < RECOVERY_WINDOW);
        if cell.recoveries.len() >= MAX_RECOVERIES_PER_WINDOW {
            return Err(EngineError::DeviceLost {
                reason: smol_str::format_smolstr!(
                    "device recovery limit reached ({} attempts within {:?})",
                    cell.recoveries.len(),
                    RECOVERY_WINDOW
                ),
                log_path: PathBuf::new(),
            });
        }
        cell.recoveries.push_back(now);
        let snapshot = stale.atlas().shadow_snapshot();
        let gpu = Arc::new(Self::create(Device::new()?)?);
        gpu.restore_atlas(&snapshot)?;
        cell.current = Some(Arc::downgrade(&gpu));
        cell.generation += 1;
        Ok(gpu)
    }

    fn create(device: Device) -> Result<Self, EngineError> {
        let queue: ID3D12CommandQueue = unsafe {
            device.raw().CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                Priority: 0,
                Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                NodeMask: 0,
            })
        }
        .map_err(|error| init_error_from("CreateCommandQueue", error))?;
        let pipeline = Pipeline::new(device.raw())?;
        let atlas = SharedAtlas::new(device.raw())?;
        let atlas_pages = [
            AtomicU32::new(atlas.page(AtlasKind::Glyph)),
            AtomicU32::new(atlas.page(AtlasKind::Image)),
        ];
        let srv_heap = SrvHeap::new(device.raw())?;
        for generation in 0..ATLAS_GENERATIONS {
            for kind in [AtlasKind::Glyph, AtlasKind::Image] {
                srv_heap.write_atlas(
                    device.raw(),
                    generation,
                    kind,
                    atlas.texture(kind),
                    atlas.layers(kind),
                );
            }
        }
        eprintln!("kytrix: d3d12 {}", device.caps().describe());
        let raw = device.raw().clone();
        Ok(Self {
            raw,
            device: Mutex::new(device),
            queue,
            pipeline,
            srv_heap,
            srv_arena: Mutex::new(DescriptorArena::new()),
            atlas: RwLock::new(atlas),
            atlas_pages,
            watermarks: Mutex::new(Vec::new()),
            atlas_generation: AtomicU8::new(0),
            atlas_switch: Mutex::new(AtlasSwitch::new()),
        })
    }

    pub(crate) fn raw(&self) -> &ID3D12Device {
        &self.raw
    }

    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    pub(crate) fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    pub(crate) fn srv_heap(&self) -> &SrvHeap {
        &self.srv_heap
    }

    pub(crate) fn window_descriptors(self: &Arc<Self>) -> Result<DescriptorLease, EngineError> {
        let base = self
            .srv_arena
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allocate()
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::format_smolstr!(
                    "srv descriptor heap exhausted (window segments: {})",
                    MAX_WINDOWS
                ),
            })?;
        Ok(DescriptorLease {
            gpu: Arc::clone(self),
            base,
        })
    }

    fn release_window_descriptors(&self, base: u32) {
        self.srv_arena
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .free(base);
    }

    pub(crate) fn atlas(&self) -> RwLockReadGuard<'_, SharedAtlas> {
        self.atlas.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn atlas_write(&self) -> RwLockWriteGuard<'_, SharedAtlas> {
        self.atlas.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn atlas_page(&self, kind: AtlasKind) -> u32 {
        self.atlas_pages[kind.index() as usize].load(Ordering::Acquire)
    }

    pub(crate) fn atlas_generation(&self) -> u8 {
        self.atlas_generation.load(Ordering::Acquire)
    }

    fn generation_idle(&self, generation: u8) -> bool {
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .all(|watermark| watermark.generation_idle(generation))
    }

    fn capture_generation_watermark(&self, generation: u8) {
        let watermarks = self
            .watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for watermark in watermarks.iter().filter_map(Weak::upgrade) {
            watermark.retire_generation(generation, watermark.value());
        }
    }

    pub(crate) fn register_watermark(&self, fence: ID3D12Fence) -> Arc<StreamWatermark> {
        let watermark = Arc::new(StreamWatermark::new(fence));
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&watermark));
        watermark
    }

    pub(crate) fn unregister_watermark(&self, watermark: &Arc<StreamWatermark>) {
        let mut watermarks = self
            .watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        watermarks.retain(|entry| {
            entry.upgrade().is_some_and(|registered| {
                !std::ptr::eq(Arc::as_ptr(&registered), Arc::as_ptr(watermark))
            })
        });
    }

    #[cfg(test)]
    pub(crate) fn watermark_values(&self) -> Vec<u64> {
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .map(|watermark| watermark.value())
            .collect()
    }

    pub(crate) fn allocate(&self, resource: ID3D12Resource) -> GpuResourceHandle {
        self.device_lock().allocate(resource)
    }

    pub(crate) fn release(&self, handle: GpuResourceHandle) -> bool {
        self.device_lock().release(handle)
    }

    pub(crate) fn note_failure(&self) -> Option<EngineError> {
        self.device_lock().note_failure()
    }

    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        self.device_lock().lost_error()
    }

    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.device_lock().live_resources()
    }

    #[cfg(test)]
    pub(crate) fn rebuild_atlas_pages(
        &self,
        requests: &[AtlasPageRequest],
    ) -> Result<AtlasRebuildOutcome, EngineError> {
        let mut atlas = self.atlas_write();
        self.rebuild_atlas_pages_locked(&mut atlas, requests)
    }

    pub(crate) fn rebuild_atlas_pages_locked(
        &self,
        atlas: &mut SharedAtlas,
        requests: &[AtlasPageRequest],
    ) -> Result<AtlasRebuildOutcome, EngineError> {
        let pending: Vec<AtlasPageRequest> = requests
            .iter()
            .filter(|request| atlas.spec(request.kind) != request.spec)
            .map(|request| {
                let current = atlas.spec(request.kind);
                AtlasPageRequest {
                    kind: request.kind,
                    spec: AtlasArraySpec {
                        page: request.spec.page,
                        layers: request.spec.layers.max(current.layers),
                    },
                }
            })
            .collect();
        if pending.is_empty() {
            return Ok(AtlasRebuildOutcome::Applied);
        }
        let active = self.atlas_generation.load(Ordering::Acquire);
        let target = (active + 1) % ATLAS_GENERATIONS;
        if !self.generation_idle(target) {
            super::diagnostics::atlas_deferred_rebuild();
            return Ok(AtlasRebuildOutcome::Deferred);
        }
        let mut switch = self
            .atlas_switch
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        switch.retired_textures[target as usize].clear();
        let mut rebuilt = AtlasRebuilt::default();
        for request in pending {
            let old = atlas.recreate(self.raw(), request.kind, request.spec)?;
            switch.retired_textures[active as usize].push(old);
            self.srv_heap.write_atlas(
                self.raw(),
                target,
                request.kind,
                atlas.texture(request.kind),
                atlas.layers(request.kind),
            );
            self.atlas_pages[request.kind.index() as usize]
                .store(request.spec.page, Ordering::Release);
            rebuilt.mark(request.kind);
        }
        debug_assert!(
            rebuilt.glyph || rebuilt.image,
            "a non-empty rebuild must recreate at least one atlas page"
        );
        self.capture_generation_watermark(active);
        self.atlas_generation.store(target, Ordering::Release);
        Ok(AtlasRebuildOutcome::Applied)
    }

    fn restore_atlas(
        &self,
        snapshot: &[Option<crate::render::windows::atlas::ShadowAtlasSnapshot>; 2],
    ) -> Result<(), EngineError> {
        let mut atlas = self.atlas.write().unwrap_or_else(PoisonError::into_inner);
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some(snapshot) = snapshot[index].as_ref() else {
                continue;
            };
            let page = snapshot.page();
            if page <= 1 {
                continue;
            }
            let layers =
                u32::try_from(snapshot.layers().len()).expect("atlas shadow layer count fits u32");
            if layers == 0 {
                continue;
            }
            let row_bytes = page as usize * texel_bytes(kind) as usize;
            if snapshot
                .layers()
                .iter()
                .flatten()
                .any(|pixels| pixels.len() != row_bytes * page as usize)
            {
                continue;
            }
            let spec = crate::render::AtlasArraySpec { page, layers };
            atlas.recreate(self.raw(), kind, spec)?;
            for generation in 0..ATLAS_GENERATIONS {
                self.srv_heap.write_atlas(
                    self.raw(),
                    generation,
                    kind,
                    atlas.texture(kind),
                    atlas.layers(kind),
                );
            }
            self.atlas_pages[index].store(page, Ordering::Release);
            let texture = atlas.texture(kind).clone();
            for (layer, pixels) in snapshot.layers().iter().enumerate() {
                let Some(pixels) = pixels else {
                    continue;
                };
                let layer = u16::try_from(layer).expect("atlas array layer fits u16");
                self.upload_atlas_page_now(&texture, kind, page, layer, pixels)?;
            }
        }
        self.atlas_switch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retired_textures = [Vec::new(), Vec::new()];
        self.atlas_generation.store(0, Ordering::Release);
        Ok(())
    }

    fn upload_atlas_page_now(
        &self,
        texture: &ID3D12Resource,
        kind: AtlasKind,
        page: u32,
        layer: u16,
        pixels: &[u8],
    ) -> Result<(), EngineError> {
        let row_bytes = page as usize * texel_bytes(kind) as usize;
        let row_pitch = row_pitch_for(kind, page);
        let staging = create_staging(self.raw(), u64::from(row_pitch) * u64::from(page))?;
        let mut pointer: *mut c_void = std::ptr::null_mut();
        unsafe { staging.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(atlas restore)", error))?;
        for row in 0..page as usize {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * row_bytes),
                    pointer.cast::<u8>().add(row * row_pitch as usize),
                    row_bytes,
                );
            }
        }
        unsafe { staging.Unmap(0, None) };

        let allocator: ID3D12CommandAllocator = unsafe {
            self.raw
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
        }
        .map_err(|error| init_error_from("CreateCommandAllocator(atlas restore)", error))?;
        let list: ID3D12GraphicsCommandList = unsafe {
            self.raw
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
        }
        .map_err(|error| init_error_from("CreateCommandList(atlas restore)", error))?;
        super::atlas::transition(
            &list,
            texture,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        super::atlas::copy_page(&list, texture, &staging, kind, page, layer, row_pitch);
        super::atlas::transition(
            &list,
            texture,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        unsafe { list.Close() }.map_err(|error| init_error_from("Close(atlas restore)", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        unsafe { self.queue.ExecuteCommandLists(&[Some(command_list)]) };
        let fence: ID3D12Fence = unsafe { self.raw.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| init_error_from("CreateFence(atlas restore)", error))?;
        unsafe { self.queue.Signal(&fence, 1) }
            .map_err(|error| init_error_from("Signal(atlas restore)", error))?;
        let event: HANDLE = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW(atlas restore)", error))?;
        let armed = unsafe { fence.SetEventOnCompletion(1, event) };
        if let Err(error) = armed {
            unsafe {
                let _ = CloseHandle(event);
            }
            return Err(init_error_from(
                "SetEventOnCompletion(atlas restore)",
                error,
            ));
        }
        let wait = unsafe { WaitForSingleObject(event, FENCE_TIMEOUT_MS) };
        unsafe {
            let _ = CloseHandle(event);
        }
        if wait != WAIT_OBJECT_0 {
            return match self.note_failure() {
                Some(sticky) => Err(sticky),
                None => Ok(()),
            };
        }
        Ok(())
    }

    fn device_lock(&self) -> MutexGuard<'_, Device> {
        self.device.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for SharedGpu {
    fn drop(&mut self) {
        let counts = debug::drain_messages(&self.raw);
        if counts.warning + counts.error + counts.corruption > 0 {
            eprintln!(
                "kytrix: d3d12 debug layer totals: warnings={} errors={} corruption={}",
                counts.warning, counts.error, counts.corruption
            );
        }
    }
}

pub(crate) struct DescriptorLease {
    gpu: Arc<SharedGpu>,
    base: u32,
}

impl DescriptorLease {
    pub(crate) fn base(&self) -> u32 {
        self.base
    }
}

impl Drop for DescriptorLease {
    fn drop(&mut self) {
        self.gpu.release_window_descriptors(self.base);
    }
}

struct GpuCell {
    current: Option<Weak<SharedGpu>>,
    recoveries: VecDeque<Instant>,
    generation: u64,
}

const RECOVERY_WINDOW: Duration = Duration::from_secs(5);
const MAX_RECOVERIES_PER_WINDOW: usize = 3;

fn gpu_cell() -> MutexGuard<'static, GpuCell> {
    static CELL: OnceLock<Mutex<GpuCell>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(GpuCell {
            current: None,
            recoveries: VecDeque::new(),
            generation: 0,
        })
    })
    .lock()
    .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(any(test, feature = "test-support"))]
fn shared_instance(
    cell: &'static OnceLock<Arc<SharedGpu>>,
    init: &'static Mutex<()>,
    create: impl FnOnce() -> Result<SharedGpu, EngineError>,
) -> Result<Arc<SharedGpu>, EngineError> {
    if let Some(gpu) = cell.get() {
        return Ok(Arc::clone(gpu));
    }
    let _guard = init.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(gpu) = cell.get() {
        return Ok(Arc::clone(gpu));
    }
    let gpu = Arc::new(create()?);
    let _ = cell.set(Arc::clone(&gpu));
    Ok(gpu)
}

fn pick_hardware_adapter(factory: &IDXGIFactory4) -> Result<IDXGIAdapter1, EngineError> {
    if let Ok(factory6) = factory.cast::<IDXGIFactory6>()
        && let Ok(adapter) = unsafe {
            factory6.EnumAdapterByGpuPreference::<IDXGIAdapter1>(
                0,
                DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
            )
        }
    {
        let desc =
            unsafe { adapter.GetDesc1() }.map_err(|error| init_error_from("GetDesc1", error))?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            return Ok(adapter);
        }
    }
    let mut index = 0u32;
    loop {
        match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => {
                let desc = unsafe { adapter.GetDesc1() }
                    .map_err(|error| init_error_from("GetDesc1", error))?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    return Ok(adapter);
                }
            }
            Err(_) => {
                return Err(EngineError::UnsupportedGpu {
                    os: "windows",
                    required: D8_REQUIRED,
                    actual: SmolStr::new_static("no hardware DXGI adapter"),
                });
            }
        }
        index += 1;
    }
}

fn enable_dred() {
    let mut settings: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
    if unsafe { D3D12GetDebugInterface(&mut settings) }.is_ok()
        && let Some(settings) = settings
    {
        unsafe {
            settings.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            settings.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
        }
    }
}

unsafe fn collect_caps(device: &ID3D12Device, adapter: &IDXGIAdapter1, warp: bool) -> GpuCaps {
    let desc = unsafe { adapter.GetDesc1() }.unwrap_or_default();
    let name_len = desc
        .Description
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(desc.Description.len());
    let adapter_name = SmolStr::new(String::from_utf16_lossy(&desc.Description[..name_len]));

    let levels = [
        D3D_FEATURE_LEVEL_12_2,
        D3D_FEATURE_LEVEL_12_1,
        D3D_FEATURE_LEVEL_12_0,
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    ];
    let mut feature_levels = D3D12_FEATURE_DATA_FEATURE_LEVELS {
        NumFeatureLevels: levels.len() as u32,
        pFeatureLevelsRequested: levels.as_ptr(),
        MaxSupportedFeatureLevel: D3D_FEATURE_LEVEL_11_0,
    };
    check_feature(
        device,
        D3D12_FEATURE_FEATURE_LEVELS,
        &mut feature_levels,
        std::mem::size_of::<D3D12_FEATURE_DATA_FEATURE_LEVELS>(),
    );

    let mut shader_model = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_8,
    };
    check_feature(
        device,
        D3D12_FEATURE_SHADER_MODEL,
        &mut shader_model,
        std::mem::size_of::<D3D12_FEATURE_DATA_SHADER_MODEL>(),
    );

    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS,
        &mut options,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>(),
    );

    let mut options3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS3,
        &mut options3,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS3>(),
    );

    let mut options4 = D3D12_FEATURE_DATA_D3D12_OPTIONS4::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS4,
        &mut options4,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS4>(),
    );

    let mut root_signature = D3D12_FEATURE_DATA_ROOT_SIGNATURE {
        HighestVersion: D3D_ROOT_SIGNATURE_VERSION_1_0,
    };
    check_feature(
        device,
        D3D12_FEATURE_ROOT_SIGNATURE,
        &mut root_signature,
        std::mem::size_of::<D3D12_FEATURE_DATA_ROOT_SIGNATURE>(),
    );

    let mut options19 = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
    check_feature(
        device,
        D3D12_FEATURE_D3D12_OPTIONS19,
        &mut options19,
        std::mem::size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS19>(),
    );

    GpuCaps {
        adapter: adapter_name,
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        dedicated_vram_mib: (desc.DedicatedVideoMemory / (1024 * 1024)) as u64,
        feature_level: feature_levels.MaxSupportedFeatureLevel.0,
        shader_model: shader_model.HighestShaderModel.0,
        resource_binding_tier: options.ResourceBindingTier.0,
        resource_heap_tier: options.ResourceHeapTier.0,
        tiled_resources_tier: options.TiledResourcesTier.0,
        min_precision_support: options.MinPrecisionSupport.0,
        native_16bit_shader_ops: options4.Native16BitShaderOpsSupported.as_bool(),
        root_signature_max: root_signature.HighestVersion.0,
        max_sampler_descriptor_heap: options19.MaxSamplerDescriptorHeapSize,
        casting_fully_typed: options3.CastingFullyTypedFormatSupported.as_bool(),
        debug_layer: debug::layer_available(),
        warp,
    }
}

fn check_feature<T>(device: &ID3D12Device, feature: D3D12_FEATURE, data: &mut T, size: usize) {
    unsafe {
        let _ = device.CheckFeatureSupport(
            feature,
            std::ptr::from_mut(data).cast::<c_void>(),
            size as u32,
        );
    }
}

fn create_staging(device: &ID3D12Device, bytes: u64) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(4),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..D3D12_RESOURCE_DESC::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(staging)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: SmolStr::new_static("d3d12 staging buffer missing"),
    })
}

struct ResourceTable {
    slots: Vec<Option<ID3D12Resource>>,
    generations: Vec<u32>,
    free: Vec<u32>,
}

impl ResourceTable {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            generations: Vec::new(),
            free: Vec::new(),
        }
    }

    fn insert(&mut self, resource: ID3D12Resource) -> GpuResourceHandle {
        if let Some(index) = self.free.pop() {
            let generation = self.generations[index as usize].wrapping_add(1);
            self.generations[index as usize] = generation;
            self.slots[index as usize] = Some(resource);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, generation, index)
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Some(resource));
            self.generations.push(0);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, 0, index)
        }
    }

    fn release(&mut self, handle: GpuResourceHandle) -> bool {
        if handle.kind() != HANDLE_KIND_OBJECT {
            debug_assert!(
                false,
                "unsupported GpuResourceHandle kind {}",
                handle.kind()
            );
            return false;
        }
        let index = handle.index() as usize;
        if self.generations.get(index).copied() != Some(handle.generation()) {
            return false;
        }
        if !matches!(self.slots.get(index), Some(Some(_))) {
            return false;
        }
        self.slots[index] = None;
        self.free.push(index as u32);
        true
    }
}

#[cfg(test)]
pub(crate) fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use windows::{
        Win32::Graphics::{
            Direct3D12::*,
            Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
        },
        core::Interface,
    };

    use super::{AtlasRebuildOutcome, Device, SharedGpu};
    use crate::{
        error::EngineError,
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, AtlasPageRequest},
    };

    fn glyph_request(page: u32) -> AtlasPageRequest {
        AtlasPageRequest {
            kind: AtlasKind::Glyph,
            spec: AtlasArraySpec { page, layers: 1 },
        }
    }

    fn debug_texture(device: &Device) -> Result<ID3D12Resource, EngineError> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..D3D12_RESOURCE_DESC::default()
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };
        let mut out: Option<ID3D12Resource> = None;
        unsafe {
            device.raw().CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut out,
            )
        }
        .map_err(|error| super::init_error_from("CreateCommittedResource", error))?;
        out.ok_or_else(|| EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("d3d12 CreateCommittedResource returned nothing"),
        })
    }

    #[test]
    fn warp_device_is_created_and_healthy() {
        let mut device = Device::new_warp().expect("WARP device meets D8");
        assert!(device.lost_error().is_none());
        assert!(device.note_failure().is_none());
        assert!(device.caps().describe().contains("warp=true"));
        assert!(
            device.caps().casting_fully_typed,
            "WARP 支持 relaxed format casting；不支持时 D8 必须显式失败"
        );
    }

    #[test]
    fn the_process_wide_gpu_is_created_once() {
        let _guard = super::gpu_test_lock();
        let first = SharedGpu::process_warp().expect("WARP shared gpu");
        let second = SharedGpu::process_warp().expect("WARP shared gpu");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(
            first.atlas().page(AtlasKind::Glyph),
            second.atlas().page(AtlasKind::Glyph)
        );
    }

    #[test]
    fn rebuilding_a_shared_atlas_page_swaps_the_descriptor_generation() {
        let _guard = super::gpu_test_lock();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 1, "占位页是 1×1");
        assert_eq!(shared.atlas_generation(), 0);
        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(64)])
                .expect("rebuild"),
            AtlasRebuildOutcome::Applied
        );
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
        assert_eq!(shared.atlas().row_pitch(AtlasKind::Glyph), 256);
        assert_eq!(
            shared.atlas_generation(),
            1,
            "换代只写目标代并把它翻成当前代"
        );

        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(64)])
                .expect("no-op rebuild"),
            AtlasRebuildOutcome::Applied
        );
        assert_eq!(shared.atlas_generation(), 1);
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);

        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(128)])
                .expect("rebuild"),
            AtlasRebuildOutcome::Applied
        );
        assert_eq!(shared.atlas_generation(), 0);
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 128);
    }

    #[test]
    fn a_generation_with_in_flight_frames_defers_the_page_rebuild() {
        let _guard = super::gpu_test_lock();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        let fence: ID3D12Fence =
            unsafe { shared.raw().CreateFence(0, D3D12_FENCE_FLAG_NONE) }.expect("fence");
        let watermark = shared.register_watermark(fence.clone());
        watermark.retire_generation(1, 1);
        assert!(!watermark.generation_idle(1), "fence 未完成 ⇒ 代 1 仍在飞");

        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(64)])
                .expect("deferred rebuild"),
            AtlasRebuildOutcome::Deferred
        );
        assert_eq!(shared.atlas_generation(), 0, "Deferred 不得翻转代际");
        assert_eq!(
            shared.atlas().page(AtlasKind::Glyph),
            1,
            "Deferred 不得换纹理（什么都没改）"
        );

        unsafe { fence.Signal(1) }.expect("signal fence");
        assert!(watermark.generation_idle(1));
        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(64)])
                .expect("rebuild"),
            AtlasRebuildOutcome::Applied
        );
        assert_eq!(shared.atlas_generation(), 1);
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
        shared.unregister_watermark(&watermark);
    }

    #[test]
    fn the_retired_watermark_is_captured_for_the_departing_generation() {
        let _guard = super::gpu_test_lock();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        let fence: ID3D12Fence =
            unsafe { shared.raw().CreateFence(0, D3D12_FENCE_FLAG_NONE) }.expect("fence");
        let watermark = shared.register_watermark(fence.clone());
        watermark.publish_for_test(7);
        assert_eq!(watermark.value(), 7);

        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(64)])
                .expect("first rebuild"),
            AtlasRebuildOutcome::Applied
        );
        assert_eq!(shared.atlas_generation(), 1, "第一次换代 0 → 1");
        assert!(
            !watermark.generation_idle(0),
            "capture 必须把弃用水位登记到离开的代（代 0），而不是目标代（代 1）"
        );
        assert!(
            watermark.generation_idle(1),
            "代 1 从未被该流使用，必须视为空闲"
        );

        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(128)])
                .expect("second rebuild"),
            AtlasRebuildOutcome::Deferred
        );
        assert_eq!(
            shared.atlas_generation(),
            1,
            "Deferred 不得切换回仍有在飞引用的代"
        );
        assert_eq!(
            shared.atlas().page(AtlasKind::Glyph),
            64,
            "Deferred 不得换纹理"
        );

        unsafe { fence.Signal(7) }.expect("signal fence");
        assert!(watermark.generation_idle(0), "fence 完成后代 0 才空闲");
        assert_eq!(
            shared
                .rebuild_atlas_pages(&[glyph_request(128)])
                .expect("third rebuild"),
            AtlasRebuildOutcome::Applied
        );
        assert_eq!(shared.atlas_generation(), 0, "第二次真正换代回到代 0");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 128);
        shared.unregister_watermark(&watermark);
    }

    #[test]
    fn removed_device_reports_device_lost_and_writes_a_log() {
        const CHILD_ENV: &str = "KYTRIX_DEVICE_LOST_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let exe = std::env::current_exe().expect("the test binary path");
            let status = std::process::Command::new(exe)
                .arg("--exact")
                .arg(concat!(
                    "render::windows::device::tests::",
                    "removed_device_reports_device_lost_and_writes_a_log"
                ))
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .status()
                .expect("spawn the device-lost child process");
            assert!(
                status.success(),
                "the device-lost child process must pass: {status}"
            );
            return;
        }
        let _guard = super::gpu_test_lock();
        let Ok(mut device) = Device::new() else {
            eprintln!("skip: no hardware D3D12 device for the device-lost test");
            return;
        };
        assert!(device.note_failure().is_none(), "healthy device");
        {
            let device5: ID3D12Device5 = device.raw().cast().expect("ID3D12Device5");
            unsafe { device5.RemoveDevice() };
        }
        let error = device
            .note_failure()
            .expect("removed device must be reported");
        match &error {
            EngineError::DeviceLost { reason, log_path } => {
                assert!(reason.to_lowercase().contains("0x887a0005"), "{reason}");
                assert!(
                    reason.contains("DEVICE_REMOVED"),
                    "原因短名必须可读: {reason}"
                );
                let path = log_path.display().to_string().to_lowercase();
                assert!(path.ends_with(".txt"), "{path}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(device.note_failure().is_some());
        assert!(device.lost_error().is_some());
    }

    #[test]
    fn resource_table_reuses_slots_with_a_new_generation() {
        let mut device = Device::new_warp().expect("WARP device");
        let texture = debug_texture(&device).expect("debug texture");
        let handle = device.allocate(texture);
        assert!(device.release(handle));
        assert!(!device.release(handle), "double release must be rejected");

        let replacement = debug_texture(&device).expect("debug texture");
        let reused = device.allocate(replacement);
        assert_eq!(reused.index(), handle.index(), "slot is reused");
        assert_ne!(reused.generation(), handle.generation());
        assert!(!device.release(handle), "stale generation is rejected");
        assert!(device.release(reused));
    }

    #[test]
    fn the_gpu_cell_reuses_a_healthy_instance() {
        let _guard = super::gpu_test_lock();
        let first = SharedGpu::new_warp().expect("WARP shared gpu");
        let before = super::gpu_cell().generation;
        let _ = SharedGpu::process_warp().expect("WARP singleton");
        assert_eq!(super::gpu_cell().generation, before);
        assert!(std::sync::Arc::strong_count(&first) >= 1);
    }
}

#[cfg(test)]
mod caps_tests {
    use smol_str::SmolStr;

    use super::{D8_REQUIRED, GpuCaps, fl_name, sm_name};
    use crate::error::EngineError;

    fn caps() -> GpuCaps {
        GpuCaps {
            adapter: SmolStr::new_static("Test Adapter"),
            vendor_id: 0x10DE,
            device_id: 0x2C02,
            dedicated_vram_mib: 15977,
            feature_level: 49_664,
            shader_model: 104,
            resource_binding_tier: 3,
            resource_heap_tier: 2,
            tiled_resources_tier: 4,
            min_precision_support: 2,
            native_16bit_shader_ops: true,
            root_signature_max: 1,
            max_sampler_descriptor_heap: 4080,
            casting_fully_typed: true,
            debug_layer: false,
            warp: false,
        }
    }

    #[test]
    fn baseline_accepts_d8_capable_device() {
        assert!(caps().baseline_ok());
        assert!(caps().check().is_ok());
    }

    #[test]
    fn baseline_rejects_each_hard_requirement() {
        let mut below_fl = caps();
        below_fl.feature_level = 45_056;
        assert!(!below_fl.baseline_ok());

        let mut below_sm = caps();
        below_sm.shader_model = 96;
        assert!(!below_sm.baseline_ok());

        let mut below_tier = caps();
        below_tier.resource_binding_tier = 1;
        assert!(!below_tier.baseline_ok());

        let mut no_cast = caps();
        no_cast.casting_fully_typed = false;
        assert!(!no_cast.baseline_ok());
        match no_cast.check().expect_err("casting is a hard requirement") {
            EngineError::UnsupportedGpu { actual, .. } => {
                assert!(actual.contains("CastSrgb false"), "{actual}")
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn check_reports_required_and_actual() {
        let mut below = caps();
        below.feature_level = 45_056;
        below.shader_model = 96;
        below.resource_binding_tier = 1;
        below.casting_fully_typed = false;

        let error = below.check().expect_err("baseline must fail");
        match &error {
            EngineError::UnsupportedGpu {
                os,
                required,
                actual,
            } => {
                assert_eq!(*os, "windows");
                assert_eq!(*required, D8_REQUIRED);
                assert_eq!(
                    actual.as_str(),
                    "FL 11_0 / SM 6_0 / BindingTier 1 / CastSrgb false"
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        let text = error.to_string();
        assert!(
            text.contains("FL 11_0 / SM 6_0 / BindingTier 1 / CastSrgb false"),
            "{text}"
        );
        assert!(text.contains("release notes"), "{text}");
        assert!(!text.to_lowercase().contains("falling back"), "{text}");
        assert!(
            !text.to_lowercase().contains("software rendering"),
            "{text}"
        );
        assert!(
            D8_REQUIRED.contains("CastingFullyTypedFormat"),
            "D8 文案必须列出 cast 能力位: {D8_REQUIRED}"
        );
    }

    #[test]
    fn describe_lists_every_diagnostic_field() {
        let text = caps().describe();
        for needle in [
            "adapter=\"Test Adapter\"",
            "vendor=0x10DE",
            "device=0x2C02",
            "vram=15977MiB",
            "FL=12_2",
            "SM=6_8",
            "binding=3",
            "heap=2",
            "tiled=4",
            "min_precision=2",
            "native16=true",
            "root_sig=1",
            "max_sampler_heap=4080",
            "cast_srgb=true",
            "debug_layer=false",
            "warp=false",
        ] {
            assert!(text.contains(needle), "missing {needle} in {text}");
        }
    }

    #[test]
    fn level_names_cover_the_d8_range() {
        assert_eq!(fl_name(45_056), "11_0");
        assert_eq!(fl_name(45_312), "11_1");
        assert_eq!(fl_name(49_152), "12_0");
        assert_eq!(fl_name(49_408), "12_1");
        assert_eq!(fl_name(49_664), "12_2");
        assert_eq!(fl_name(0), "unknown");
        assert_eq!(sm_name(96), "6_0");
        assert_eq!(sm_name(98), "6_2");
        assert_eq!(sm_name(102), "6_6");
        assert_eq!(sm_name(104), "6_8");
        assert_eq!(sm_name(0), "unknown");
    }
}
