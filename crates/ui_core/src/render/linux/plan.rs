//! swapchain / 设备选择的**纯决策**（R11-7 / R11-8 / R11-9 / R11-13 / R11-15）。
//!
//! 本模块不碰任何 Vulkan 句柄：输入是 `vkGetPhysicalDeviceSurface*` 的**查询结果**（值类型），
//! 输出是「该建什么链 / 该不该重建 / 该选哪张卡」。放在一起的唯一理由：这四类判断互为前提
//! （extent 依赖 transform 与 caps，重建必要性依赖一次成功建链的全部参数，选卡依赖能不能建链），
//! 分散到 `swapchain.rs` / `device.rs` 里就必然出现两份不一致的判断（旧实现正是如此：
//! `swapchain.rs` 判 format / present mode，`device.rs` 只看 `device_type` 打分）。
//!
//! 每一项都在本文件底部有单元测试（Linux 目标：`cargo test -p ui_core --all-features`）。

use smol_str::SmolStr;

use crate::render::{frame::MAX_FRAMES_IN_FLIGHT, linux::pipeline::RENDER_FORMAT};

/// swapchain 建链的全部参数（**一次成功建链的事实**）。
///
/// 两个用途：
/// 1. `GpuCaps::describe()` 把它写进诊断行（旧实现完全不记录 swapchain 的格式 / 色彩空间，
///    「HDR 合成器上整屏偏色」只能靠猜）；
/// 2. `SUBOPTIMAL` / `OUT_OF_DATE` 的重建必要性判断（R11-7）：只有这些字段真的变了才重建，
///    否则会退化成「每帧重建」自旋。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SwapchainKey {
    pub extent: (u32, u32),
    pub format: ash::vk::Format,
    pub color_space: ash::vk::ColorSpaceKHR,
    pub present_mode: ash::vk::PresentModeKHR,
    pub composite_alpha: ash::vk::CompositeAlphaFlagsKHR,
    pub pre_transform: ash::vk::SurfaceTransformFlagsKHR,
    pub image_count: u32,
}

/// `currentExtent` 的「应用决定尺寸」哨兵值（`0xFFFFFFFF`）。
pub(crate) const EXTENT_SENTINEL: u32 = u32::MAX;

/// 90°/270° 的 transform 会让宽高互换（`vkGetPhysicalDeviceSurfaceCapabilitiesKHR` 的约定：
/// `min/max/currentImageExtent` 都是**旋转前**的尺寸，而旋转 90° 后屏幕上的宽高互换）。
pub(crate) fn transform_swaps_axes(transform: ash::vk::SurfaceTransformFlagsKHR) -> bool {
    transform.intersects(
        ash::vk::SurfaceTransformFlagsKHR::ROTATE_90
            | ash::vk::SurfaceTransformFlagsKHR::ROTATE_270,
    )
}

/// 按 `preTransform` 把「屏幕上的逻辑像素尺寸」换成「image 的尺寸」。
///
/// 旋转 90°/270° 时 image 的宽高是屏幕上的高宽（合成器自己做旋转），提交给
/// `VkSwapchainCreateInfoKHR::imageExtent` 的必须是 image 的尺寸。
pub(crate) fn image_extent_for_transform(
    logical: (u32, u32),
    pre_transform: ash::vk::SurfaceTransformFlagsKHR,
) -> (u32, u32) {
    if transform_swaps_axes(pre_transform) {
        (logical.1.max(1), logical.0.max(1))
    } else {
        (logical.0.max(1), logical.1.max(1))
    }
}

/// 计算 `VkSwapchainCreateInfoKHR::imageExtent`（R11-9）。
///
/// 三条规则（逐字对齐规范）：
/// 1. `currentExtent != 0xFFFFFFFF` ⇒ **必须**用它：surface 决定了尺寸，应用的自选值会被判
///    `SUBOPTIMAL`（持续 SUBOPTIMAL + 无条件重建 = 每帧重建自旋，R11-7 的触发条件之一）。
/// 2. `0xFFFFFFFF`（Wayland 常见）⇒ 用请求值，并按 `min/maxImageExtent` 夹紧
///    （越界会被 `vkCreateSwapchainKHR` 拒绝）。
/// 3. 请求值先按 `preTransform` 换轴。
///
/// 返回 `None` 表示 caps 退化（`min > max`），调用方必须报强类型错误而不是硬建链。
pub(crate) fn choose_extent(
    current_extent: (u32, u32),
    min_extent: (u32, u32),
    max_extent: (u32, u32),
    requested: (u32, u32),
    pre_transform: ash::vk::SurfaceTransformFlagsKHR,
) -> Option<(u32, u32)> {
    if current_extent.0 != EXTENT_SENTINEL && current_extent.1 != EXTENT_SENTINEL {
        // surface 已经决定：即使与请求值不同也必须遵从（否则永久 SUBOPTIMAL）。
        return Some((current_extent.0.max(1), current_extent.1.max(1)));
    }
    if min_extent.0 > max_extent.0 || min_extent.1 > max_extent.1 {
        return None;
    }
    let image = image_extent_for_transform(requested, pre_transform);
    Some((
        image.0.clamp(min_extent.0.max(1), max_extent.0.max(1)),
        image.1.clamp(min_extent.1.max(1), max_extent.1.max(1)),
    ))
}

