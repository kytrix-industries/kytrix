#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentedColorSpace {
    SrgbRec709,
}

pub(crate) const PRESENTED_COLOR_SPACE: PresentedColorSpace = PresentedColorSpace::SrgbRec709;

#[cfg(windows)]
#[must_use]
pub(crate) fn dxgi_color_space() -> windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

    match PRESENTED_COLOR_SPACE {
        PresentedColorSpace::SrgbRec709 => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    }
}

#[cfg(windows)]
#[must_use]
pub(crate) fn support_allows_present(support: u32) -> bool {
    use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT;

    support & (DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32) != 0
}

#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn vulkan_color_space() -> ash::vk::ColorSpaceKHR {
    match PRESENTED_COLOR_SPACE {
        PresentedColorSpace::SrgbRec709 => ash::vk::ColorSpaceKHR::SRGB_NONLINEAR,
    }
}

#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn accepts_color_space(color_space: ash::vk::ColorSpaceKHR) -> bool {
    color_space == vulkan_color_space()
}

#[cfg(target_os = "macos")]
#[must_use]
pub(crate) fn layer_uses_srgb() -> bool {
    matches!(PRESENTED_COLOR_SPACE, PresentedColorSpace::SrgbRec709)
}

#[cfg(test)]
mod tests {
    use super::{PRESENTED_COLOR_SPACE, PresentedColorSpace};

    fn backends() -> [(&'static str, &'static str); 3] {
        [
            (
                "render/windows/swapchain.rs",
                include_str!("windows/swapchain.rs"),
            ),
            (
                "render/macos/swapchain.rs",
                include_str!("macos/swapchain.rs"),
            ),
            (
                "render/linux/swapchain.rs",
                include_str!("linux/swapchain.rs"),
            ),
        ]
    }

    fn production(source: &'static str) -> &'static str {
        source.split("#[cfg(test)]").next().unwrap_or(source)
    }

    fn production_code(source: &'static str) -> String {
        production(source)
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_presented_color_space_is_explicit_srgb_and_consumed_by_all_backends() {
        assert_eq!(PRESENTED_COLOR_SPACE, PresentedColorSpace::SrgbRec709);

        for (name, source) in backends() {
            let source = production_code(source);
            assert!(
                source.contains("colorspace") || source.contains("color_space"),
                "{name} 必须消费共享层的色彩空间决定"
            );
            for forbidden in ["G2084", "HDR10", "extendedRange", "DISPLAY_P3", "BT2020"] {
                assert!(
                    !source.contains(forbidden),
                    "{name} 不得写死 {forbidden}（HDR / 广色域是显式不支持的决策）"
                );
            }
        }
        let linux = production_code(include_str!("linux/swapchain.rs"));
        assert!(
            linux.contains("accepts_color_space"),
            "Linux 必须用共享判据筛选 surface format（不得透传合成器给的值）"
        );
        assert!(
            linux.contains("image_color_space"),
            "创建 swapchain 时必须显式写入声明的 `imageColorSpace`"
        );
        assert!(production_code(include_str!("windows/swapchain.rs")).contains("SetColorSpace1"));
        assert!(production_code(include_str!("macos/swapchain.rs")).contains("setColorspace"));
    }

    #[test]
    fn hdr_is_explicitly_unsupported() {
        fn only_srgb(color_space: PresentedColorSpace) -> bool {
            match color_space {
                PresentedColorSpace::SrgbRec709 => true,
            }
        }
        assert!(only_srgb(PRESENTED_COLOR_SPACE));

        let shared = production_code(include_str!("colorspace.rs"));
        for forbidden in ["G2084", "HDR10", "extendedRange", "BT2020", "DISPLAY_P3"] {
            assert!(
                !shared.contains(forbidden),
                "共享决定点不得出现 {forbidden}（HDR 需新增变体 + 三端逐端 match）"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_maps_to_rgb_full_g22_none_p709() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

        assert_eq!(
            super::dxgi_color_space(),
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
        );
        assert!(super::support_allows_present(0x1));
        assert!(!super::support_allows_present(0x0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_requires_srgb_nonlinear() {
        use ash::vk;

        assert_eq!(
            super::vulkan_color_space(),
            vk::ColorSpaceKHR::SRGB_NONLINEAR
        );
        assert!(super::accepts_color_space(
            vk::ColorSpaceKHR::SRGB_NONLINEAR
        ));
        assert!(!super::accepts_color_space(
            vk::ColorSpaceKHR::DISPLAY_P3_NONLINEAR_EXT
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_layer_declares_srgb() {
        assert!(super::layer_uses_srgb());
    }
}
