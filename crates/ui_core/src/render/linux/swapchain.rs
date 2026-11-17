//! Wayland WSI：每窗口 `VkSurfaceKHR` + swapchain / acquire / present
//! （`design/phase-12-macos-linux.md` §6.3 / §6.9、`design/async-render-pipeline.md` §7.2 / §7.4 / §7.6）。
//!
//! 实例是进程级的（[`VulkanInstance`]），swapchain / surface 是**每窗口**的：多窗口共享同一个
//! `VkDevice` 与同一条 graphics queue，但各自的 surface 与图像链互不相干。
//!
//! **没有「必须整面 copy」这回事**（R11-12）：present 之后 swapchain image 的内容对应用不可信，
//! 但每张 image 相对 persist 还差哪些矩形是**可以记账**的（`render::compositor::ImageCopyPlan`）。
//! 本模块只负责两件事：把「这次要拷哪些矩形 / 这次要 hint 哪些矩形」翻译成 `VkImageCopy` 与
//! `VkPresentRegionKHR`，且**两者必须一致**（hint 说没变、实际又写了，是 hint 语义内的浪费；
//! 反过来「hint 说要变、实际没写」则会让合成器保留旧像素）。
//!
//! 同步原语的所有权（§7.2）：
//! - `image_available` 在每窗口的 `FrameSync` 里**每帧槽一个**（acquire 时 signal、submit 时 wait）；
//! - `render_finished` 在**本结构里按 swapchain image 索引**（acquire 回同一张图之前，
//!   presentation engine 可能仍持有它 ⇒ 不能按帧槽索引）。
//!
//! 节奏（I3）：`vkAcquireNextImageKHR` 就是 Vulkan 的呈现节奏原语；超时用有限值（100 ms），
//! 只为在遮挡 / 合成器停摆时仍能响应 shutdown。阻塞只发生在 GPU 线程（§8.2）。
//!
//! 销毁（§7.4 / R11-3）：`vkDestroySwapchainKHR` 要求「所有从该 swapchain 获取的 image 的使用
//! 已完成」，`vkDestroySemaphore` 要求「引用该信号量的批次已完成」—— present 的 `waitSemaphores`
//! 也算引用。**队列 idle 是这两条的前置条件**，`GpuRenderer` 在销毁前必须做过一次
//! `SharedGpu::wait_queue_idle`（有界 2 s）。旧注释声称「`vkDeviceWaitIdle` 不能证明 presentation
//! engine 已释放图像，因此不作为释放依据」，并因此把释放挂在「新链某帧的 timeline」上 —— 那是
//! **更弱**的同步（两者之间没有任何依赖关系），也正是 R11-3 的成因。

use std::sync::{Arc, Mutex, PoisonError};

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

/// acquire 的有限超时（100 ms）：只用于响应 shutdown（§7.3 / §8.2）。
const ACQUIRE_TIMEOUT_NS: u64 = 100 * 1_000_000;

/// `vkAcquireNextImageKHR` 的结果（`NOT_READY` 不是错误：回循环检查 shutdown / 新帧）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Acquire {
    /// 拿到一张图；`suboptimal` 时本帧照常 present，之后重建 swapchain。
    Image { index: u32, suboptimal: bool },
    /// 超时没有可用图像：保持待画帧，回循环重试。
    NotReady,
    /// surface 与 swapchain 不匹配：重建后重试。
    OutOfDate,
    /// surface 永久失效（合成器重启 / 显卡复位）：**重建 `VkSurfaceKHR`** 之后重试（R11-11）。
    SurfaceLost,
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
    /// surface 永久失效：与 acquire 侧同一恢复路径（R11-11）。
    SurfaceLost,
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
    /// teardown 的兜底标记（AR3 §5.1 / P2-5）：`leaked()` 为真时 `Drop` 不销毁 surface。
    handles: DeferredHandles,
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
            handles: DeferredHandles::default(),
        })
    }

    /// teardown 兜底（AR3 §5.1 / P2-5）：不再销毁 `VkSurfaceKHR`（它的 swapchain 也没被销毁）。
    pub(crate) fn leak(&mut self) {
        self.handles.leak();
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
        // teardown 无法证明队列已 idle：句柄已交给进程级兜底（AR3 §5.1 / P2-5）。
        // `vkDestroySurfaceKHR` 要求它的 swapchain 全部先销毁（VUID-vkDestroySurfaceKHR-surface-01266），
        // 而那条链此刻只能在进程退出时回收。
        if self.handles.leaked() {
            return;
        }
        // 该 surface 的 swapchain 已先于本结构销毁（`GpuRenderer` 的字段顺序 / teardown 顺序）。
        self.gpu.instance().destroy_surface(self.raw);
    }
}