/// `minImageCount` 的选择（R11-15）。
///
/// 规则：**至少** `minImageCount`，且至少 `MAX_FRAMES_IN_FLIGHT`（否则下一帧的 acquire 会先于
/// 前一帧的 fence 阻塞 —— 不是错误，是白丢性能），再按 `maxImageCount`（0 = 无上限）夹紧。
/// `minImageCount + 1` 是「多一张缓冲」的常规做法（避免 present 与 acquire 抢同一张）。
pub(crate) fn choose_image_count(min_image_count: u32, max_image_count: u32) -> u32 {
    let wanted = min_image_count
        .saturating_add(1)
        .max(MAX_FRAMES_IN_FLIGHT as u32);
    match max_image_count {
        0 => wanted,
        max => wanted.clamp(min_image_count, max.max(min_image_count)),
    }
}

/// 选 surface format（R11-8）：**format 与 colorSpace 必须同时匹配**引擎的呈现契约。
///
/// `VkSurfaceFormatKHR` 是 (format, colorSpace) 二元组：同一个 `B8G8R8A8_SRGB` 可以报
/// `SRGB_NONLINEAR`、`EXTENDED_SRGB_LINEAR`、`HDR10_ST2084`、`DISPLAY_P3`。旧实现只看 format、
/// 把合成器给的 colorSpace 原样透传，于是 HDR 合成器上我们写入的 sRGB 字节被按 PQ/HLG 解释，
/// 整屏亮度 / 对比度错误 —— 这是**只会在 Linux 上出现**的跨后端不一致。
///
/// 找不到精确匹配时返回 `None`（D8 硬基线：不做「format 对但色彩空间不对」的降级，那等于
/// 明知会偏色还继续）。
pub(crate) fn choose_surface_format(
    formats: &[ash::vk::SurfaceFormatKHR],
    color_space: impl Fn(ash::vk::ColorSpaceKHR) -> bool,
) -> Option<ash::vk::SurfaceFormatKHR> {
    formats
        .iter()
        .find(|candidate| candidate.format == RENDER_FORMAT && color_space(candidate.color_space))
        .copied()
}

/// present mode：Wayland surface 必须报告 `MAILBOX`（最新帧替换待呈现帧，与「最新帧槽」同构）。
///
/// 返回 `None` 表示不满足基线（不做 FIFO 静默回退：FIFO 会让「丢帧 + 合并 damage」的语义
/// 变成「排队」，与引擎的帧模型冲突）。
pub(crate) fn choose_present_mode(
    modes: &[ash::vk::PresentModeKHR],
) -> Option<ash::vk::PresentModeKHR> {
    modes
        .contains(&ash::vk::PresentModeKHR::MAILBOX)
        .then_some(ash::vk::PresentModeKHR::MAILBOX)
}

/// composite alpha：只能从 `supportedCompositeAlpha` 里挑（很多合成器只给 OPAQUE/INHERIT）。
///
/// 优先级：OPAQUE > INHERIT > PRE_MULTIPLIED > POST_MULTIPLIED（前两个在 Wayland 上语义
/// 完全相同；PRE/POST 会让合成器再做一次混合，与引擎「persist 已是不透明结果」的假设不符）。
pub(crate) fn choose_composite_alpha(
    supported: ash::vk::CompositeAlphaFlagsKHR,
) -> Option<ash::vk::CompositeAlphaFlagsKHR> {
    use ash::vk::CompositeAlphaFlagsKHR as Alpha;
    [
        Alpha::OPAQUE,
        Alpha::INHERIT,
        Alpha::PRE_MULTIPLIED,
        Alpha::POST_MULTIPLIED,
    ]
    .into_iter()
    .find(|candidate| supported.contains(*candidate))
}

/// 建链必需的 usage：`COLOR_ATTACHMENT`（渲染）与 `TRANSFER_DST`（persist → image）。
///
/// 函数而不是常量：ash 的位标志类型只提供 `BitOr`（不是 `const fn`），常量表达式里拼不出来。
pub(crate) fn required_usage() -> ash::vk::ImageUsageFlags {
    ash::vk::ImageUsageFlags::COLOR_ATTACHMENT | ash::vk::ImageUsageFlags::TRANSFER_DST
}

/// 一个候选设备的选卡事实（全是选卡阶段就能廉价查到的值）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeviceFacts {
    pub device_type: Option<ash::vk::PhysicalDeviceType>,
    /// 有 `GRAPHICS` 队列族。
    pub graphics_queue: bool,
    /// `VkPhysicalDeviceProperties.apiVersion`（**D8 基线**：`>= 1.2`）。
    pub api_version: u32,
    /// `VkPhysicalDeviceVulkan12Features.descriptorIndexing`（D8 基线；`api_version < 1.2`
    /// 的设备无法查询，恒 `false`）。
    pub descriptor_indexing: bool,
    /// `VkPhysicalDeviceVulkan12Features.timelineSemaphore`（D8 基线；`FrameSync` 的 retire
    /// 记账依赖它，所以两条路径都必须要求）。
    pub timeline_semaphore: bool,
    /// 该队列族能把图像呈现到这个 surface（离屏路径恒 `true`）。
    pub surface_queue: bool,
    /// 设备扩展含 `VK_KHR_swapchain`。
    pub swapchain_extension: bool,
    /// surface formats 里有 `RENDER_FORMAT` + 声明的 colorSpace。
    pub surface_format: bool,
    /// present modes 含 `MAILBOX`。
    pub present_mode: bool,
    /// `supportedUsageFlags ⊇ COLOR_ATTACHMENT | TRANSFER_DST`。
    pub usage: bool,
    /// `minImageExtent <= maxImageExtent` 且非退化。
    pub extent: bool,
}

