use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use ash::vk;
use smol_str::SmolStr;

use super::{
    atlas::SharedAtlas,
    instance::VulkanInstance,
    memory::{MemoryArena, MemoryNeed, Suballocation, pick_memory_type},
    pipeline::{DescriptorPool, MAX_WINDOWS, Pipeline},
    plan::{
        self, CandidateFacts, DEVICE_OVERRIDE_ENV, DeviceFacts, DeviceSelector,
        choose_surface_format, required_usage,
    },
};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        AtlasArraySpec, AtlasPageRequest,
        atlas::AtlasRebuilt,
        frame::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT},
        retire::{GpuResourceHandle, RetireQueue},
    },
};

pub(crate) const D8_REQUIRED: &str = "Vulkan 1.2 / descriptorIndexing / timelineSemaphore / VK_KHR_swapchain / VK_KHR_wayland_surface";

pub(crate) const GPU_TIMEOUT_NS: u64 = 2_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    pub adapter: SmolStr,
    pub api_version: u32,
    pub device_type: vk::PhysicalDeviceType,
    pub descriptor_indexing: bool,
    pub runtime_descriptor_array: bool,
    pub timeline_semaphore: bool,
    pub swapchain: bool,
    pub wayland_surface: bool,
    pub incremental_present: bool,
    pub validation: bool,
    pub debug_messenger: bool,
    pub device_selector: SmolStr,
    pub device_score: u32,
}

impl GpuCaps {
    pub(crate) fn baseline_ok(&self) -> bool {
        self.api_version >= vk::API_VERSION_1_2
            && self.descriptor_indexing
            && self.timeline_semaphore
    }

    pub(crate) fn actual(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "Vulkan {}.{}.{} / descriptorIndexing={} / timelineSemaphore={}",
            vk::api_version_major(self.api_version),
            vk::api_version_minor(self.api_version),
            vk::api_version_patch(self.api_version),
            self.descriptor_indexing,
            self.timeline_semaphore
        )
    }

    pub(crate) fn describe(&self) -> SmolStr {
        smol_str::format_smolstr!(
            "adapter={:?} api={}.{}.{} type={:?} score={} selector={} descriptor_indexing={} runtime_array={} timeline={} swapchain={} wayland_surface={} incremental_present={} validation={} debug_messenger={}",
            self.adapter,
            vk::api_version_major(self.api_version),
            vk::api_version_minor(self.api_version),
            vk::api_version_patch(self.api_version),
            self.device_type,
            self.device_score,
            self.device_selector,
            self.descriptor_indexing,
            self.runtime_descriptor_array,
            self.timeline_semaphore,
            self.swapchain,
            self.wayland_surface,
            self.incremental_present,
            self.validation,
            self.debug_messenger
        )
    }

    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.baseline_ok() {
            Ok(())
        } else {
            Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: D8_REQUIRED,
                actual: self.actual(),
            })
        }
    }
}

const HANDLE_KIND_OBJECT: u8 = 1;