/// swapchain + **每 image 一个** render-finished 信号量 + 建链参数快照。
pub(crate) struct Swapchain {
    device: ash::Device,
    raw: vk::SwapchainKHR,
    fn_: ash::khr::swapchain::Device,
    images: Vec<vk::Image>,
    /// 一次成功建链的全部参数（诊断 + 重建必要性判断，R11-7 / R11-8）。
    key: SwapchainKey,
    /// `VK_KHR_incremental_present` **是否已启用**（不是「扩展可用」：R11-23）。
    incremental: bool,
    /// 长度 = `images.len()`：按 image index 索引（§7.2 硬规则 1）。
    render_finished: Vec<vk::Semaphore>,
    /// teardown 的兜底标记（AR3 §5.1 / P2-5）：`leaked()` 为真时 `Drop` 不销毁任何句柄。
    handles: DeferredHandles,
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
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效。
        let formats =
            unsafe { surface_fn.get_physical_device_surface_formats(shared.physical(), surface) }
                .map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!("get_physical_device_surface_formats: {error:?}"),
            })?;
        // R11-8 / R06-10：format 与 colorSpace 必须**同时**匹配引擎声明的呈现契约
        // （`render::colorspace`）。旧实现只看 format、把合成器给的 colorSpace 原样透传 ——
        // 在 HDR / P3 合成器上会把 sRGB 编码字节按 PQ/HLG 解释，整屏亮度与对比度错误。
        let chosen =
            choose_surface_format(&formats, crate::render::colorspace::accepts_color_space)
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
        let present_mode =
            choose_present_mode(&present_modes).ok_or_else(|| EngineError::UnsupportedGpu {
                os: "linux",
                required: "VK_PRESENT_MODE_MAILBOX_KHR on a Wayland surface",
                actual: smol_str::format_smolstr!("present modes {present_modes:?}"),
            })?;
        // R11-9：`currentExtent` 有两个合法形态（具体尺寸 / `0xFFFFFFFF`），且必须按
        // `preTransform` 换轴。旧实现既不用 currentExtent 也不换轴 ⇒ 驱动决定尺寸时永久
        // SUBOPTIMAL（并叠加 R11-7 变成每帧重建）。
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
        .ok_or_else(|| EngineError::UnsupportedGpu {
            os: "linux",
            required: "a non-degenerate surface extent range",
            actual: smol_str::format_smolstr!(
                "min={:?} max={:?} current={:?}",
                capabilities.min_image_extent,
                capabilities.max_image_extent,
                capabilities.current_extent
            ),
        })?;
        if !capabilities
            .supported_usage_flags
            .contains(plan::required_usage())
        {
            return Err(EngineError::UnsupportedGpu {
                os: "linux",
                required: "COLOR_ATTACHMENT | TRANSFER_DST swapchain images",
                actual: smol_str::format_smolstr!(
                    "surface supports {:?}",
                    capabilities.supported_usage_flags
                ),
            });
        }
        let composite_alpha = choose_composite_alpha(capabilities.supported_composite_alpha)
            .ok_or_else(|| EngineError::UnsupportedGpu {
                os: "linux",
                required: "at least one composite alpha mode",
                actual: smol_str::format_smolstr!(
                    "surface supports {:?}",
                    capabilities.supported_composite_alpha
                ),
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
        // SAFETY: `create` 的指针在本次调用期间有效；swapchain 在本结构 Drop（或调用方延迟 drop）
        // 时销毁；`VkQueue` 由共享 GPU 持有且在 `with_queue` 内串行化。
        let raw = unsafe { fn_.create_swapchain(&create, None) }.map_err(|error| {
            EngineError::Swapchain {
                detail: smol_str::format_smolstr!("create_swapchain_khr: {error:?}"),
            }
        })?;
        // SAFETY: `raw` 刚创建；image 句柄的存活期由 swapchain 保证。
        let images = match unsafe { fn_.get_swapchain_images(raw) } {
            Ok(images) => images,
            Err(error) => {
                // 失败路径不泄漏：swapchain 已创建，必须销毁（R11-29 的同族问题）。
                // SAFETY: 没有任何 image / 信号量引用它。
                unsafe { fn_.destroy_swapchain(raw, None) };
                return Err(EngineError::Swapchain {
                    detail: smol_str::format_smolstr!("get_swapchain_images: {error:?}"),
                });
            }
        };
        // 每 image 一个 render-finished（§7.2 硬规则 1：不能按帧槽索引）。
        let mut render_finished = Vec::with_capacity(images.len());
        for _ in 0..images.len() {
            // SAFETY: 句柄归本结构独占；`Drop` 里销毁（调用方保证队列已 idle）。
            let semaphore = match unsafe {
                shared
                    .raw()
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            } {
                Ok(semaphore) => semaphore,
                Err(error) => {
                    // 已创建的信号量与 swapchain 就地回收（失败路径不泄漏）。
                    // SAFETY: 没有在飞批次引用它们（swapchain 从未 present 过）。
                    unsafe {
                        for semaphore in &render_finished {
                            shared.raw().destroy_semaphore(*semaphore, None);
                        }
                        fn_.destroy_swapchain(raw, None);
                    }
                    return Err(EngineError::Swapchain {
                        detail: smol_str::format_smolstr!(
                            "create_semaphore(render_finished): {error:?}"
                        ),
                    });
                }
            };
            render_finished.push(semaphore);
        }
        log_plan(&key, &formats);
        Ok(Self {
            device: shared.raw().clone(),
            raw,
            fn_,
            images,
            key,
            // 「已启用」而不是「可用」：`VkPresentRegionsKHR` 只有在扩展已启用时才合法（R11-23）。
            incremental: shared.caps().incremental_present,
            render_finished,
            handles: DeferredHandles::default(),
        })
    }

    /// teardown 兜底（AR3 §5.1 / P2-5）：不再销毁本链与它的 render-finished 信号量。
    ///
    /// 调用方（`GpuRenderer::drop`）在无法证明队列已 idle 时调用：`vkDestroySwapchainKHR` 要求
    /// presentation engine 已释放该链（VUID-…-01282），`vkDestroySemaphore` 要求相关提交已完成。
    /// 句柄留给 `SharedGpu` 的 `vkDestroyDevice` 回收。
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

    /// 一次成功建链的全部参数（诊断 + 重建必要性判断）。
    pub(crate) fn key(&self) -> SwapchainKey {
        self.key
    }

    /// swapchain 的 image 数（每 image 的同步状态表按它分配）。
    pub(crate) fn image_count(&self) -> usize {
        self.images.len()
    }

    pub(crate) fn image(&self, index: u32) -> vk::Image {
        self.images[index as usize]
    }

    /// acquire：`image_available` 由调用方按帧槽给出（§7.2）。
    ///
    /// `surface_lost` 与 `out_of_date` 分开上报（R11-11）：前者要重建 `VkSurfaceKHR`，后者只要
    /// 重建 swapchain。旧实现把两者合并成 `OutOfDate`，于是「合成器重启」这条路径永远走不到
    /// surface 恢复逻辑，窗口黑屏到 UI 上报错误。
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
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Ok(Acquire::SurfaceLost),
            // §7.7：设备丢失统一走 `SharedGpu` 的粘性分类（诊断文件只有一个出口）。
            Err(error) => Err(shared.classify_failure(error, "acquire_next_image")),
        }
    }

    /// present：等**本 image** 的 render-finished；`incremental_present` 的 region 必须与本次
    /// 实际拷贝的矩形一致（R11-12）。
    ///
    /// `dirty` 是本次真正写进这张 image 的矩形（`None` = 整面 / 没有 hint）：它同时是
    /// `VkPresentRegionKHR` 的内容与「这一帧搬了多少像素」的事实来源。
    ///
    /// **唯一的例外**（AR3 §5.3）：本帧一个像素都没搬（damage 全落在 `imageExtent` 之外）时，
    /// 调用方传的是**上一次**的 hint —— 扩展没有「什么都没变」的编码（空 region 数组 = 整面），
    /// 声称整张 image 都变了只会让合成器白重合成一遍。两种情况下 region 都落在 image 内、
    /// 且都不小于「实际变化」，所以合成结果都正确。
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
        // SAFETY: `info` 的指针指向本函数栈上的数组；`VkQueue` 由共享 GPU 持有，主机访问在
        // `with_queue` 内外部同步。
        let result = shared.with_queue(|queue| unsafe { self.fn_.queue_present(queue, &info) });
        match result {
            Ok(false) => Ok(Present::Presented),
            Ok(true) => Ok(Present::Suboptimal),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok(Present::OutOfDate),
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Ok(Present::SurfaceLost),
            Err(error) => Err(shared.classify_failure(error, "queue_present")),
        }
    }

    /// 本 image 的 render-finished 信号量（提交时 signal）。
    pub(crate) fn render_finished(&self, image_index: u32) -> vk::Semaphore {
        self.render_finished[image_index as usize]
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // teardown 无法证明队列已 idle：句柄已交给进程级兜底（AR3 §5.1 / P2-5）。
        if self.handles.leaked() {
            return;
        }
        // SAFETY: 调用方保证**队列已 idle**（`SharedGpu::wait_queue_idle`，R11-3）：这是
        // `vkDestroySwapchainKHR` / `vkDestroySemaphore` 对 presentation engine 与在飞批次的
        // 共同前置条件。旧实现等的是「新链某一帧的 timeline」，与旧链的 present 之间没有任何
        // 依赖关系，因此不构成保证。
        unsafe {
            for semaphore in &self.render_finished {
                self.device.destroy_semaphore(*semaphore, None);
            }
            self.fn_.destroy_swapchain(self.raw, None);
        }
    }
}

