//! Wayland WSI：每窗口 `VkSurfaceKHR` + swapchain / acquire / present
//! （`design/phase-12-macos-linux.md` §6.3 / §6.9、`design/async-render-pipeline.md` §7.2 / §7.4 / §7.6）。
//!
//! 实例是进程级的（[`VulkanInstance`]），swapchain / surface 是**每窗口**的：多窗口共享同一个
//! `VkDevice` 与同一条 graphics queue，但各自的 surface 与图像链互不相干。
//!
//! **无局部 present**：swapchain image 的内容 present 后未定义 ⇒ 每帧整面 `cmd_copy_image`；
//! `VK_KHR_incremental_present` 只作为 `pNext` hint（引擎不依赖）。整面 copy 要求 persist 与
//! swapchain image 同格式，因此 swapchain 固定要求 [`RENDER_FORMAT`]（不做格式回退）。
//!
//! 同步原语的所有权（§7.2）：
//! - `image_available` 在每窗口的 `FrameSync` 里**每帧槽一个**（acquire 时 signal、submit 时 wait）；
//! - `render_finished` 在**本结构里按 swapchain image 索引**（acquire 回同一张图之前，
//!   presentation engine 可能仍持有它 ⇒ 不能按帧槽索引）。
//!
//! 节奏（I3）：`vkAcquireNextImageKHR` 就是 Vulkan 的呈现节奏原语；超时用有限值（100 ms），
//! 只为在遮挡 / 合成器停摆时仍能响应 shutdown。阻塞只发生在 GPU 线程（§8.2）。
//!
//! 销毁（§7.4）：本结构没有「重建」入口。重建由 `GpuRenderer` 用 `oldSwapchain` 创建新结构完成，
//! 旧结构留在延迟销毁队列里，等新 swapchain 成功 present 至少一帧且该帧完成之后才 drop
//! （`vkDeviceWaitIdle` **不能**证明 presentation engine 已释放图像，因此不作为释放依据）。
//!
//! `VkQueue` 的主机访问来自多个窗口线程（present），因此经 `SharedGpu::with_queue` 串行化
//! （Vulkan 要求外部同步）。

use std::sync::Arc;

use ash::vk::{self, Handle};

use crate::{
    error::EngineError,
    render::{
        compositor::{FramePlan, Plan},
        linux::{
            device::{SharedGpu, on_gpu_thread},
            pipeline::RENDER_FORMAT,
        },
    },
};

/// acquire 的有限超时（100 ms）：只用于响应 shutdown（§7.3 / §8.2）。
const ACQUIRE_TIMEOUT_NS: u64 = 100 * 1_000_000;

/// Wayland surface **必须**报告 `MAILBOX`（WSI 规范；§7.6）：最新帧替换待呈现帧，与最新帧槽同构。
const PRESENT_MODE: vk::PresentModeKHR = vk::PresentModeKHR::MAILBOX;

/// `vkAcquireNextImageKHR` 的结果（`NOT_READY` 不是错误：回循环检查 shutdown / 新帧）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Acquire {
    /// 拿到一张图；`suboptimal` 时本帧照常 present，之后重建 swapchain。
    Image { index: u32, suboptimal: bool },
    /// 超时没有可用图像：保持待画帧，回循环重试。
    NotReady,
    /// surface 与 swapchain 不匹配：重建后重试。
    OutOfDate,
}

/// `vkQueuePresentKHR` 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Present {
    /// 已交给 presentation engine。
    Presented,
    /// 呈现本身成功，但 surface 已不再最优：本帧照常，之后重建。
    Suboptimal,
    /// 本帧没能呈现：重建 swapchain 后重试。
    OutOfDate,
}

/// 一个窗口的 `VkSurfaceKHR`（存活期 = 窗口渲染器）。
///
/// 持有 `Arc<SharedGpu>`：surface 的销毁要求 `VkInstance` 还活着，而实例由共享 GPU 保活。
/// 引用计数上这同时保证「最后一个窗口关闭时才销毁 instance」，销毁顺序固定为
/// swapchain → surface → device → instance。
pub(crate) struct Surface {
    gpu: Arc<SharedGpu>,
    raw: vk::SurfaceKHR,
    /// 平台句柄：`VK_ERROR_SURFACE_LOST_KHR` 之后必须用它们重建 `VkSurfaceKHR`。
    ///
    /// 所有权仍属平台层（`WaylandState`）：渲染器 drop 早于 `wl_surface` 销毁（V-122 ⑨），
    /// 所以这里只借不持有。
    display: *mut std::ffi::c_void,
    wl_surface: *mut std::ffi::c_void,
}