thread_local! {
    static ON_GPU_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn enter_gpu_thread() {
    ON_GPU_THREAD.set(true);
}

pub(crate) fn on_gpu_thread() -> bool {
    ON_GPU_THREAD.get()
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum GpuObject {
    Buffer(vk::Buffer),
    Memory(vk::DeviceMemory),
    Suballocated {
        buffer: vk::Buffer,
        allocation: Suballocation,
    },
}

impl GpuObject {
    fn destroy(self, device: &ash::Device) {
        unsafe {
            match self {
                Self::Buffer(buffer) => device.destroy_buffer(buffer, None),
                Self::Memory(memory) => device.free_memory(memory, None),
                Self::Suballocated { buffer, .. } => device.destroy_buffer(buffer, None),
            }
        }
    }

    fn suballocation(self) -> Option<Suballocation> {
        match self {
            Self::Suballocated { allocation, .. } => Some(allocation),
            Self::Buffer(_) | Self::Memory(_) => None,
        }
    }
}

pub(crate) struct Device {
    raw: ash::Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    caps: GpuCaps,
    memory_type_flags: Vec<vk::MemoryPropertyFlags>,
    properties: vk::PhysicalDeviceProperties,
    timestamp_period: f32,
    timestamp_valid_bits: u32,
}

impl Device {
    fn new(
        instance: &VulkanInstance,
        pick_surface: Option<vk::SurfaceKHR>,
    ) -> Result<Self, EngineError> {
        let devices = unsafe { instance.raw().enumerate_physical_devices() }
            .map_err(|error| init_error("enumerate_physical_devices", format!("{error:?}")))?;
        let selector = device_selector();
        let window_path = pick_surface.is_some();
        let (physical, queue_family, score) =
            select_device(instance, &devices, pick_surface, window_path, &selector)?;

        let properties = unsafe { instance.raw().get_physical_device_properties(physical) };
        let device_extensions = unsafe {
            instance
                .raw()
                .enumerate_device_extension_properties(physical)
        }
        .map_err(|error| {
            init_error(
                "enumerate_device_extension_properties",
                format!("{error:?}"),
            )
        })?;
        let has_extension = |name: &std::ffi::CStr| {
            device_extensions.iter().any(|extension| {
                (unsafe { std::ffi::CStr::from_ptr(extension.extension_name.as_ptr()) }) == name
            })
        };
        let swapchain = has_extension(ash::khr::swapchain::NAME);
        let incremental_present = has_extension(ash::khr::incremental_present::NAME);

        let mut features12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features12);
        unsafe {
            instance
                .raw()
                .get_physical_device_features2(physical, &mut features2);
        }
        let validation = instance.validation_enabled();
        let caps = GpuCaps {
            adapter: SmolStr::new(
                unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }
                    .to_string_lossy(),
            ),
            api_version: properties.api_version,
            device_type: properties.device_type,
            descriptor_indexing: features12.descriptor_indexing == vk::TRUE,
            runtime_descriptor_array: features12.runtime_descriptor_array == vk::TRUE,
            timeline_semaphore: features12.timeline_semaphore == vk::TRUE,
            swapchain,
            wayland_surface: instance.surface_fn().is_some(),
            incremental_present: incremental_present && window_path,
            validation,
            debug_messenger: instance.debug_messenger_installed(),
            device_selector: selector.describe(),
            device_score: score,
        };
        caps.check()?;
        if window_path && !caps.swapchain {
            return Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: D8_REQUIRED,
                actual: SmolStr::new_static("VK_KHR_swapchain missing"),
            });
        }
        debug_assert!(!caps.describe().is_empty());

        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let mut enabled12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .descriptor_indexing(true)
            .runtime_descriptor_array(caps.runtime_descriptor_array);
        let mut extension_pointers = Vec::with_capacity(2);
        if caps.swapchain && window_path {
            extension_pointers.push(ash::khr::swapchain::NAME.as_ptr());
            if incremental_present {
                extension_pointers.push(ash::khr::incremental_present::NAME.as_ptr());
            }
        }
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&extension_pointers)
            .push_next(&mut enabled12);
        let raw = unsafe { instance.raw().create_device(physical, &create, None) }
            .map_err(|error| init_error("create_device", format!("{error:?}")))?;
        let queue = unsafe { raw.get_device_queue(queue_family, 0) };
        let memory_properties = unsafe {
            instance
                .raw()
                .get_physical_device_memory_properties(physical)
        };
        let queue_properties = unsafe {
            instance
                .raw()
                .get_physical_device_queue_family_properties(physical)
        };
        let timestamp_valid_bits = queue_properties
            .get(queue_family as usize)
            .map_or(0, |family| family.timestamp_valid_bits);
        let memory_type_flags = memory_properties.memory_types
            [..memory_properties.memory_type_count as usize]
            .iter()
            .map(|memory_type| memory_type.property_flags)
            .collect();

        Ok(Self {
            raw,
            physical,
            queue,
            queue_family,
            caps,
            memory_type_flags,
            properties,
            timestamp_period: properties.limits.timestamp_period,
            timestamp_valid_bits,
        })
    }

    pub(crate) fn raw(&self) -> &ash::Device {
        &self.raw
    }

    pub(crate) fn physical(&self) -> vk::PhysicalDevice {
        self.physical
    }

    pub(crate) fn queue(&self) -> vk::Queue {
        self.queue
    }

    pub(crate) fn queue_family(&self) -> u32 {
        self.queue_family
    }

    pub(crate) fn caps(&self) -> &GpuCaps {
        &self.caps
    }

    pub(crate) fn timestamps(&self) -> (f32, u32) {
        (self.timestamp_period, self.timestamp_valid_bits)
    }

    pub(crate) fn memory_type(&self, type_bits: u32, need: MemoryNeed) -> Option<u32> {
        pick_memory_type(type_bits, &self.memory_type_flags, need)
    }

    pub(crate) fn memory_types(&self) -> &[vk::MemoryPropertyFlags] {
        &self.memory_type_flags
    }

    pub(crate) fn properties(&self) -> &vk::PhysicalDeviceProperties {
        &self.properties
    }
}

fn device_selector() -> DeviceSelector {
    match std::env::var(DEVICE_OVERRIDE_ENV) {
        Ok(value) => DeviceSelector::parse(&value),
        Err(_) => DeviceSelector::Auto,
    }
}

