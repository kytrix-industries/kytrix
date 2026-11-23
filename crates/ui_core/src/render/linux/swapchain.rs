use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicU32, Ordering},
};

use ash::vk::{self, Handle};

use crate::{
    error::EngineError,
    render::{
        compositor::DamageRects,
        linux::{
            device::{DeferredHandles, SharedGpu, on_gpu_thread},
            plan::{
                self, SwapchainKey, choose_composite_alpha, choose_extent, choose_image_count,
                choose_present_mode, choose_surface_format,
            },
        },
    },
};

const ACQUIRE_TIMEOUT_NS: u64 = 100 * 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Acquire {
    Image { index: u32, suboptimal: bool },
    NotReady,
    OutOfDate,
    SurfaceLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Present {
    Presented,
    Suboptimal,
    OutOfDate,
    SurfaceLost,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LiveSwapchains(Arc<AtomicU32>);

impl LiveSwapchains {
    fn note_created(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn note_destroyed(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    fn debug_assert_idle(&self) {
        debug_assert_eq!(
            self.count(),
            0,
            "recreating a VkSurfaceKHR requires all of its swapchains to be destroyed first \
             (VUID-vkDestroySurfaceKHR-surface-01266); force-drain the deferred swapchains"
        );
    }
}

pub(crate) struct Surface {
    gpu: Arc<SharedGpu>,
    raw: vk::SurfaceKHR,
    display: *mut std::ffi::c_void,
    wl_surface: *mut std::ffi::c_void,
    live_swapchains: LiveSwapchains,
    handles: DeferredHandles,
}

impl Surface {
    pub(crate) fn new(
        gpu: &Arc<SharedGpu>,
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            gpu: Arc::clone(gpu),
            raw: gpu.instance().create_surface(display, surface)?,
            display,
            wl_surface: surface,
            live_swapchains: LiveSwapchains::default(),
            handles: DeferredHandles::default(),
        })
    }

    pub(crate) fn leak(&mut self) {
        self.handles.leak();
    }

    pub(crate) fn raw(&self) -> vk::SurfaceKHR {
        self.raw
    }

    fn live_swapchains(&self) -> &LiveSwapchains {
        &self.live_swapchains
    }

    pub(crate) fn recreate(&mut self) -> Result<(), EngineError> {
        self.live_swapchains.debug_assert_idle();
        let raw = self
            .gpu
            .instance()
            .create_surface(self.display, self.wl_surface)?;
        let old = std::mem::replace(&mut self.raw, raw);
        self.gpu.instance().destroy_surface(old);
        Ok(())
    }
}

unsafe impl Send for Surface {}

impl Drop for Surface {
    fn drop(&mut self) {
        if self.handles.leaked() {
            return;
        }
        self.gpu.instance().destroy_surface(self.raw);
    }
}

#[derive(Debug)]
pub(crate) enum CreateFailure {
    OutOfDate,
    SurfaceLost,
    Fatal(EngineError),
}

impl CreateFailure {
    pub(crate) fn into_error(self) -> EngineError {
        match self {
            Self::Fatal(error) => error,
            Self::OutOfDate => EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static(
                    "swapchain was out of date during initial window creation",
                ),
            },
            Self::SurfaceLost => EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static(
                    "surface was lost during initial window creation",
                ),
            },
        }
    }
}

fn recoverable_create_failure(result: vk::Result) -> Option<CreateFailure> {
    match result {
        vk::Result::ERROR_OUT_OF_DATE_KHR => Some(CreateFailure::OutOfDate),
        vk::Result::ERROR_SURFACE_LOST_KHR => Some(CreateFailure::SurfaceLost),
        _ => None,
    }
}

fn create_failure(shared: &SharedGpu, result: vk::Result, context: &'static str) -> CreateFailure {
    recoverable_create_failure(result)
        .unwrap_or_else(|| CreateFailure::Fatal(shared.classify_failure(result, context)))
}

pub(crate) struct Swapchain {
    device: ash::Device,
    raw: vk::SwapchainKHR,
    fn_: ash::khr::swapchain::Device,
    images: Vec<vk::Image>,
    key: SwapchainKey,
    incremental: bool,
    render_finished: Vec<vk::Semaphore>,
    live_swapchains: LiveSwapchains,
    handles: DeferredHandles,
}

impl Swapchain {
    pub(crate) fn new(
        shared: &SharedGpu,
        surface: &Surface,
        size: (u32, u32),
    ) -> Result<Self, CreateFailure> {
        Self::create(shared, surface, size, vk::SwapchainKHR::null())
    }