impl Surface {
    /// 用平台层的 `wl_display*` / `wl_surface*` 建 surface（同一进程级实例）。
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
        })
    }

    pub(crate) fn raw(&self) -> vk::SurfaceKHR {
        self.raw
    }

    /// `VK_ERROR_SURFACE_LOST_KHR` 之后用同一对 wl 句柄重建 surface。
    ///
    /// 调用方保证该 surface 的 swapchain 已经销毁（`replace_swapchain` 的失败路径先置空
    /// swapchain 再调本函数）。
    pub(crate) fn recreate(&mut self) -> Result<(), EngineError> {
        let raw = self
            .gpu
            .instance()
            .create_surface(self.display, self.wl_surface)?;
        let old = std::mem::replace(&mut self.raw, raw);
        self.gpu.instance().destroy_surface(old);
        Ok(())
    }
}

// SAFETY: 两个平台句柄只在 GPU 线程上用于重建 `VkSurfaceKHR`（`Surface::recreate`）。
// wl 对象的**所有权**仍属平台层（`WaylandState`），渲染器 drop 早于它们（V-122 ⑨）；
// 这里只跨越线程边界传递指针值，与 `vkCreateWaylandSurfaceKHR` 的常规用法一致。
unsafe impl Send for Surface {}

impl Drop for Surface {
    fn drop(&mut self) {
        // 该 surface 的 swapchain 已先于本结构销毁（`GpuRenderer` 的字段顺序 / teardown 顺序）。
        self.gpu.instance().destroy_surface(self.raw);
    }
}

/// swapchain + **每 image 一个** render-finished 信号量。
pub(crate) struct Swapchain {
    device: ash::Device,
    raw: vk::SwapchainKHR,
    fn_: ash::khr::swapchain::Device,
    images: Vec<vk::Image>,
    extent: vk::Extent2D,
    incremental: bool,
    /// 长度 = `images.len()`：按 image index 索引（§7.2 硬规则 1）。
    render_finished: Vec<vk::Semaphore>,
}

impl Swapchain {
    /// 首次创建（窗口初始化）。
    pub(crate) fn new(
        shared: &SharedGpu,
        surface: vk::SurfaceKHR,
        size: (u32, u32),
    ) -> Result<Self, EngineError> {
        Self::create(shared, surface, size, vk::SwapchainKHR::null())
    }

    /// `old` 必须来自同一个 surface：新 swapchain 通过 `oldSwapchain` 接管 surface
    /// （先建后销毁会报 `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`，F-148）。格式恒为
    /// [`RENDER_FORMAT`]，重建不改变渲染管线 / persist 的格式。
    ///
    /// 规范：即使创建失败，旧 swapchain 也已 retired、必须销毁 —— 调用方负责把它放进延迟销毁队列。
    pub(crate) fn replace(
        shared: &SharedGpu,
        surface: vk::SurfaceKHR,
        size: (u32, u32),
        old: vk::SwapchainKHR,
    ) -> Result<Self, EngineError> {
        Self::create(shared, surface, size, old)
    }