fn select_device(
    instance: &VulkanInstance,
    devices: &[vk::PhysicalDevice],
    pick_surface: Option<vk::SurfaceKHR>,
    window_path: bool,
    selector: &DeviceSelector,
) -> Result<(vk::PhysicalDevice, u32, u32), EngineError> {
    let surface_fn = instance.surface_fn();
    let mut candidates: Vec<CandidateFacts> = Vec::with_capacity(devices.len());
    let mut queue_families: Vec<Option<u32>> = Vec::with_capacity(devices.len());
    for (index, physical) in devices.iter().copied().enumerate() {
        let properties = unsafe { instance.raw().get_physical_device_properties(physical) };
        let name = unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let family = unsafe {
            instance
                .raw()
                .get_physical_device_queue_family_properties(physical)
        }
        .iter()
        .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS));
        let mut facts = DeviceFacts {
            device_type: Some(properties.device_type),
            graphics_queue: family.is_some(),
            api_version: properties.api_version,
            surface_queue: true,
            ..DeviceFacts::default()
        };
        if properties.api_version >= vk::API_VERSION_1_2 {
            let mut features12 = vk::PhysicalDeviceVulkan12Features::default();
            let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features12);
            unsafe {
                instance
                    .raw()
                    .get_physical_device_features2(physical, &mut features2);
            }
            facts.descriptor_indexing = features12.descriptor_indexing == vk::TRUE;
            facts.timeline_semaphore = features12.timeline_semaphore == vk::TRUE;
        }
        if window_path
            && let (Some(surface_fn), Some(surface), Some(family)) =
                (surface_fn, pick_surface, family)
        {
            facts.surface_queue = unsafe {
                surface_fn.get_physical_device_surface_support(physical, family as u32, surface)
            }
            .unwrap_or(false);
            if facts.surface_queue {
                facts.swapchain_extension =
                    device_has_extension(instance, physical, ash::khr::swapchain::NAME);
                let formats =
                    unsafe { surface_fn.get_physical_device_surface_formats(physical, surface) }
                        .unwrap_or_default();
                facts.surface_format =
                    choose_surface_format(&formats, crate::render::colorspace::accepts_color_space)
                        .is_some();
                let modes = unsafe {
                    surface_fn.get_physical_device_surface_present_modes(physical, surface)
                }
                .unwrap_or_default();
                facts.present_mode = plan::choose_present_mode(&modes).is_some();
                if let Ok(capabilities) = unsafe {
                    surface_fn.get_physical_device_surface_capabilities(physical, surface)
                } {
                    facts.usage = capabilities
                        .supported_usage_flags
                        .contains(required_usage());
                    facts.extent = capabilities.min_image_extent.width
                        <= capabilities.max_image_extent.width
                        && capabilities.min_image_extent.height
                            <= capabilities.max_image_extent.height;
                }
            }
        }
        candidates.push(CandidateFacts {
            index: index as u32,
            name: SmolStr::new(name),
            facts,
        });
        queue_families.push(family.map(|family| family as u32));
    }
    let selected = plan::pick_best(&candidates, window_path, selector).map_err(|actual| {
        EngineError::UnsupportedGpu {
            os: "linux",
            required: D8_REQUIRED,
            actual,
        }
    })?;
    let queue_family = queue_families[selected]
        .expect("pick_best only selects devices with a GRAPHICS queue family");
    let score = plan::score_device(&candidates[selected].facts, window_path)
        .expect("pick_best only selects devices that pass its own filter");
    Ok((devices[selected], queue_family, score))
}

fn device_has_extension(
    instance: &VulkanInstance,
    physical: vk::PhysicalDevice,
    name: &std::ffi::CStr,
) -> bool {
    let extensions = unsafe {
        instance
            .raw()
            .enumerate_device_extension_properties(physical)
    }
    .unwrap_or_default();
    extensions.iter().any(|extension| {
        (unsafe { std::ffi::CStr::from_ptr(extension.extension_name.as_ptr()) }) == name
    })
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { self.raw.destroy_device(None) };
    }
}

pub(crate) struct GpuBuffer {
    pub buffer: vk::Buffer,
    pub pointer: Option<*mut u8>,
    pub handle: GpuResourceHandle,
}

unsafe impl Send for GpuBuffer {}

struct LostInfo {
    reason: SmolStr,
    log_path: PathBuf,
}

pub(crate) struct SharedGpu {
    descriptors: Mutex<DescriptorPool>,
    pipeline: Pipeline,
    atlas: RwLock<SharedAtlas>,
    atlas_pages: [AtomicU32; 2],
    queue_lock: Mutex<()>,
    idle_timeline: vk::Semaphore,
    idle_sequence: AtomicU64,
    resources: Mutex<ResourceTable>,
    memory: Mutex<MemoryArena>,
    lost: Mutex<Option<LostInfo>>,
    device: Device,
    instance: VulkanInstance,
}

impl SharedGpu {
    pub(crate) fn process_windowed(
        display: *mut c_void,
        surface: *mut c_void,
    ) -> Result<Arc<Self>, EngineError> {
        static GPU: OnceLock<Mutex<Weak<SharedGpu>>> = OnceLock::new();
        let slot = GPU.get_or_init(|| Mutex::new(Weak::new()));
        Self::shared(slot, || Self::create_windowed(display, surface))
    }

    pub(crate) fn process_headless() -> Result<Arc<Self>, EngineError> {
        static GPU: OnceLock<Mutex<Weak<SharedGpu>>> = OnceLock::new();
        let slot = GPU.get_or_init(|| Mutex::new(Weak::new()));
        Self::shared(slot, Self::create_headless)
    }