/// 一个候选设备的选卡输入（设备枚举阶段的**值**快照；诊断 / 覆盖匹配需要名字与下标）。
#[derive(Clone, Debug)]
pub(crate) struct CandidateFacts {
    /// `vkEnumeratePhysicalDevices` 的下标（`KYTRIX_VK_DEVICE=<index>` 与诊断行用）。
    pub index: u32,
    /// `VkPhysicalDeviceProperties.deviceName`。
    pub name: SmolStr,
    /// 选卡事实。
    pub facts: DeviceFacts,
}

/// 一个候选设备被跳过的原因（写进 `UnsupportedGpu.actual`，R11-13 的「不要只说 no device」）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceRejection {
    NoGraphicsQueue,
    /// D8 基线：Vulkan 1.2 是 descriptorIndexing / timelineSemaphore 被启用与使用的前提。
    ApiVersionBelow12 {
        actual: u32,
    },
    /// D8 基线：`VkPhysicalDeviceVulkan12Features.descriptorIndexing`。
    MissingDescriptorIndexing,
    /// D8 基线：`VkPhysicalDeviceVulkan12Features.timelineSemaphore`。
    MissingTimelineSemaphore,
    NoSurfaceQueue,
    MissingSwapchainExtension,
    UnsupportedSurfaceFormat,
    MissingPresentMode,
    UnsupportedUsage,
    DegenerateExtent,
}

impl DeviceRejection {
    /// 诊断行里的稳定文案（`SmolStr`：apiVersion 需要格式化，其余是静态短串）。
    pub(crate) fn describe(self) -> SmolStr {
        match self {
            Self::NoGraphicsQueue => SmolStr::new_static("no GRAPHICS queue family"),
            Self::ApiVersionBelow12 { actual } => smol_str::format_smolstr!(
                "Vulkan {}.{}.{} is below the required 1.2",
                ash::vk::api_version_major(actual),
                ash::vk::api_version_minor(actual),
                ash::vk::api_version_patch(actual)
            ),
            Self::MissingDescriptorIndexing => {
                SmolStr::new_static("VK_EXT_descriptor_indexing (descriptorIndexing) missing")
            }
            Self::MissingTimelineSemaphore => {
                SmolStr::new_static("timelineSemaphore feature missing")
            }
            Self::NoSurfaceQueue => {
                SmolStr::new_static("queue family cannot present to the surface")
            }
            Self::MissingSwapchainExtension => SmolStr::new_static("VK_KHR_swapchain missing"),
            Self::UnsupportedSurfaceFormat => {
                SmolStr::new_static("surface lacks B8G8R8A8_SRGB + SRGB_NONLINEAR")
            }
            Self::MissingPresentMode => {
                SmolStr::new_static("surface lacks VK_PRESENT_MODE_MAILBOX_KHR")
            }
            Self::UnsupportedUsage => {
                SmolStr::new_static("surface lacks COLOR_ATTACHMENT | TRANSFER_DST")
            }
            Self::DegenerateExtent => {
                SmolStr::new_static("surface image extent range is degenerate")
            }
        }
    }
}

/// 给候选设备打分（`window_path = false` 时只要求图形队列与 D8 基线）。
///
/// 打分顺序：独显 4 > 集显 3 > 虚拟/其它 2 > CPU 软件光栅器 1。**CPU 不排除**：无头 CI /
/// 远程主机常常只有 lavapipe，把它彻底拒掉会让引擎在那类环境里跑不起来（Chrome 同样保留
/// SwiftShader 兜底）；硬件与软件都在时硬件必然胜出。
///
/// 过滤条件分两层：
/// 1. **D8 基线**（C10）：`apiVersion >= 1.2` + `descriptorIndexing` + `timelineSemaphore`。
///    这三项是 `create_device` 启用的 feature 与 `FrameSync` 的 retire 记账的前提；旧实现把它们
///    放在「选中之后」的 `GpuCaps::check()`，于是多 GPU（旧 dGPU + 新 iGPU / lavapipe）上会先
///    选中不合格的高分卡、再在启动阶段硬失败，而本可回退到合规设备。window / headless 两条
///    路径都要求（离屏也要 timeline）。对齐 Chromium `VulkanDeviceQueue::Initialize`（选卡循环
///    先过滤 `apiVersion` / 队列 / present 支持再打分）。
/// 2. **能不能真的把这个 surface 呈现出来**（R11-13）：只在 window 路径要求。
pub(crate) fn score_device(facts: &DeviceFacts, window_path: bool) -> Result<u32, DeviceRejection> {
    if !facts.graphics_queue {
        return Err(DeviceRejection::NoGraphicsQueue);
    }
    if facts.api_version < ash::vk::API_VERSION_1_2 {
        return Err(DeviceRejection::ApiVersionBelow12 {
            actual: facts.api_version,
        });
    }
    if !facts.descriptor_indexing {
        return Err(DeviceRejection::MissingDescriptorIndexing);
    }
    if !facts.timeline_semaphore {
        return Err(DeviceRejection::MissingTimelineSemaphore);
    }
    if window_path {
        if !facts.surface_queue {
            return Err(DeviceRejection::NoSurfaceQueue);
        }
        if !facts.swapchain_extension {
            return Err(DeviceRejection::MissingSwapchainExtension);
        }
        if !facts.surface_format {
            return Err(DeviceRejection::UnsupportedSurfaceFormat);
        }
        if !facts.present_mode {
            return Err(DeviceRejection::MissingPresentMode);
        }
        if !facts.usage {
            return Err(DeviceRejection::UnsupportedUsage);
        }
        if !facts.extent {
            return Err(DeviceRejection::DegenerateExtent);
        }
    }
    Ok(match facts.device_type {
        Some(ash::vk::PhysicalDeviceType::DISCRETE_GPU) => 4,
        Some(ash::vk::PhysicalDeviceType::INTEGRATED_GPU) => 3,
        Some(ash::vk::PhysicalDeviceType::CPU) => 1,
        Some(_) => 2,
        None => 0,
    })
}

