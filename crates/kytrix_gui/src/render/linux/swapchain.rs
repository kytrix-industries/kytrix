//! Wayland WSI：swapchain / acquire / present（`design/phase-12-macos-linux.md` §6.3 / §6.9）。
//!
//! **无局部 present**：swapchain image 的内容 present 后未定义 ⇒ 每帧整面 `cmd_copy_image`；
//! `VK_KHR_incremental_present` 只作为 `pNext` hint（引擎不依赖）。

use ash::vk;

use crate::{
    error::EngineError,
    render::{
        compositor::{FramePlan, Plan},
        linux::{device::Device, instance::VulkanInstance},
    },
};

/// 同时存在的帧数（与 `MAX_FRAMES_IN_FLIGHT` 对齐）。
const FRAMES: usize = crate::render::frame::MAX_FRAMES_IN_FLIGHT;

/// swapchain + 每帧的 render-finished 信号量。
pub(crate) struct Swapchain {
    raw: vk::SwapchainKHR,
    fn_: ash::khr::swapchain::Device,
    images: Vec<vk::Image>,
    format: vk::Format,
    extent: vk::Extent2D,
    incremental: bool,
    fresh: bool,
    acquire: vk::Semaphore,
    render_finished: [vk::Semaphore; FRAMES],
}