    pub(crate) fn replace(
        shared: &SharedGpu,
        surface: &Surface,
        size: (u32, u32),
        old: vk::SwapchainKHR,
    ) -> Result<Self, CreateFailure> {
        Self::create(shared, surface, size, old)
    }

    fn create(
        shared: &SharedGpu,
        surface: &Surface,
        size: (u32, u32),
        old: vk::SwapchainKHR,
    ) -> Result<Self, CreateFailure> {
        let instance = shared.instance();
        let live_swapchains = surface.live_swapchains().clone();
        let surface = surface.raw();
        let surface_fn = instance.surface_fn().ok_or_else(|| {
            CreateFailure::Fatal(EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static("swapchain requires a Wayland surface"),
            })
        })?;
        let formats =
            unsafe { surface_fn.get_physical_device_surface_formats(shared.physical(), surface) }
                .map_err(|error| {
                create_failure(shared, error, "get_physical_device_surface_formats")
            })?;
        let chosen = choose_surface_format(
            &formats,
            crate::render::colorspace::accepts_color_space,
        )
        .ok_or_else(|| {
            CreateFailure::Fatal(EngineError::UnsupportedGpu {
                os: "linux",
                required: "B8G8R8A8_SRGB surface format with VK_COLOR_SPACE_SRGB_NONLINEAR_KHR",
                actual: smol_str::format_smolstr!(
                    "surface reports {:?}",
                    formats
                        .iter()
                        .map(|format| (format.format, format.color_space))
                        .collect::<Vec<_>>()
                ),
            })
        })?;
        let capabilities = unsafe {
            surface_fn.get_physical_device_surface_capabilities(shared.physical(), surface)
        }
        .map_err(|error| {
            create_failure(shared, error, "get_physical_device_surface_capabilities")
        })?;
        let present_modes = unsafe {
            surface_fn.get_physical_device_surface_present_modes(shared.physical(), surface)
        }
        .map_err(|error| {
            create_failure(shared, error, "get_physical_device_surface_present_modes")
        })?;
        let present_mode = choose_present_mode(&present_modes).ok_or_else(|| {
            CreateFailure::Fatal(EngineError::UnsupportedGpu {
                os: "linux",
                required: "VK_PRESENT_MODE_MAILBOX_KHR on a Wayland surface",
                actual: smol_str::format_smolstr!("present modes {present_modes:?}"),
            })
        })?;
        let extent = choose_extent(
            (
                capabilities.current_extent.width,
                capabilities.current_extent.height,
            ),
            (
                capabilities.min_image_extent.width,
                capabilities.min_image_extent.height,
            ),
            (
                capabilities.max_image_extent.width,
                capabilities.max_image_extent.height,
            ),
            size,
            capabilities.current_transform,
        )
        .ok_or_else(|| {
            CreateFailure::Fatal(EngineError::UnsupportedGpu {
                os: "linux",
                required: "a non-degenerate surface extent range",
                actual: smol_str::format_smolstr!(
                    "min={:?} max={:?} current={:?}",
                    capabilities.min_image_extent,
                    capabilities.max_image_extent,
                    capabilities.current_extent
                ),
            })
        })?;
        if !capabilities
            .supported_usage_flags
            .contains(plan::required_usage())
        {
            return Err(CreateFailure::Fatal(EngineError::UnsupportedGpu {
                os: "linux",
                required: "COLOR_ATTACHMENT | TRANSFER_DST swapchain images",
                actual: smol_str::format_smolstr!(
                    "surface supports {:?}",
                    capabilities.supported_usage_flags
                ),
            }));
        }
        let composite_alpha = choose_composite_alpha(capabilities.supported_composite_alpha)
            .ok_or_else(|| {
                CreateFailure::Fatal(EngineError::UnsupportedGpu {
                    os: "linux",
                    required: "at least one composite alpha mode",
                    actual: smol_str::format_smolstr!(
                        "surface supports {:?}",
                        capabilities.supported_composite_alpha
                    ),
                })
            })?;
        let image_count =
            choose_image_count(capabilities.min_image_count, capabilities.max_image_count);
        let key = SwapchainKey {
            extent,
            format: chosen.format,
            color_space: chosen.color_space,
            present_mode,
            composite_alpha,
            pre_transform: capabilities.current_transform,
            image_count,
        };
        let fn_ = ash::khr::swapchain::Device::new(instance.raw(), shared.raw());
        let create = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .old_swapchain(old)
            .min_image_count(image_count)
            .image_format(chosen.format)
            .image_color_space(chosen.color_space)
            .image_extent(vk::Extent2D {
                width: extent.0,
                height: extent.1,
            })
            .image_array_layers(1)
            .image_usage(plan::required_usage())
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(composite_alpha)
            .present_mode(present_mode)
            .clipped(true);
        let raw = unsafe { fn_.create_swapchain(&create, None) }
            .map_err(|error| create_failure(shared, error, "create_swapchain_khr"))?;
        let images = match unsafe { fn_.get_swapchain_images(raw) } {
            Ok(images) => images,
            Err(error) => {
                unsafe { fn_.destroy_swapchain(raw, None) };
                return Err(create_failure(shared, error, "get_swapchain_images"));
            }
        };
        let mut render_finished = Vec::with_capacity(images.len());
        for _ in 0..images.len() {
            let semaphore = match unsafe {
                shared
                    .raw()
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            } {
                Ok(semaphore) => semaphore,
                Err(error) => {
                    unsafe {
                        for semaphore in &render_finished {
                            shared.raw().destroy_semaphore(*semaphore, None);
                        }
                        fn_.destroy_swapchain(raw, None);
                    }
                    return Err(create_failure(
                        shared,
                        error,
                        "create_semaphore(render_finished)",
                    ));
                }
            };
            render_finished.push(semaphore);
        }
        log_plan(&key, &formats);
        let swapchain = Self {
            device: shared.raw().clone(),
            raw,
            fn_,
            images,
            key,
            incremental: shared.caps().incremental_present,
            render_finished,
            live_swapchains,
            handles: DeferredHandles::default(),
        };
        swapchain.live_swapchains.note_created();
        Ok(swapchain)
    }

    pub(crate) fn leak(&mut self) {
        self.handles.leak();
    }

    pub(crate) fn raw(&self) -> vk::SwapchainKHR {
        self.raw
    }

    pub(crate) fn extent(&self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.key.extent.0,
            height: self.key.extent.1,
        }
    }

    pub(crate) fn key(&self) -> SwapchainKey {
        self.key
    }

    pub(crate) fn image_count(&self) -> usize {
        self.images.len()
    }

    pub(crate) fn image(&self, index: u32) -> vk::Image {
        self.images[index as usize]
    }

    pub(crate) fn acquire(
        &self,
        shared: &SharedGpu,
        image_available: vk::Semaphore,
    ) -> Result<Acquire, EngineError> {
        debug_assert!(on_gpu_thread(), "acquire 只能发生在 kytrix-vulkan 线程");
        debug_assert!(
            !image_available.is_null(),
            "image_available must be per frame slot"
        );
        let result = unsafe {
            self.fn_.acquire_next_image(
                self.raw,
                ACQUIRE_TIMEOUT_NS,
                image_available,
                vk::Fence::null(),
            )
        };
        match result {
            Ok((index, suboptimal)) => Ok(Acquire::Image { index, suboptimal }),
            Err(vk::Result::TIMEOUT) | Err(vk::Result::NOT_READY) => Ok(Acquire::NotReady),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok(Acquire::OutOfDate),
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Ok(Acquire::SurfaceLost),
            Err(error) => Err(shared.classify_failure(error, "acquire_next_image")),
        }
    }

    pub(crate) fn present(
        &self,
        shared: &SharedGpu,
        image_index: u32,
        dirty: Option<DamageRects>,
    ) -> Result<Present, EngineError> {
        debug_assert!(on_gpu_thread(), "present 只能发生在 kytrix-vulkan 线程");
        let swapchains = [self.raw];
        let indices = [image_index];
        let waits = [self.render_finished[image_index as usize]];
        let mut info = vk::PresentInfoKHR::default()
            .wait_semaphores(&waits)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let rects = dirty
            .filter(|rects| !rects.is_empty())
            .map(|rects| present_rects(&rects));
        let mut regions = [vk::PresentRegionKHR::default()];
        let mut regions_info = vk::PresentRegionsKHR::default();
        if self.incremental
            && let Some(rects) = rects.as_ref()
        {
            regions[0] = regions[0].rectangles(rects);
            regions_info = regions_info.regions(&regions);
            regions_info.swapchain_count = 1;
            info = info.push_next(&mut regions_info);
        }
        let result = shared.with_queue(|queue| unsafe { self.fn_.queue_present(queue, &info) });
        match result {
            Ok(false) => Ok(Present::Presented),
            Ok(true) => Ok(Present::Suboptimal),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok(Present::OutOfDate),
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Ok(Present::SurfaceLost),
            Err(error) => Err(shared.classify_failure(error, "queue_present")),
        }
    }

    pub(crate) fn render_finished(&self, image_index: u32) -> vk::Semaphore {
        self.render_finished[image_index as usize]
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        self.live_swapchains.note_destroyed();
        if self.handles.leaked() {
            return;
        }
        unsafe {
            for semaphore in &self.render_finished {
                self.device.destroy_semaphore(*semaphore, None);
            }
            self.fn_.destroy_swapchain(self.raw, None);
        }
    }
}