/// 用户覆盖入口（R11-13）：`KYTRIX_VK_DEVICE` 强制选卡。
///
/// 取值：`<index>`（`vkEnumeratePhysicalDevices` 的下标）或 `<deviceName 子串>`（大小写不敏感）。
/// 现场排障的刚需：多 GPU 机器上要能强制走 iGPU / dGPU 复现问题（`DRI_PRIME` 只影响 Mesa 的
/// 默认选择，管不到 Vulkan 的显式枚举）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceSelector {
    /// 自动打分（默认）。
    Auto,
    /// 按枚举下标。
    Index(u32),
    /// 按 `deviceName` 子串（大小写不敏感）。
    Name(SmolStr),
}

/// 环境变量名（产品路径读取；`Auto` 时不设置）。
pub(crate) const DEVICE_OVERRIDE_ENV: &str = "KYTRIX_VK_DEVICE";

impl DeviceSelector {
    /// 解析环境变量的值；空串 / 纯空白视为 `Auto`。
    pub(crate) fn parse(value: &str) -> Self {
        let value = value.trim();
        if value.is_empty() {
            return Self::Auto;
        }
        match value.parse::<u32>() {
            Ok(index) => Self::Index(index),
            Err(_) => Self::Name(SmolStr::new(value)),
        }
    }

    /// 该候选是否被强制选中。
    pub(crate) fn matches(&self, index: u32, name: &str) -> bool {
        match self {
            Self::Auto => false,
            Self::Index(expected) => *expected == index,
            // 大小写不敏感的**子串**匹配：`KYTRIX_VK_DEVICE=llvmpipe` / `=NVIDIA` 都直观。
            Self::Name(name_filter) => name
                .to_ascii_lowercase()
                .contains(&name_filter.to_ascii_lowercase()),
        }
    }

    /// 诊断文案。
    pub(crate) fn describe(&self) -> SmolStr {
        match self {
            Self::Auto => SmolStr::new_static("auto"),
            Self::Index(index) => smol_str::format_smolstr!("index={index}"),
            Self::Name(name) => smol_str::format_smolstr!("name~{name}"),
        }
    }
}