/// 建链参数的诊断行：**只在参数变化时打印**（拖动改窗口尺寸时不会刷屏），
/// 但把每一次变化的全部字段都写出来（R11-7 的「最近一次重建原因」、R11-8 的 colorSpace）。
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

/// `incremental_present` 的矩形。
///
/// **入参必须已经用 `DamageRects::clamped(extent)` 夹过**（`present_hint` 就是那份集合）：
/// `VkRectLayerKHR` 不允许负 offset、也不允许超出 image 尺寸（`VUID-VkRectLayerKHR-*`），
/// 而夹紧只在 `copy_to_swapchain` 里做一次 —— hint 与实际拷贝的矩形因此逐字一致（R11-12）。
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
    use crate::{math::PhysicalRect, render::compositor::DamageRects};

    #[test]
    fn present_rects_mirror_the_copied_rectangles() {
        // 入参是 `copy_to_swapchain` 夹过之后的集合（hint 必须与**实际拷贝**的矩形一致，
        // R11-12）；夹紧本身在共享层 `DamageRects::clamped` 的单测里验证。
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
        // 出界矩形被夹紧之后只剩落在 image 内的那一块 —— hint 不能声称写了没写的像素。
        let mut out_of_bounds = DamageRects::new();
        assert!(out_of_bounds.push(PhysicalRect::new(-3, -2, 1, 1)));
        let clamped = out_of_bounds.clamped(64, 64);
        let rects = super::present_rects(&clamped);
        assert_eq!(rects.len(), 1);
        // `VkRectLayerKHR` 没有 `PartialEq`：逐字段比较。
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