fn log_plan(key: &SwapchainKey, formats: &[vk::SurfaceFormatKHR]) {
    static LAST: Mutex<Option<SwapchainKey>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if last.as_ref() == Some(key) {
        return;
    }
    *last = Some(*key);
    let offered = formats
        .iter()
        .map(|format| (format.format, format.color_space))
        .collect::<Vec<_>>();
    eprintln!(
        "kytrix: vulkan swapchain extent={:?} format={:?} color_space={:?} present_mode={:?} composite_alpha={:?} transform={:?} images={} offered={offered:?}",
        key.extent,
        key.format,
        key.color_space,
        key.present_mode,
        key.composite_alpha,
        key.pre_transform,
        key.image_count
    );
}

fn present_rects(dirty: &DamageRects) -> Vec<vk::RectLayerKHR> {
    dirty
        .as_slice()
        .iter()
        .map(|rect| vk::RectLayerKHR {
            offset: vk::Offset2D {
                x: rect.x0.max(0),
                y: rect.y0.max(0),
            },
            extent: vk::Extent2D {
                width: rect.width().max(0) as u32,
                height: rect.height().max(0) as u32,
            },
            layer: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{CreateFailure, LiveSwapchains, recoverable_create_failure};
    use crate::{math::PhysicalRect, render::compositor::DamageRects};

    #[test]
    fn create_failures_classify_recoverable_wsi_state_and_leave_the_rest_to_the_shared_gpu() {
        assert!(matches!(
            recoverable_create_failure(vk::Result::ERROR_OUT_OF_DATE_KHR),
            Some(CreateFailure::OutOfDate)
        ));
        assert!(matches!(
            recoverable_create_failure(vk::Result::ERROR_SURFACE_LOST_KHR),
            Some(CreateFailure::SurfaceLost)
        ));
        for fatal in [
            vk::Result::ERROR_DEVICE_LOST,
            vk::Result::ERROR_UNKNOWN,
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
            vk::Result::SUCCESS,
        ] {
            assert!(
                recoverable_create_failure(fatal).is_none(),
                "{fatal:?} 不能当成可恢复的 WSI 状态（必须由 SharedGpu 分类成 Fatal）"
            );
        }
    }

    #[test]
    fn the_live_swapchain_counter_tracks_creation_and_destruction() {
        let live = LiveSwapchains::default();
        assert_eq!(live.count(), 0, "surface 新建时没有 swapchain");
        live.note_created();
        assert_eq!(live.count(), 1);
        live.note_created();
        assert_eq!(
            live.count(),
            2,
            "多条链（重建期间旧链仍在延迟队列）必须都计上"
        );
        live.note_destroyed();
        assert_eq!(live.count(), 1);
        live.note_destroyed();
        assert_eq!(live.count(), 0, "全部销毁之后才满足 recreate 的前置条件");
        live.debug_assert_idle();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "VUID-vkDestroySurfaceKHR-surface-01266")]
    fn recreating_a_surface_while_its_swapchain_is_alive_is_a_debug_error() {
        let live = LiveSwapchains::default();
        live.note_created();
        live.debug_assert_idle();
    }

    #[test]
    fn present_rects_mirror_the_copied_rectangles() {
        let mut dirty = DamageRects::new();
        assert!(dirty.push(PhysicalRect::new(4, 8, 12, 16)));
        assert!(dirty.push(PhysicalRect::new(0, 0, 1, 1)));
        let rects = super::present_rects(&dirty);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].offset, ash::vk::Offset2D { x: 4, y: 8 });
        assert_eq!(
            rects[0].extent,
            ash::vk::Extent2D {
                width: 8,
                height: 8
            }
        );
        assert_eq!(rects[1].offset, ash::vk::Offset2D { x: 0, y: 0 });
        assert_eq!(
            rects[1].extent,
            ash::vk::Extent2D {
                width: 1,
                height: 1
            }
        );
        let mut out_of_bounds = DamageRects::new();
        assert!(out_of_bounds.push(PhysicalRect::new(-3, -2, 1, 1)));
        let clamped = out_of_bounds.clamped(64, 64);
        let rects = super::present_rects(&clamped);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].offset, ash::vk::Offset2D { x: 0, y: 0 });
        assert_eq!(
            rects[0].extent,
            ash::vk::Extent2D {
                width: 1,
                height: 1
            }
        );
        assert_eq!(rects[0].layer, 0);
    }
}