    #[cfg(test)]
    pub(crate) fn new_headless_isolated() -> Result<Arc<Self>, EngineError> {
        Ok(Arc::new(Self::create_headless()?))
    }

    fn shared(
        slot: &'static Mutex<Weak<SharedGpu>>,
        create: impl FnOnce() -> Result<SharedGpu, EngineError>,
    ) -> Result<Arc<Self>, EngineError> {
        let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(gpu) = slot.upgrade() {
            return Ok(gpu);
        }
        let gpu = Arc::new(create()?);
        *slot = Arc::downgrade(&gpu);
        Ok(gpu)
    }

    fn create_windowed(display: *mut c_void, surface: *mut c_void) -> Result<Self, EngineError> {
        let instance = VulkanInstance::new_product()?;
        let probe = instance.create_surface(display, surface)?;
        let device = match Device::new(&instance, Some(probe)) {
            Ok(device) => device,
            Err(error) => {
                instance.destroy_surface(probe);
                return Err(error);
            }
        };
        instance.destroy_surface(probe);
        Self::with_device(instance, device)
    }

    fn create_headless() -> Result<Self, EngineError> {
        let instance = VulkanInstance::new_headless()?;
        let device = Device::new(&instance, None)?;
        Self::with_device(instance, device)
    }

    fn with_device(instance: VulkanInstance, device: Device) -> Result<Self, EngineError> {
        eprintln!("kytrix: vulkan {}", device.caps().describe());
        let pipeline = Pipeline::new(&device)?;
        let atlas = SharedAtlas::new(&device)?;
        let descriptors = DescriptorPool::new(device.raw(), pipeline.set_layout())?;
        pipeline.write_atlas(
            descriptors.all_sets(),
            atlas.glyph_view(),
            atlas.image_view(),
            atlas.sampler(),
        );
        let idle_timeline = {
            let mut info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            unsafe {
                device.raw().create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut info),
                    None,
                )
            }
            .map_err(|error| init_error("create_semaphore(idle)", format!("{error:?}")))?
        };
        let atlas_pages = [
            AtomicU32::new(atlas.page(AtlasKind::Glyph)),
            AtomicU32::new(atlas.page(AtlasKind::Image)),
        ];
        let gpu = Self {
            descriptors: Mutex::new(descriptors),
            pipeline,
            atlas: RwLock::new(atlas),
            atlas_pages,
            queue_lock: Mutex::new(()),
            idle_timeline,
            idle_sequence: AtomicU64::new(0),
            resources: Mutex::new(ResourceTable::new()),
            memory: Mutex::new(MemoryArena::new()),
            lost: Mutex::new(None),
            device,
            instance,
        };
        {
            let atlas = gpu.atlas();
            gpu.transition_atlas_layout(&atlas, &[AtlasKind::Glyph, AtlasKind::Image])?;
        }
        Ok(gpu)
    }

    pub(crate) fn raw(&self) -> &ash::Device {
        self.device.raw()
    }

    pub(crate) fn physical(&self) -> vk::PhysicalDevice {
        self.device.physical()
    }

    pub(crate) fn queue_family(&self) -> u32 {
        self.device.queue_family()
    }

    pub(crate) fn caps(&self) -> &GpuCaps {
        self.device.caps()
    }

    pub(crate) fn timestamps(&self) -> (f32, u32) {
        self.device.timestamps()
    }

    pub(crate) fn memory_type(&self, type_bits: u32, need: MemoryNeed) -> Option<u32> {
        self.device.memory_type(type_bits, need)
    }

    pub(crate) fn allocate_buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        need: MemoryNeed,
    ) -> Result<GpuBuffer, EngineError> {
        let size = size.max(4);
        let buffer = unsafe {
            self.device.raw().create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(|error| init_error("create_buffer(suballocated)", format!("{error:?}")))?;
        let requirements = unsafe { self.device.raw().get_buffer_memory_requirements(buffer) };
        let allocated = {
            let mut arena = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
            arena.allocate(
                self.device.raw(),
                self.device.memory_types(),
                requirements.memory_type_bits,
                need,
                requirements.size,
                requirements.alignment,
            )
        };
        let (allocation, pointer) = match allocated {
            Ok(values) => values,
            Err(error) => {
                unsafe { self.device.raw().destroy_buffer(buffer, None) };
                return Err(error);
            }
        };
        if let Err(error) = unsafe {
            self.device
                .raw()
                .bind_buffer_memory(buffer, allocation.memory, allocation.offset)
        } {
            let mut arena = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
            arena.free(allocation);
            unsafe { self.device.raw().destroy_buffer(buffer, None) };
            return Err(init_error(
                "bind_buffer_memory(suballocated)",
                format!("{error:?}"),
            ));
        }
        let handle = self.allocate(GpuObject::Suballocated { buffer, allocation });
        Ok(GpuBuffer {
            buffer,
            pointer,
            handle,
        })
    }

    #[cfg(test)]
    pub(crate) fn memory_stats(&self) -> (u64, u64, u64) {
        let arena = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
        (
            arena.device_allocations(),
            arena.live_bytes(),
            arena.reserved_bytes(),
        )
    }

    pub(crate) fn instance(&self) -> &VulkanInstance {
        &self.instance
    }

    pub(crate) fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    pub(crate) fn with_queue<T>(&self, access: impl FnOnce(vk::Queue) -> T) -> T {
        let _guard = self
            .queue_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        access(self.device.queue())
    }

    pub(crate) fn wait_queue_idle(&self) -> Result<(), EngineError> {
        let value = self.idle_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let timeline = [self.idle_timeline];
        let values = [value];
        let mut timeline_info =
            vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
        let submit = vk::SubmitInfo::default()
            .signal_semaphores(&timeline)
            .push_next(&mut timeline_info);
        self.with_queue(|queue| unsafe {
            self.device
                .raw()
                .queue_submit(queue, &[submit], vk::Fence::null())
        })
        .map_err(|error| self.classify_failure(error, "queue_submit(idle)"))?;
        let wait = vk::SemaphoreWaitInfo::default()
            .semaphores(&timeline)
            .values(&values);
        match unsafe { self.device.raw().wait_semaphores(&wait, GPU_TIMEOUT_NS) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.mark_lost(
                smol_str::format_smolstr!(
                    "GPU idle wait timed out after {} ms (device still present; GPU hung)",
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => Err(self.classify_failure(error, "wait_semaphores(idle)")),
        }
    }

    pub(crate) fn wait_fence(
        &self,
        fence: vk::Fence,
        context: &'static str,
    ) -> Result<(), EngineError> {
        let fences = [fence];
        match unsafe { self.device.raw().wait_for_fences(&fences, true, GPU_TIMEOUT_NS) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.mark_lost(
                smol_str::format_smolstr!(
                    "{context}: GPU fence wait timed out after {} ms (device still present; GPU hung)",
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => Err(self.classify_failure(error, context)),
        }
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

    #[cfg(test)]
    pub(crate) fn rebuild_atlas_pages(
        &self,
        requests: &[AtlasPageRequest],
    ) -> Result<AtlasRebuilt, EngineError> {
        let mut atlas = self.atlas_write();
        self.rebuild_atlas_pages_locked(&mut atlas, requests)
    }

    pub(crate) fn rebuild_atlas_pages_locked(
        &self,
        atlas: &mut SharedAtlas,
        requests: &[AtlasPageRequest],
    ) -> Result<AtlasRebuilt, EngineError> {
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
        let mut rebuilt = AtlasRebuilt::default();
        if pending.is_empty() {
            return Ok(rebuilt);
        }
        self.wait_queue_idle()?;
        for request in &pending {
            atlas.recreate(&self.device, request.kind, request.spec)?;
            self.atlas_pages[request.kind.index() as usize]
                .store(request.spec.page, Ordering::Release);
            rebuilt.mark(request.kind);
        }
        debug_assert!(
            rebuilt.glyph || rebuilt.image,
            "a non-empty rebuild must recreate at least one atlas page"
        );
        let kinds: Vec<AtlasKind> = pending.iter().map(|request| request.kind).collect();
        self.transition_atlas_layout(atlas, &kinds)?;
        {
            let descriptors = self
                .descriptors
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.pipeline.write_atlas(
                descriptors.all_sets(),
                atlas.glyph_view(),
                atlas.image_view(),
                atlas.sampler(),
            );
        }
        Ok(rebuilt)
    }

    pub(crate) fn window_descriptors(self: &Arc<Self>) -> Result<DescriptorLease, EngineError> {
        let mut descriptors = self
            .descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let chunk = descriptors.allocate().ok_or_else(|| EngineError::AppInit {
            detail: smol_str::format_smolstr!(
                "descriptor pool exhausted (window segments: {MAX_WINDOWS})"
            ),
        })?;
        let sets = descriptors.chunk(chunk);
        Ok(DescriptorLease {
            gpu: Arc::clone(self),
            chunk,
            sets,
        })
    }

    fn release_window_descriptors(&self, chunk: usize) {
        self.descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .free(chunk);
    }

    pub(crate) fn write_slot(
        &self,
        sets: &[vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT],
        slot: usize,
        upload: vk::Buffer,
        clip_offset: u64,
    ) {
        let _guard = self
            .descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.pipeline.write_slot(sets[slot], upload, clip_offset);
    }

    pub(crate) fn allocate(&self, object: GpuObject) -> GpuResourceHandle {
        self.resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(object)
    }

    pub(crate) fn release(&self, handle: GpuResourceHandle) -> bool {
        let object = self
            .resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(handle);
        let Some(object) = object else {
            return false;
        };
        object.destroy(self.device.raw());
        if let Some(allocation) = object.suballocation() {
            self.memory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .free(allocation);
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.resources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live()
    }

    pub(crate) fn note_failure(&self, result: vk::Result, frame: u64) -> Option<EngineError> {
        if let Some(error) = self.lost_error() {
            return Some(error);
        }
        if result != vk::Result::ERROR_DEVICE_LOST {
            return None;
        }
        Some(self.mark_lost(
            smol_str::format_smolstr!("VK_ERROR_DEVICE_LOST ({result:?})"),
            frame,
        ))
    }

    pub(crate) fn mark_lost(&self, reason: SmolStr, frame: u64) -> EngineError {
        if let Some(error) = self.lost_error() {
            return error;
        }
        let log_path = write_diagnostics(&reason, frame);
        let mut lost = self.lost.lock().unwrap_or_else(PoisonError::into_inner);
        *lost = Some(LostInfo {
            reason: reason.clone(),
            log_path: log_path.clone(),
        });
        EngineError::DeviceLost { reason, log_path }
    }

    pub(crate) fn lost_error(&self) -> Option<EngineError> {
        let lost = self.lost.lock().unwrap_or_else(PoisonError::into_inner);
        lost.as_ref().map(|lost| EngineError::DeviceLost {
            reason: lost.reason.clone(),
            log_path: lost.log_path.clone(),
        })
    }

    pub(crate) fn classify_failure(
        &self,
        result: vk::Result,
        context: &'static str,
    ) -> EngineError {
        match self.note_failure(result, 0) {
            Some(lost) => lost,
            None => init_error(context, format!("{result:?}")),
        }
    }

    fn transition_atlas_layout(
        &self,
        atlas: &SharedAtlas,
        kinds: &[AtlasKind],
    ) -> Result<(), EngineError> {
        if kinds.is_empty() {
            return Ok(());
        }
        let raw = self.device.raw();
        let pool = unsafe {
            raw.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(self.queue_family())
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }
        .map_err(|error| {
            init_error(
                "create_command_pool(atlas transition)",
                format!("{error:?}"),
            )
        })?;
        let outcome = self.record_and_wait_transitions(raw, pool, atlas, kinds);
        unsafe { raw.destroy_command_pool(pool, None) };
        outcome
    }

    fn record_and_wait_transitions(
        &self,
        raw: &ash::Device,
        pool: vk::CommandPool,
        atlas: &SharedAtlas,
        kinds: &[AtlasKind],
    ) -> Result<(), EngineError> {
        let command = unsafe {
            raw.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(|error| {
            init_error(
                "allocate_command_buffers(atlas transition)",
                format!("{error:?}"),
            )
        })?[0];
        let fence = unsafe { raw.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|error| init_error("create_fence(atlas transition)", format!("{error:?}")))?;
        let outcome = (|| -> Result<(), EngineError> {
            unsafe {
                raw.begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
            }
            .map_err(|error| {
                init_error(
                    "begin_command_buffer(atlas transition)",
                    format!("{error:?}"),
                )
            })?;
            atlas.record_layout_transitions(raw, command, kinds);
            unsafe { raw.end_command_buffer(command) }.map_err(|error| {
                init_error("end_command_buffer(atlas transition)", format!("{error:?}"))
            })?;
            let commands = [command];
            let submit = vk::SubmitInfo::default().command_buffers(&commands);
            self.with_queue(|queue| unsafe { raw.queue_submit(queue, &[submit], fence) })
                .map_err(|error| self.classify_failure(error, "queue_submit(atlas transition)"))?;
            self.wait_fence(fence, "wait_for_fences(atlas transition)")
        })();
        unsafe {
            raw.destroy_fence(fence, None);
            raw.free_command_buffers(pool, &[command]);
        }
        outcome
    }
}

impl Drop for SharedGpu {
    fn drop(&mut self) {
        let _ = self.wait_queue_idle();
        let raw = self.device.raw().clone();
        let resources = self
            .resources
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        resources.drain_all(|object| object.destroy(&raw));
        let memory = self
            .memory
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        unsafe { memory.destroy(&raw) };
        unsafe { raw.destroy_semaphore(self.idle_timeline, None) };
    }
}

pub(crate) struct DescriptorLease {
    gpu: Arc<SharedGpu>,
    chunk: usize,
    sets: [vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT],
}

impl DescriptorLease {
    pub(crate) fn sets(&self) -> &[vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT] {
        &self.sets
    }
}

impl Drop for DescriptorLease {
    fn drop(&mut self) {
        self.gpu.release_window_descriptors(self.chunk);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeferredHandles {
    leaked: bool,
}

impl DeferredHandles {
    pub(crate) fn leak(&mut self) {
        self.leaked = true;
    }

    pub(crate) fn leaked(&self) -> bool {
        self.leaked
    }
}

pub(crate) struct FrameSync {
    shared: Arc<SharedGpu>,
    timeline: vk::Semaphore,
    fences: [vk::Fence; MAX_FRAMES_IN_FLIGHT],
    image_available: [vk::Semaphore; MAX_FRAMES_IN_FLIGHT],
    ring: InFlightRing,
    retire: RetireQueue,
    handles: DeferredHandles,
}

impl FrameSync {
    pub(crate) fn new(shared: &Arc<SharedGpu>) -> Result<Self, EngineError> {
        let raw = shared.raw();
        let mut timeline_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let timeline = unsafe {
            raw.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline_info),
                None,
            )
        }
        .map_err(|error| init_error("create_semaphore(timeline)", format!("{error:?}")))?;

        let mut fences = [vk::Fence::null(); MAX_FRAMES_IN_FLIGHT];
        for fence in fences.iter_mut() {
            *fence = unsafe {
                raw.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            }
            .map_err(|error| init_error("create_fence", format!("{error:?}")))?;
        }
        let mut image_available = [vk::Semaphore::null(); MAX_FRAMES_IN_FLIGHT];
        for semaphore in image_available.iter_mut() {
            *semaphore = unsafe { raw.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                .map_err(|error| {
                    init_error("create_semaphore(image_available)", format!("{error:?}"))
                })?;
        }
        Ok(Self {
            shared: Arc::clone(shared),
            timeline,
            fences,
            image_available,
            ring: InFlightRing::new(),
            retire: RetireQueue::new(),
            handles: DeferredHandles::default(),
        })
    }

    pub(crate) fn leak(&mut self) {
        self.handles.leak();
    }

    pub(crate) fn slot(&mut self) -> FrameSlot {
        let slot = self.ring.acquire();
        debug_assert_eq!(
            self.ring.last_signaled(),
            slot.signal_value.saturating_sub(1),
            "ring must advance exactly one step per submitted frame"
        );
        slot
    }

    pub(crate) fn image_available(&self, slot: usize) -> vk::Semaphore {
        self.image_available[slot]
    }

    pub(crate) fn wait_slot(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        let fences = [self.fences[slot.index]];
        match unsafe {
            self.shared
                .raw()
                .wait_for_fences(&fences, true, GPU_TIMEOUT_NS)
        } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.shared.mark_lost(
                smol_str::format_smolstr!(
                    "frame slot {} fence wait timed out after {} ms (device still present; GPU hung)",
                    slot.index,
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => match self.shared.note_failure(error, 0) {
                Some(lost) => Err(lost),
                None => Err(init_error("wait_for_fences", format!("{error:?}"))),
            },
        }
    }

    pub(crate) fn reset_fence(&self, slot: usize) -> Result<(), EngineError> {
        let fences = [self.fences[slot]];
        unsafe { self.shared.raw().reset_fences(&fences) }
            .map_err(|error| init_error("reset_fences", format!("{error:?}")))
    }

    pub(crate) fn fence(&self, slot: usize) -> vk::Fence {
        self.fences[slot]
    }

    pub(crate) fn advance(&mut self, slot: FrameSlot) {
        self.ring.advance(slot);
    }

    pub(crate) fn completed_value(&self) -> Result<u64, EngineError> {
        unsafe { self.shared.raw().get_semaphore_counter_value(self.timeline) }
            .map_err(|error| init_error("get_semaphore_counter_value", format!("{error:?}")))
    }

    pub(crate) fn timeline(&self) -> vk::Semaphore {
        self.timeline
    }

    pub(crate) fn submitted_value(&self) -> u64 {
        self.ring.last_signaled()
    }

    pub(crate) fn wait_idle(&self) -> Result<(), EngineError> {
        let value = self.submitted_value();
        if value == 0 {
            return Ok(());
        }
        let timeline = [self.timeline];
        let values = [value];
        let wait = vk::SemaphoreWaitInfo::default()
            .semaphores(&timeline)
            .values(&values);
        match unsafe { self.shared.raw().wait_semaphores(&wait, GPU_TIMEOUT_NS) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(self.shared.mark_lost(
                smol_str::format_smolstr!(
                    "GPU teardown wait timed out after {} ms (device still present; GPU hung)",
                    GPU_TIMEOUT_NS / 1_000_000
                ),
                0,
            )),
            Err(error) => match self.shared.note_failure(error, 0) {
                Some(lost) => Err(lost),
                None => Err(init_error(
                    "wait_semaphores(teardown)",
                    format!("{error:?}"),
                )),
            },
        }
    }

    pub(crate) fn retire(&mut self, sequence: u64, handle: GpuResourceHandle) {
        self.retire.retire(sequence, handle);
    }

    pub(crate) fn drain_retire(&mut self, completed: u64) -> usize {
        let shared = Arc::clone(&self.shared);
        self.retire.on_completed(completed, &mut |handle| {
            let _ = shared.release(handle);
        })
    }

    pub(crate) fn drain_all_retired(&mut self) -> usize {
        let shared = Arc::clone(&self.shared);
        self.retire.drain_all(&mut |handle| {
            let _ = shared.release(handle);
        })
    }

    #[cfg(test)]
    pub(crate) fn pending_retired(&self) -> usize {
        self.retire.pending()
    }
}

impl Drop for FrameSync {
    fn drop(&mut self) {
        if self.handles.leaked() {
            return;
        }
        unsafe {
            for fence in self.fences {
                self.shared.raw().destroy_fence(fence, None);
            }
            for semaphore in self.image_available {
                self.shared.raw().destroy_semaphore(semaphore, None);
            }
            self.shared.raw().destroy_semaphore(self.timeline, None);
        }
    }
}

struct ResourceTable {
    slots: Vec<Option<GpuObject>>,
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

    fn insert(&mut self, object: GpuObject) -> GpuResourceHandle {
        if let Some(index) = self.free.pop() {
            let generation = self.generations[index as usize].wrapping_add(1);
            self.generations[index as usize] = generation;
            self.slots[index as usize] = Some(object);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, generation, index)
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Some(object));
            self.generations.push(0);
            GpuResourceHandle::new(HANDLE_KIND_OBJECT, 0, index)
        }
    }

    fn take(&mut self, handle: GpuResourceHandle) -> Option<GpuObject> {
        if handle.kind() != HANDLE_KIND_OBJECT {
            return None;
        }
        let index = handle.index() as usize;
        if self.generations.get(index).copied() != Some(handle.generation()) {
            return None;
        }
        let object = self.slots.get_mut(index)?.take();
        if object.is_some() {
            self.free.push(index as u32);
        }
        object
    }

    #[cfg(test)]
    fn live(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    fn drain_all(&mut self, mut destroy: impl FnMut(GpuObject)) {
        for slot in &mut self.slots {
            if let Some(object) = slot.take() {
                destroy(object);
            }
        }
        self.free.clear();
    }
}

pub(crate) fn init_error(context: &'static str, detail: impl std::fmt::Display) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!("vulkan {context}: {detail}"),
    }
}

fn write_diagnostics(reason: &str, frame: u64) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kytrix-vk-{}-{frame}.txt", std::process::id()));
    let body = format!("kytrix vulkan device lost\nreason: {reason}\n");
    let _ = std::fs::write(&path, body);
    path
}

#[cfg(test)]
pub(crate) fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{GpuObject, ResourceTable, SharedGpu, gpu_test_lock};
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, AtlasPageRequest},
    };

    fn glyph_request(page: u32) -> AtlasPageRequest {
        AtlasPageRequest {
            kind: AtlasKind::Glyph,
            spec: AtlasArraySpec { page, layers: 1 },
        }
    }

    #[test]
    fn the_process_wide_gpu_is_created_once() {
        let _guard = gpu_test_lock();
        let first = SharedGpu::process_headless().expect("headless shared gpu");
        let second = SharedGpu::process_headless().expect("headless shared gpu");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(
            first.atlas().page(AtlasKind::Glyph),
            second.atlas().page(AtlasKind::Glyph)
        );
    }

    #[test]
    fn rebuilding_a_shared_atlas_page_swaps_the_texture() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 1, "占位页是 1×1");
        shared
            .rebuild_atlas_pages(&[glyph_request(64)])
            .expect("rebuild");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
        assert_eq!(
            shared.atlas_page(AtlasKind::Glyph),
            64,
            "无锁快照与写锁内页尺寸必须一致"
        );
        shared
            .rebuild_atlas_pages(&[glyph_request(64)])
            .expect("no-op rebuild");
        assert_eq!(shared.atlas().page(AtlasKind::Glyph), 64);
    }

    #[test]
    fn windows_receive_disjoint_and_reusable_descriptor_segments() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let first = shared.window_descriptors().expect("first segment");
        let second = shared.window_descriptors().expect("second segment");
        for slot in 0..first.sets().len() {
            assert_ne!(
                first.sets()[slot],
                second.sets()[slot],
                "两个窗口的帧槽描述符不得共享"
            );
        }
        let recycled = first.sets()[0];
        drop(first);
        let third = shared.window_descriptors().expect("recycled segment");
        assert_eq!(
            third.sets()[0],
            recycled,
            "归还的段必须被复用（池容量有界）"
        );
    }

    #[test]
    fn resource_handles_reject_stale_generations_and_drain_on_teardown() {
        let mut table = ResourceTable::new();
        let handle = table.insert(GpuObject::Buffer(vk::Buffer::null()));
        assert!(table.take(handle).is_some());
        assert!(table.take(handle).is_none(), "double take is rejected");

        let recycled = table.insert(GpuObject::Buffer(vk::Buffer::null()));
        assert_eq!(recycled.index(), handle.index(), "slot is reused");
        assert_ne!(recycled.generation(), handle.generation());
        assert!(
            table.take(handle).is_none(),
            "stale generation must be rejected"
        );

        table.insert(GpuObject::Memory(vk::DeviceMemory::null()));
        let mut drained = 0usize;
        table.drain_all(|_| drained += 1);
        assert_eq!(drained, 2, "teardown drains every remaining object");
        assert_eq!(table.live(), 0);
    }
}