    fn create(
        shared: &SharedGpu,
        surface: vk::SurfaceKHR,
        size: (u32, u32),
        old: vk::SwapchainKHR,
    ) -> Result<Self, EngineError> {
        let instance = shared.instance();
        let surface_fn = instance
            .surface_fn()
            .ok_or_else(|| EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static("swapchain requires a Wayland surface"),
            })?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let formats =
            unsafe { surface_fn.get_physical_device_surface_formats(shared.physical(), surface) }
                .map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!("get_physical_device_surface_formats: {error:?}"),
            })?;
        // 渲染目标格式是进程级固定的（PSO / render pass / persist 都按它建）：surface 必须提供
        // 同一格式，否则整面 `cmdCopyImage` 不合法（B8G8R8A8_SRGB 与 R8G8B8A8_SRGB 不兼容）。
        // R06-10：`colorSpace` 必须与引擎声明的呈现色彩空间一致（`render::colorspace`）。
        // 旧实现只查 format、把合成器给的 `colorSpace` 原样透传 —— 在 HDR/P3 合成器上会整屏偏色。
        // 与 `MAILBOX` 的处理一致：规范要求的硬前提，不满足就按 D8 基线失败，不做静默回退。
        let chosen = formats
            .iter()
            .find(|candidate| {
                candidate.format == RENDER_FORMAT
                    && crate::render::colorspace::accepts_color_space(candidate.color_space)
            })
            .copied()
            .ok_or_else(|| EngineError::UnsupportedGpu {
                os: "linux",
                required: "B8G8R8A8_SRGB surface format with VK_COLOR_SPACE_SRGB_NONLINEAR_KHR",
                actual: smol_str::format_smolstr!(
                    "surface reports {:?}",
                    formats
                        .iter()
                        .map(|format| (format.format, format.color_space))
                        .collect::<Vec<_>>()
                ),
            })?;
        // SAFETY: 只读查询；surface 由同一实例创建。
        let capabilities = unsafe {
            surface_fn.get_physical_device_surface_capabilities(shared.physical(), surface)
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "get_physical_device_surface_capabilities: {error:?}"
            ),
        })?;
        // SAFETY: 只读查询。
        let present_modes = unsafe {
            surface_fn.get_physical_device_surface_present_modes(shared.physical(), surface)
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "get_physical_device_surface_present_modes: {error:?}"
            ),
        })?;
        if !present_modes.contains(&PRESENT_MODE) {
            // Wayland surface 必须报告 MAILBOX（§7.6）：不报告说明不是规范的 Wayland WSI，不做 FIFO 回退。
            return Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: "VK_PRESENT_MODE_MAILBOX_KHR on a Wayland surface",
                actual: smol_str::format_smolstr!("present modes {present_modes:?}"),
            });
        }
        // Wayland 的 buffer 尺寸由客户端决定（合成器按 configure 约束），但 **Vulkan 仍要求**
        // `imageExtent` 落在 `minImageExtent..=maxImageExtent` 内：请求超出上界时
        // `vkCreateSwapchainKHR` 直接返回 `ERROR_INITIALIZATION_FAILED`（大窗口 / 特殊合成器上
        // 会命中）。因此这里夹紧，代价是超过上界时内容被 viewport 略微缩小（仍按逻辑尺寸
        // 布局，只是 pixel 密度下降），比建链失败好得多。
        let requested = vk::Extent2D {
            width: size.0.max(1),
            height: size.1.max(1),
        };
        let extent = vk::Extent2D {
            width: requested.width.clamp(
                capabilities.min_image_extent.width,
                capabilities.max_image_extent.width,
            ),
            height: requested.height.clamp(
                capabilities.min_image_extent.height,
                capabilities.max_image_extent.height,
            ),
        };
        // usage 必须被 `supported_usage_flags` 覆盖，否则同样是 INITIALIZATION_FAILED。
        // 直通 copy 需要 TRANSFER_DST、渲染需要 COLOR_ATTACHMENT —— 两者缺一就明确报错，
        // 而不是留给驱动吐一个无信息量的失败。
        let required_usage =
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST;
        if !capabilities.supported_usage_flags.contains(required_usage) {
            return Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: "COLOR_ATTACHMENT | TRANSFER_DST swapchain images",
                actual: smol_str::format_smolstr!(
                    "surface supports {:?}",
                    capabilities.supported_usage_flags
                ),
            });
        }
        // composite alpha 只能从 `supported_composite_alpha` 里挑：多数合成器只给
        // OPAQUE/INHERIT，硬写 OPAQUE 在某些环境（如嵌套合成器）会被拒绝。
        let composite_alpha = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::INHERIT,
        ]
        .into_iter()
        .find(|candidate| capabilities.supported_composite_alpha.contains(*candidate))
        .ok_or_else(|| EngineError::UnsupportedGpu {
            os: "linux",
            required: "at least one composite alpha mode",
            actual: smol_str::format_smolstr!(
                "surface supports {:?}",
                capabilities.supported_composite_alpha
            ),
        })?;
        let image_count =
            (capabilities.min_image_count + 1)
                .max(3)
                .min(if capabilities.max_image_count == 0 {
                    u32::MAX
                } else {
                    capabilities.max_image_count
                });
        let fn_ = ash::khr::swapchain::Device::new(instance.raw(), shared.raw());
        let create = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .old_swapchain(old)
            .min_image_count(image_count)
            .image_format(chosen.format)
            .image_color_space(chosen.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(required_usage)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(composite_alpha)
            .present_mode(PRESENT_MODE)
            .clipped(true);
        // SAFETY: `create` 的指针在本次调用期间有效；swapchain 在本结构 Drop（或调用方延迟 drop）
        // 时销毁；`VkQueue` 由共享 GPU 持有且在 `with_queue` 内串行化。
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

        // 每 image 一个 render-finished（§7.2 硬规则 1：不能按帧槽索引）。
        let mut render_finished = Vec::with_capacity(images.len());
        for _ in 0..images.len() {
            // SAFETY: 句柄归本结构独占；`Drop` 里销毁（调用方保证 presentation engine 已释放它们）。
            let semaphore = unsafe {
                shared
                    .raw()
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_semaphore(render_finished): {error:?}"),
            })?;
            render_finished.push(semaphore);
        }

        Ok(Self {
            device: shared.raw().clone(),
            raw,
            fn_,
            images,
            extent,
            incremental: shared.caps().incremental_present,
            render_finished,
        })
    }

    pub(crate) fn raw(&self) -> vk::SwapchainKHR {
        self.raw
    }

    pub(crate) fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub(crate) fn image(&self, index: u32) -> vk::Image {
        self.images[index as usize]
    }

    /// acquire：`image_available` 由调用方按帧槽给出（§7.2）。
    pub(crate) fn acquire(&self, image_available: vk::Semaphore) -> Result<Acquire, EngineError> {
        debug_assert!(on_gpu_thread(), "acquire 只能发生在 kytrix-vulkan 线程");
        debug_assert!(
            !image_available.is_null(),
            "image_available must be per frame slot"
        );
        // SAFETY: swapchain / 信号量归调用方所有；有限超时保证 shutdown 有界。
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
            // 超时：信号量不会被 signal（规范），帧请求保留着重试。
            Err(vk::Result::TIMEOUT) | Err(vk::Result::NOT_READY) => Ok(Acquire::NotReady),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok(Acquire::OutOfDate),
            // §7.7：surface lost 也走 §7.4 的重建路径（重建会用 `oldSwapchain`；没有新的
            // `wl_surface` 时创建会失败，并以强类型 `EngineError::Swapchain` 冒泡出去）。
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Ok(Acquire::OutOfDate),
            Err(error) => Err(classify(error)),
        }
    }

    /// present：等**本 image** 的 render-finished，`incremental_present` 只在有 dirty 集时挂 `pNext`。
    pub(crate) fn present(
        &self,
        shared: &SharedGpu,
        image_index: u32,
        frame: FramePlan,
    ) -> Result<Present, EngineError> {
        debug_assert!(on_gpu_thread(), "present 只能发生在 kytrix-vulkan 线程");
        let swapchains = [self.raw];
        let indices = [image_index];
        let waits = [self.render_finished[image_index as usize]];
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
        // SAFETY: `info` 的指针指向本函数栈上的数组；`VkQueue` 由共享 GPU 持有，主机访问在
        // `with_queue` 内外部同步。
        let result = shared.with_queue(|queue| unsafe { self.fn_.queue_present(queue, &info) });
        match result {
            Ok(false) => Ok(Present::Presented),
            Ok(true) => Ok(Present::Suboptimal),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok(Present::OutOfDate),
            Err(error) => Err(classify(error)),
        }
    }

    /// 本 image 的 render-finished 信号量（提交时 signal）。
    pub(crate) fn render_finished(&self, image_index: u32) -> vk::Semaphore {
        self.render_finished[image_index as usize]
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: 调用方保证 presentation engine 已不再使用本 swapchain 的图像与信号量
        // （退出路径先等本窗口 timeline；重建路径等新 swapchain 的首帧完成，§7.4）。
        unsafe {
            for semaphore in &self.render_finished {
                self.device.destroy_semaphore(*semaphore, None);
            }
            self.fn_.destroy_swapchain(self.raw, None);
        }
    }
}

fn classify(error: vk::Result) -> EngineError {
    if error == vk::Result::ERROR_DEVICE_LOST {
        EngineError::DeviceLost {
            reason: smol_str::SmolStr::new_static("VK_ERROR_DEVICE_LOST"),
            log_path: std::env::temp_dir().join(format!("kytrix-vk-{}.txt", std::process::id())),
        }
    } else {
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