impl Swapchain {
    pub(crate) fn new(
        instance: &VulkanInstance,
        device: &Device,
        size: (u32, u32),
    ) -> Result<Self, EngineError> {
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let formats = unsafe {
            instance
                .surface_fn()
                .get_physical_device_surface_formats(device.physical(), instance.surface())
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!("get_physical_device_surface_formats: {error:?}"),
        })?;
        let chosen = formats
            .iter()
            .find(|format| format.format == vk::Format::B8G8R8A8_SRGB)
            .or_else(|| {
                formats
                    .iter()
                    .find(|format| format.format == vk::Format::R8G8B8A8_SRGB)
            })
            .copied()
            .ok_or_else(|| EngineError::UnsupportedGpu {
                os: "linux",
                required: "B8G8R8A8_SRGB or R8G8B8A8_SRGB surface format",
                actual: smol_str::SmolStr::new_static("surface reports no *_SRGB format"),
            })?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let capabilities = unsafe {
            instance
                .surface_fn()
                .get_physical_device_surface_capabilities(device.physical(), instance.surface())
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "get_physical_device_surface_capabilities: {error:?}"
            ),
        })?;
        let extent = vk::Extent2D {
            width: size.0.max(1),
            height: size.1.max(1),
        };
        let image_count =
            (capabilities.min_image_count + 1)
                .max(3)
                .min(if capabilities.max_image_count == 0 {
                    u32::MAX
                } else {
                    capabilities.max_image_count
                });
        let fn_ = ash::khr::swapchain::Device::new(instance.raw(), device.raw());
        let create = vk::SwapchainCreateInfoKHR::default()
            .surface(instance.surface())
            .min_image_count(image_count)
            .image_format(chosen.format)
            .image_color_space(chosen.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true);
        // SAFETY: `create` 的指针在本次调用期间有效；swapchain 在本结构 Drop 时销毁。
        let raw = unsafe { fn_.create_swapchain(&create, None) }.map_err(|error| {
            EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_swapchain_khr: {error:?}"),
            }
        })?;
        // SAFETY: `raw` 刚创建；image 句柄的存活期由 swapchain 保证。
        let images =
            unsafe { fn_.get_swapchain_images(raw) }.map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!("get_swapchain_images: {error:?}"),
            })?;

        // SAFETY: 信号量归本结构独占；acquire / present 各用一次。
        let acquire = unsafe {
            device
                .raw()
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!("create_semaphore(acquire): {error:?}"),
        })?;
        let mut render_finished = [vk::Semaphore::null(); FRAMES];
        for semaphore in render_finished.iter_mut() {
            // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
            *semaphore = unsafe {
                device
                    .raw()
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_semaphore(render): {error:?}"),
            })?;
        }

        Ok(Self {
            raw,
            fn_,
            images,
            format: chosen.format,
            extent,
            incremental: device.caps().incremental_present,
            fresh: true,
            acquire,
            render_finished,
        })
    }

    pub(crate) fn format(&self) -> vk::Format {
        self.format
    }

    pub(crate) fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub(crate) fn image(&self, index: u32) -> vk::Image {
        self.images[index as usize]
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    pub(crate) fn presented(&mut self) {
        self.fresh = false;
    }

    /// acquire：`OUT_OF_DATE` / suboptimal 都置 `fresh`（本帧仍可能提交）。
    pub(crate) fn acquire(&mut self, device: &Device, frame: u64) -> Result<u32, EngineError> {
        // SAFETY: swapchain / 信号量归本结构独占；等待无限（FIFO 下不丢帧）。
        let result = unsafe {
            self.fn_
                .acquire_next_image(self.raw, u64::MAX, self.acquire, vk::Fence::null())
        };
        let _ = frame;
        match result {
            Ok((index, suboptimal)) => {
                if suboptimal {
                    self.fresh = true;
                }
                Ok(index)
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.fresh = true;
                Ok(u32::MAX)
            }
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Err(EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static("VK_ERROR_SURFACE_LOST_KHR"),
            }),
            Err(error) => Err(device_lost_or_swapchain(device, error)),
        }
    }

    /// present：等 render-finished，`incremental_present` 只在有 dirty 集时挂 `pNext`。
    pub(crate) fn present(
        &mut self,
        device: &Device,
        slot: usize,
        image_index: u32,
        frame: FramePlan,
    ) -> Result<(), EngineError> {
        let swapchains = [self.raw];
        let indices = [image_index];
        let waits = [self.render_finished[slot]];
        let mut info = vk::PresentInfoKHR::default()
            .wait_semaphores(&waits)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let rects = present_rects(frame);
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
        // SAFETY: 队列由 device 提供；`info` 的指针指向本函数栈上的数组。
        let result = unsafe { self.fn_.queue_present(device.queue(), &info) };
        match result {
            Ok(_) => {
                self.presented();
                Ok(())
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.fresh = true;
                Ok(())
            }
            Err(error) => Err(device_lost_or_swapchain(device, error)),
        }
    }

    /// resize：重建 swapchain（旧的立即销毁；同步提交下 GPU 已空闲）。
    pub(crate) fn resize(
        &mut self,
        instance: &VulkanInstance,
        device: &Device,
        size: (u32, u32),
    ) -> Result<(), EngineError> {
        // 旧 swapchain 不能在使用中被销毁：resize 是罕见路径，直接等队列空闲
        // （`Renderer::resize` 已先 `wait_idle`，OUT_OF_DATE 路径没有 ⇒ 这里兜住）。
        // SAFETY: 队列由本结构所属的设备创建。
        unsafe {
            device
                .raw()
                .queue_wait_idle(device.queue())
                .map_err(|error| EngineError::Swapchain {
                    detail: smol_str::format_smolstr!("queue_wait_idle(resize): {error:?}"),
                })?;
        }
        let old = self.raw;
        let mut replacement = Self::new(instance, device, size)?;
        // 先把新句柄/图像取出来，再把 replacement 置空 —— 顺序反了会把 `null` 写进 `self`
        // （旧的 Drop 仍会销毁旧句柄，这里显式销毁）。
        let new_raw = replacement.raw;
        let new_images = std::mem::take(&mut replacement.images);
        replacement.raw = vk::SwapchainKHR::null();
        // SAFETY: 旧 swapchain 不再被使用（`Renderer::resize` 先 `wait_idle`）。
        unsafe { self.fn_.destroy_swapchain(old, None) };
        let old_acquire = std::mem::replace(&mut self.acquire, replacement.acquire);
        let old_render = std::mem::replace(&mut self.render_finished, replacement.render_finished);
        // SAFETY: 旧信号量只被旧 swapchain 使用。
        unsafe {
            device.raw().destroy_semaphore(old_acquire, None);
            for semaphore in old_render {
                device.raw().destroy_semaphore(semaphore, None);
            }
        }
        self.raw = new_raw;
        self.images = new_images;
        self.format = replacement.format;
        self.extent = replacement.extent;
        self.fresh = true;
        Ok(())
    }

    pub(crate) fn wait_semaphore(&self) -> vk::Semaphore {
        self.acquire
    }

    pub(crate) fn render_finished(&self, slot: usize) -> vk::Semaphore {
        self.render_finished[slot]
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // `resize` 会把被替换下来的句柄置空（句柄已交给新结构）。
        if self.raw == vk::SwapchainKHR::null() {
            return;
        }
        // SAFETY: 句柄归本结构独占；`Renderer::drop` 先 `wait_idle`。
        unsafe {
            self.fn_.destroy_swapchain(self.raw, None);
        }
    }
}

fn device_lost_or_swapchain(device: &Device, error: vk::Result) -> EngineError {
    if error == vk::Result::ERROR_DEVICE_LOST {
        EngineError::DeviceLost {
            reason: smol_str::SmolStr::new_static("VK_ERROR_DEVICE_LOST"),
            log_path: std::env::temp_dir().join(format!("kytrix-vk-{}.txt", std::process::id())),
        }
    } else {
        let _ = device;
        EngineError::Swapchain {
            detail: smol_str::format_smolstr!("{error:?}"),
        }
    }
}

/// `incremental_present` 的矩形（只在 partial 且非 fresh 时给）。
fn present_rects(frame: FramePlan) -> Option<Vec<vk::RectLayerKHR>> {
    if matches!(frame.plan(), Plan::Idle | Plan::Full) {
        return None;
    }
    let dirty = frame.present_dirty()?;
    Some(
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
            .collect(),
    )
}