/// 选卡决策（**纯逻辑**，可单测）：过滤 → 打分 → 取最高分；`selector` 命中时直接采用。
///
/// 返回选中候选在 `candidates` 里的位置。错误是 `UnsupportedGpu.actual` 的完整文案：
/// - 全部不合格：逐个列出 `#index name: reason`（现场排障要能区分「没有驱动」与「独显不提供
///   这个 surface 的 SRGB 格式」）；
/// - `KYTRIX_VK_DEVICE` 强制命中不合格设备：错误里带上覆盖来源与该设备的原因（保留原有语义）；
/// - 强制指定了不存在的设备：明确失败（静默回退 auto 会让「强制选卡复现问题」失效）。
pub(crate) fn pick_best(
    candidates: &[CandidateFacts],
    window_path: bool,
    selector: &DeviceSelector,
) -> Result<usize, SmolStr> {
    let mut best: Option<(usize, u32)> = None;
    let mut rejected: Vec<SmolStr> = Vec::with_capacity(candidates.len());
    let mut overridden_rejection: Option<SmolStr> = None;
    for (position, candidate) in candidates.iter().enumerate() {
        let forced = selector.matches(candidate.index, &candidate.name);
        let score = match score_device(&candidate.facts, window_path) {
            Ok(score) => score,
            Err(rejection) => {
                let reason = smol_str::format_smolstr!(
                    "#{} {:?}: {}",
                    candidate.index,
                    candidate.name,
                    rejection.describe()
                );
                if forced {
                    overridden_rejection = Some(reason.clone());
                }
                rejected.push(reason);
                continue;
            }
        };
        // 覆盖入口：命中即立刻采用（不再比较得分）。
        if forced {
            return Ok(position);
        }
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((position, score));
        }
    }
    if let Some(reason) = overridden_rejection {
        return Err(smol_str::format_smolstr!(
            "{DEVICE_OVERRIDE_ENV}={} rejected: {reason}",
            selector.describe()
        ));
    }
    let Some((position, _)) = best else {
        return Err(if rejected.is_empty() {
            SmolStr::new_static("no Vulkan physical devices")
        } else {
            smol_str::format_smolstr!("no usable device: {}", rejected.join("; "))
        });
    };
    if !matches!(selector, DeviceSelector::Auto) {
        return Err(smol_str::format_smolstr!(
            "{DEVICE_OVERRIDE_ENV}={} matched no device ({} candidates)",
            selector.describe(),
            candidates.len()
        ));
    }
    Ok(position)
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{
        CandidateFacts, DEVICE_OVERRIDE_ENV, DeviceFacts, DeviceRejection, DeviceSelector,
        SwapchainKey, choose_composite_alpha, choose_extent, choose_image_count,
        choose_present_mode, choose_surface_format, image_extent_for_transform, pick_best,
        score_device, transform_swaps_axes,
    };

    fn surface_format(format: vk::Format, color_space: vk::ColorSpaceKHR) -> vk::SurfaceFormatKHR {
        vk::SurfaceFormatKHR {
            format,
            color_space,
        }
    }

    fn srgb_color_space(color_space: vk::ColorSpaceKHR) -> bool {
        color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
    }

    // ── R11-9：extent / transform ────────────────────────────────────────────

    #[test]
    fn a_concrete_current_extent_overrides_the_request() {
        // surface 决定尺寸（0xFFFFFFFF 之外的形态）：请求值必须被忽略，否则永久 SUBOPTIMAL。
        assert_eq!(
            choose_extent(
                (800, 600),
                (1, 1),
                (4096, 4096),
                (1920, 1080),
                vk::SurfaceTransformFlagsKHR::IDENTITY
            ),
            Some((800, 600))
        );
    }

    #[test]
    fn the_app_decided_extent_is_clamped_to_the_supported_range() {
        let identity = vk::SurfaceTransformFlagsKHR::IDENTITY;
        assert_eq!(
            choose_extent(
                (u32::MAX, u32::MAX),
                (1, 1),
                (2560, 1600),
                (3840, 2160),
                identity
            ),
            Some((2560, 1600)),
            "超过 maxImageExtent 必须下夹（否则建链直接失败）"
        );
        assert_eq!(
            choose_extent(
                (u32::MAX, u32::MAX),
                (64, 64),
                (4096, 4096),
                (10, 10),
                identity
            ),
            Some((64, 64)),
            "低于 minImageExtent 必须上夹（否则 renderArea 会超出 framebuffer）"
        );
    }

    #[test]
    fn degenerate_capabilities_are_rejected_instead_of_built() {
        assert_eq!(
            choose_extent(
                (u32::MAX, u32::MAX),
                (100, 100),
                (50, 50),
                (10, 10),
                vk::SurfaceTransformFlagsKHR::IDENTITY
            ),
            None,
            "min > max 不是可建链的表面"
        );
    }

    #[test]
    fn a_rotated_surface_swaps_the_image_axes() {
        assert!(transform_swaps_axes(
            vk::SurfaceTransformFlagsKHR::ROTATE_90
        ));
        assert!(transform_swaps_axes(
            vk::SurfaceTransformFlagsKHR::ROTATE_270
        ));
        assert!(!transform_swaps_axes(
            vk::SurfaceTransformFlagsKHR::IDENTITY
        ));
        assert!(!transform_swaps_axes(
            vk::SurfaceTransformFlagsKHR::ROTATE_180
        ));
        assert_eq!(
            image_extent_for_transform((800, 600), vk::SurfaceTransformFlagsKHR::ROTATE_90),
            (600, 800)
        );
        // 旋转 + 应用决定尺寸：夹紧发生在换轴**之后**（夹的是 image 尺寸）。
        assert_eq!(
            choose_extent(
                (u32::MAX, u32::MAX),
                (1, 1),
                (500, 4096),
                (800, 600),
                vk::SurfaceTransformFlagsKHR::ROTATE_90
            ),
            Some((500, 800)),
            "image 尺寸 = (高, 宽)，再按 caps 夹紧"
        );
    }

    // ── R11-15：image count ─────────────────────────────────────────────────

    #[test]
    fn the_image_count_never_drops_below_the_minimum_or_the_frame_slots() {
        // 常规：min+1 且至少 3。
        assert_eq!(choose_image_count(2, 0), 3);
        assert_eq!(choose_image_count(3, 0), 4);
        // `maxImageCount` 是**硬上界**（`VUID-VkSwapchainCreateInfoKHR-imageExtent-...` 同族的
        // `minImageCount <= maxImageCount` 约束）：min=2/max=2 时唯一合法值是 2。
        // 「至少 MAX_FRAMES_IN_FLIGHT」只是**期望**，不能压过 surface 的合法区间 —— 只有 2 张图时
        // 第三帧会先在槽 fence 上等一等（性能损失），但链本身合法。
        assert_eq!(
            choose_image_count(2, 2),
            2,
            "max=2 时必须夹回 2（3 会被驱动判 VUID）"
        );
        assert_eq!(choose_image_count(4, 3), 4, "max < min：仍必须满足 min");
        // 无上限。
        assert_eq!(choose_image_count(0, 0), 3, "至少 MAX_FRAMES_IN_FLIGHT");
        assert_eq!(choose_image_count(3, 8), 4);
        // 不变式：任何输入下都落在 [min, max] 内（max=0 表示无上限）。
        for (min, max) in [(0, 0), (1, 1), (2, 2), (2, 3), (4, 3), (5, 5), (1, 9)] {
            let count = choose_image_count(min, max);
            assert!(count >= min.max(1), "min={min} max={max} count={count}");
            if max != 0 {
                assert!(count <= max.max(min), "min={min} max={max} count={count}");
            }
        }
    }

    // ── R11-8：format / colorSpace ──────────────────────────────────────────

    #[test]
    fn the_surface_format_requires_both_format_and_color_space() {
        let formats = [
            surface_format(
                vk::Format::B8G8R8A8_SRGB,
                vk::ColorSpaceKHR::HDR10_ST2084_EXT,
            ),
            surface_format(vk::Format::B8G8R8A8_SRGB, vk::ColorSpaceKHR::SRGB_NONLINEAR),
        ];
        assert_eq!(
            choose_surface_format(&formats, srgb_color_space),
            Some(surface_format(
                vk::Format::B8G8R8A8_SRGB,
                vk::ColorSpaceKHR::SRGB_NONLINEAR
            )),
            "同一 format 的 HDR 色彩空间必须被跳过"
        );
        // 只有 HDR 色彩空间：明确失败（不做「format 对就凑合」的降级）。
        assert_eq!(choose_surface_format(&formats[..1], srgb_color_space), None);
        // 格式不对也不行（整面 cmdCopyImage 要求格式兼容）。
        assert_eq!(
            choose_surface_format(
                &[surface_format(
                    vk::Format::R8G8B8A8_SRGB,
                    vk::ColorSpaceKHR::SRGB_NONLINEAR
                )],
                srgb_color_space
            ),
            None
        );
    }

    #[test]
    fn mailbox_is_the_only_accepted_present_mode() {
        assert_eq!(
            choose_present_mode(&[vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX]),
            Some(vk::PresentModeKHR::MAILBOX)
        );
        assert_eq!(choose_present_mode(&[vk::PresentModeKHR::FIFO]), None);
    }

    #[test]
    fn the_composite_alpha_comes_from_the_supported_set() {
        assert_eq!(
            choose_composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE),
            Some(vk::CompositeAlphaFlagsKHR::OPAQUE)
        );
        assert_eq!(
            choose_composite_alpha(vk::CompositeAlphaFlagsKHR::INHERIT),
            Some(vk::CompositeAlphaFlagsKHR::INHERIT)
        );
        assert_eq!(
            choose_composite_alpha(
                vk::CompositeAlphaFlagsKHR::OPAQUE | vk::CompositeAlphaFlagsKHR::INHERIT
            ),
            Some(vk::CompositeAlphaFlagsKHR::OPAQUE),
            "OPAQUE 优先（Wayland 上与 INHERIT 等价，但后者依赖合成器）"
        );
        assert_eq!(
            choose_composite_alpha(vk::CompositeAlphaFlagsKHR::empty()),
            None
        );
    }

    // ── R11-13：选卡 ────────────────────────────────────────────────────────

    fn windowed_facts(device_type: vk::PhysicalDeviceType) -> DeviceFacts {
        DeviceFacts {
            device_type: Some(device_type),
            graphics_queue: true,
            // D8 基线（C10）：合规设备的默认值。
            api_version: vk::API_VERSION_1_2,
            descriptor_indexing: true,
            timeline_semaphore: true,
            surface_queue: true,
            swapchain_extension: true,
            surface_format: true,
            present_mode: true,
            usage: true,
            extent: true,
        }
    }

    fn candidate(index: u32, name: &str, facts: DeviceFacts) -> CandidateFacts {
        CandidateFacts {
            index,
            name: smol_str::SmolStr::new(name),
            facts,
        }
    }

    #[test]
    fn a_device_that_cannot_present_is_rejected_instead_of_failing_later() {
        // 独显「更强」但不能呈现这个 surface（PRIME / 远程合成器）：必须被跳过。
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.surface_format = false;
        assert_eq!(
            score_device(&discrete, true),
            Err(DeviceRejection::UnsupportedSurfaceFormat)
        );
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.surface_queue = false;
        assert_eq!(
            score_device(&discrete, true),
            Err(DeviceRejection::NoSurfaceQueue)
        );
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.present_mode = false;
        assert_eq!(
            score_device(&discrete, true),
            Err(DeviceRejection::MissingPresentMode)
        );
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.usage = false;
        assert_eq!(
            score_device(&discrete, true),
            Err(DeviceRejection::UnsupportedUsage)
        );
        // 能呈现的那张（集显）必须被接受：这正是「退到能工作的 GPU」的意义。
        assert_eq!(
            score_device(
                &windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU),
                true
            ),
            Ok(3)
        );
        assert_eq!(
            score_device(&windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU), true),
            Ok(4)
        );
    }

    #[test]
    fn scoring_prefers_discrete_then_integrated_and_keeps_cpu_as_a_fallback() {
        let discrete = score_device(&windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU), true)
            .expect("discrete");
        let integrated = score_device(
            &windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU),
            true,
        )
        .expect("integrated");
        let cpu = score_device(&windowed_facts(vk::PhysicalDeviceType::CPU), true).expect("cpu");
        assert!(
            discrete > integrated && integrated > cpu,
            "{discrete:?} {integrated:?} {cpu:?}"
        );
        assert!(score_device(&windowed_facts(vk::PhysicalDeviceType::CPU), true).is_ok());
    }

    #[test]
    fn the_headless_path_does_not_require_swapchain_capabilities() {
        let mut facts = windowed_facts(vk::PhysicalDeviceType::CPU);
        facts.surface_queue = false;
        facts.swapchain_extension = false;
        facts.surface_format = false;
        facts.present_mode = false;
        facts.usage = false;
        facts.extent = false;
        assert!(
            score_device(&facts, false).is_ok(),
            "离屏路径不需要 WSI（只会建 device + persist）"
        );
    }

    #[test]
    fn a_device_without_a_graphics_queue_is_always_rejected() {
        let mut facts = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        facts.graphics_queue = false;
        assert_eq!(
            score_device(&facts, false),
            Err(DeviceRejection::NoGraphicsQueue)
        );
    }

    #[test]
    fn the_device_override_accepts_an_index_or_a_name_substring() {
        assert_eq!(DeviceSelector::parse(""), DeviceSelector::Auto);
        assert_eq!(DeviceSelector::parse("   "), DeviceSelector::Auto);
        assert_eq!(DeviceSelector::parse("1"), DeviceSelector::Index(1));
        assert_eq!(
            DeviceSelector::parse("NVIDIA"),
            DeviceSelector::Name(smol_str::SmolStr::new_static("NVIDIA"))
        );
        let by_index = DeviceSelector::parse("2");
        assert!(by_index.matches(2, "llvmpipe"));
        assert!(!by_index.matches(0, "llvmpipe"));
        let by_name = DeviceSelector::parse("llvmpipe");
        assert!(by_name.matches(0, "llvmpipe (LLVM 20.1.0, 256 bits)"));
        assert!(by_name.matches(0, "LLVMPIPE"));
        assert!(!by_name.matches(0, "Intel(R) UHD Graphics"));
        assert!(!DeviceSelector::Auto.matches(0, "anything"));
        assert!(!by_name.describe().is_empty());
        assert!(!by_index.describe().is_empty());
    }

    #[test]
    fn rejection_reasons_are_human_readable() {
        for rejection in [
            DeviceRejection::NoGraphicsQueue,
            DeviceRejection::ApiVersionBelow12 {
                actual: vk::make_api_version(0, 1, 1, 0),
            },
            DeviceRejection::MissingDescriptorIndexing,
            DeviceRejection::MissingTimelineSemaphore,
            DeviceRejection::NoSurfaceQueue,
            DeviceRejection::MissingSwapchainExtension,
            DeviceRejection::UnsupportedSurfaceFormat,
            DeviceRejection::MissingPresentMode,
            DeviceRejection::UnsupportedUsage,
            DeviceRejection::DegenerateExtent,
        ] {
            assert!(!rejection.describe().is_empty());
        }
        // apiVersion 必须格式化成可读版本号，而不是裸 `u32`。
        let text = DeviceRejection::ApiVersionBelow12 {
            actual: vk::make_api_version(0, 1, 1, 0),
        }
        .describe();
        assert!(text.contains("1.1.0"), "{text}");
        assert!(text.contains("1.2"), "{text}");
    }

    // ── C10：D8 基线作为选卡过滤条件 ─────────────────────────────────────

    /// 3 个新 rejection 在 window / headless 两条路径上都生效：timeline 被 `FrameSync` 使用、
    /// descriptorIndexing 在 `create_device` 里启用，与是否呈现无关。
    #[test]
    fn the_d8_baseline_is_enforced_on_both_the_window_and_headless_paths() {
        let mut below_12 = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        below_12.api_version = vk::make_api_version(0, 1, 1, 0);
        let mut no_descriptor_indexing = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        no_descriptor_indexing.descriptor_indexing = false;
        let mut no_timeline = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        no_timeline.timeline_semaphore = false;
        for window_path in [true, false] {
            assert_eq!(
                score_device(&below_12, window_path),
                Err(DeviceRejection::ApiVersionBelow12 {
                    actual: vk::make_api_version(0, 1, 1, 0)
                }),
                "window_path={window_path}"
            );
            assert_eq!(
                score_device(&no_descriptor_indexing, window_path),
                Err(DeviceRejection::MissingDescriptorIndexing),
                "window_path={window_path}"
            );
            assert_eq!(
                score_device(&no_timeline, window_path),
                Err(DeviceRejection::MissingTimelineSemaphore),
                "window_path={window_path}"
            );
        }
    }

    /// I10-b：高分但 api < 1.2 的独显必须被跳过，合规的集显被选中（多 GPU / PRIME 的真实现场）。
    #[test]
    fn a_high_scoring_but_incompatible_device_is_skipped_for_a_compliant_one() {
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.api_version = vk::make_api_version(0, 1, 0, 0);
        let integrated = windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU);
        let candidates = [
            candidate(0, "Old Discrete GPU", discrete),
            candidate(1, "New Integrated GPU", integrated),
        ];
        // window 与 headless 两条路径都必须退到合规的集显。
        for window_path in [true, false] {
            let selected = pick_best(&candidates, window_path, &DeviceSelector::Auto)
                .expect("合规的集显必须被接受");
            assert_eq!(selected, 1, "高分但不合规的独显不能胜出");
            assert_eq!(
                score_device(&candidates[selected].facts, window_path),
                Ok(3)
            );
        }
        // 只有那张不合规的独显：列出原因而不是静默启动失败。
        let error = pick_best(&candidates[..1], true, &DeviceSelector::Auto)
            .expect_err("没有合规设备时必须报错");
        assert!(error.contains("no usable device"), "{error}");
        assert!(error.contains("#0 \"Old Discrete GPU\""), "{error}");
        assert!(error.contains("below the required 1.2"), "{error}");
    }

    /// I10-b：高分但缺 timelineSemaphore 的独显同样被跳过。
    #[test]
    fn a_high_scoring_device_without_timeline_semaphore_is_rejected() {
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.timeline_semaphore = false;
        let integrated = windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU);
        let candidates = [
            candidate(0, "Discrete", discrete),
            candidate(1, "Integrated", integrated),
        ];
        assert_eq!(
            pick_best(&candidates, true, &DeviceSelector::Auto),
            Ok(1),
            "不合规的高分设备必须被跳过"
        );
    }

    /// I10-c：全部不合格时错误逐个列出每台设备的原因（含下标与名字）。
    #[test]
    fn pick_best_lists_every_candidate_reason_when_no_device_is_usable() {
        let mut first = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        first.api_version = vk::make_api_version(0, 1, 0, 0);
        let mut second = windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU);
        second.timeline_semaphore = false;
        let candidates = [
            candidate(0, "Old Discrete", first),
            candidate(1, "Broken Integrated", second),
        ];
        let error =
            pick_best(&candidates, true, &DeviceSelector::Auto).expect_err("全部不合格必须报错");
        assert!(error.contains("no usable device"), "{error}");
        assert!(error.contains("#0 \"Old Discrete\""), "{error}");
        assert!(error.contains("#1 \"Broken Integrated\""), "{error}");
        assert!(error.contains("below the required 1.2"), "{error}");
        assert!(
            error.contains("timelineSemaphore feature missing"),
            "{error}"
        );
    }

    /// I10-c：`KYTRIX_VK_DEVICE` 强制命中不合格设备时，错误必须说明覆盖来源与该设备的原因。
    #[test]
    fn a_forced_selector_that_hits_an_incompatible_device_reports_the_override() {
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.descriptor_indexing = false;
        let candidates = [candidate(0, "llvmpipe", discrete)];
        let error = pick_best(&candidates, true, &DeviceSelector::Index(0))
            .expect_err("强制命中不合格设备必须报错");
        assert!(error.contains(DEVICE_OVERRIDE_ENV), "{error}");
        assert!(error.contains("index=0 rejected"), "{error}");
        assert!(error.contains("descriptorIndexing"), "{error}");
        let error = pick_best(&candidates, true, &DeviceSelector::Name("llvmpipe".into()))
            .expect_err("按名字覆盖命中不合格设备同样报错");
        assert!(error.contains("name~llvmpipe rejected"), "{error}");
    }

    /// 覆盖入口命中合规设备时直接采用（即使不是最高分）：现场排障要能强制走 iGPU / dGPU。
    #[test]
    fn a_forced_selector_that_hits_a_compatible_device_wins_over_the_auto_score() {
        let discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        let integrated = windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU);
        let candidates = [
            candidate(0, "Discrete", discrete),
            candidate(1, "Integrated", integrated),
        ];
        assert_eq!(
            pick_best(&candidates, true, &DeviceSelector::Index(1)),
            Ok(1)
        );
        // 指定了不存在的设备：明确失败，不静默回退 auto。
        let error = pick_best(&candidates, true, &DeviceSelector::Index(7))
            .expect_err("覆盖不存在的设备必须报错");
        assert!(
            error.contains("matched no device (2 candidates)"),
            "{error}"
        );
    }

    /// 合规的低分设备（CPU / lavapipe）必须被接受：无头 CI / 远程主机常常只有软件光栅器。
    #[test]
    fn a_compliant_low_score_device_is_accepted() {
        let cpu = windowed_facts(vk::PhysicalDeviceType::CPU);
        let candidates = [candidate(0, "llvmpipe", cpu)];
        assert_eq!(
            pick_best(&candidates, false, &DeviceSelector::Auto),
            Ok(0),
            "lavapipe 合规就必须能跑（D8 基线不排除 CPU 设备）"
        );
    }

    /// `SwapchainKey` 是重建必要性的判据来源：任一字段变化都必须被认为不同。
    #[test]
    fn the_swapchain_key_covers_every_build_parameter() {
        let base = SwapchainKey {
            extent: (800, 600),
            format: vk::Format::B8G8R8A8_SRGB,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            present_mode: vk::PresentModeKHR::MAILBOX,
            composite_alpha: vk::CompositeAlphaFlagsKHR::OPAQUE,
            pre_transform: vk::SurfaceTransformFlagsKHR::IDENTITY,
            image_count: 3,
        };
        assert_eq!(base, base);
        let mut changed = base;
        changed.extent = (801, 600);
        assert_ne!(changed, base);
        let mut changed = base;
        changed.image_count = 4;
        assert_ne!(changed, base, "image count 变化意味着链本身不同");
    }
}
