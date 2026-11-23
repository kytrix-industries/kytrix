use smol_str::SmolStr;

use crate::render::{frame::MAX_FRAMES_IN_FLIGHT, linux::pipeline::RENDER_FORMAT};

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

pub(crate) const EXTENT_SENTINEL: u32 = u32::MAX;

pub(crate) fn transform_swaps_axes(transform: ash::vk::SurfaceTransformFlagsKHR) -> bool {
    transform.intersects(
        ash::vk::SurfaceTransformFlagsKHR::ROTATE_90
            | ash::vk::SurfaceTransformFlagsKHR::ROTATE_270,
    )
}

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

pub(crate) fn choose_extent(
    current_extent: (u32, u32),
    min_extent: (u32, u32),
    max_extent: (u32, u32),
    requested: (u32, u32),
    pre_transform: ash::vk::SurfaceTransformFlagsKHR,
) -> Option<(u32, u32)> {
    if current_extent.0 != EXTENT_SENTINEL && current_extent.1 != EXTENT_SENTINEL {
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

pub(crate) fn choose_image_count(min_image_count: u32, max_image_count: u32) -> u32 {
    let wanted = min_image_count
        .saturating_add(1)
        .max(MAX_FRAMES_IN_FLIGHT as u32);
    match max_image_count {
        0 => wanted,
        max => wanted.clamp(min_image_count, max.max(min_image_count)),
    }
}

pub(crate) fn choose_surface_format(
    formats: &[ash::vk::SurfaceFormatKHR],
    color_space: impl Fn(ash::vk::ColorSpaceKHR) -> bool,
) -> Option<ash::vk::SurfaceFormatKHR> {
    formats
        .iter()
        .find(|candidate| candidate.format == RENDER_FORMAT && color_space(candidate.color_space))
        .copied()
}

pub(crate) fn choose_present_mode(
    modes: &[ash::vk::PresentModeKHR],
) -> Option<ash::vk::PresentModeKHR> {
    modes
        .contains(&ash::vk::PresentModeKHR::MAILBOX)
        .then_some(ash::vk::PresentModeKHR::MAILBOX)
}

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

pub(crate) fn required_usage() -> ash::vk::ImageUsageFlags {
    ash::vk::ImageUsageFlags::COLOR_ATTACHMENT | ash::vk::ImageUsageFlags::TRANSFER_DST
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeviceFacts {
    pub device_type: Option<ash::vk::PhysicalDeviceType>,
    pub graphics_queue: bool,
    pub api_version: u32,
    pub descriptor_indexing: bool,
    pub timeline_semaphore: bool,
    pub surface_queue: bool,
    pub swapchain_extension: bool,
    pub surface_format: bool,
    pub present_mode: bool,
    pub usage: bool,
    pub extent: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CandidateFacts {
    pub index: u32,
    pub name: SmolStr,
    pub facts: DeviceFacts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceRejection {
    NoGraphicsQueue,
    ApiVersionBelow12 { actual: u32 },
    MissingDescriptorIndexing,
    MissingTimelineSemaphore,
    NoSurfaceQueue,
    MissingSwapchainExtension,
    UnsupportedSurfaceFormat,
    MissingPresentMode,
    UnsupportedUsage,
    DegenerateExtent,
}

impl DeviceRejection {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceSelector {
    Auto,
    Index(u32),
    Name(SmolStr),
}

pub(crate) const DEVICE_OVERRIDE_ENV: &str = "KYTRIX_VK_DEVICE";

impl DeviceSelector {
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

    pub(crate) fn matches(&self, index: u32, name: &str) -> bool {
        match self {
            Self::Auto => false,
            Self::Index(expected) => *expected == index,
            Self::Name(name_filter) => name
                .to_ascii_lowercase()
                .contains(&name_filter.to_ascii_lowercase()),
        }
    }

    pub(crate) fn describe(&self) -> SmolStr {
        match self {
            Self::Auto => SmolStr::new_static("auto"),
            Self::Index(index) => smol_str::format_smolstr!("index={index}"),
            Self::Name(name) => smol_str::format_smolstr!("name~{name}"),
        }
    }
}

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

    #[test]
    fn a_concrete_current_extent_overrides_the_request() {
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

    #[test]
    fn the_image_count_never_drops_below_the_minimum_or_the_frame_slots() {
        assert_eq!(choose_image_count(2, 0), 3);
        assert_eq!(choose_image_count(3, 0), 4);
        assert_eq!(
            choose_image_count(2, 2),
            2,
            "max=2 时必须夹回 2（3 会被驱动判 VUID）"
        );
        assert_eq!(choose_image_count(4, 3), 4, "max < min：仍必须满足 min");
        assert_eq!(choose_image_count(0, 0), 3, "至少 MAX_FRAMES_IN_FLIGHT");
        assert_eq!(choose_image_count(3, 8), 4);
        for (min, max) in [(0, 0), (1, 1), (2, 2), (2, 3), (4, 3), (5, 5), (1, 9)] {
            let count = choose_image_count(min, max);
            assert!(count >= min.max(1), "min={min} max={max} count={count}");
            if max != 0 {
                assert!(count <= max.max(min), "min={min} max={max} count={count}");
            }
        }
    }

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
        assert_eq!(choose_surface_format(&formats[..1], srgb_color_space), None);
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

    fn windowed_facts(device_type: vk::PhysicalDeviceType) -> DeviceFacts {
        DeviceFacts {
            device_type: Some(device_type),
            graphics_queue: true,
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
        let text = DeviceRejection::ApiVersionBelow12 {
            actual: vk::make_api_version(0, 1, 1, 0),
        }
        .describe();
        assert!(text.contains("1.1.0"), "{text}");
        assert!(text.contains("1.2"), "{text}");
    }

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

    #[test]
    fn a_high_scoring_but_incompatible_device_is_skipped_for_a_compliant_one() {
        let mut discrete = windowed_facts(vk::PhysicalDeviceType::DISCRETE_GPU);
        discrete.api_version = vk::make_api_version(0, 1, 0, 0);
        let integrated = windowed_facts(vk::PhysicalDeviceType::INTEGRATED_GPU);
        let candidates = [
            candidate(0, "Old Discrete GPU", discrete),
            candidate(1, "New Integrated GPU", integrated),
        ];
        for window_path in [true, false] {
            let selected = pick_best(&candidates, window_path, &DeviceSelector::Auto)
                .expect("合规的集显必须被接受");
            assert_eq!(selected, 1, "高分但不合规的独显不能胜出");
            assert_eq!(
                score_device(&candidates[selected].facts, window_path),
                Ok(3)
            );
        }
        let error = pick_best(&candidates[..1], true, &DeviceSelector::Auto)
            .expect_err("没有合规设备时必须报错");
        assert!(error.contains("no usable device"), "{error}");
        assert!(error.contains("#0 \"Old Discrete GPU\""), "{error}");
        assert!(error.contains("below the required 1.2"), "{error}");
    }

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
        let error = pick_best(&candidates, true, &DeviceSelector::Index(7))
            .expect_err("覆盖不存在的设备必须报错");
        assert!(
            error.contains("matched no device (2 candidates)"),
            "{error}"
        );
    }

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
