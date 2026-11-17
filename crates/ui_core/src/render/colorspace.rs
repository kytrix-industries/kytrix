//! 呈现色彩空间：**三端统一声明** sRGB（R06-10）。
//!
//! 引擎整条链只产出一件事：**sRGB 编码的 8-bit RGBA**（`apply_gamma` 恒等，编码由 sRGB 渲染
//! 目标格式在写回时完成一次，见 `contract.md` §3）。因此「呈现面内容是什么色彩空间」必须是显式
//! 契约，而不是各平台各自推断：
//!
//! | 平台 | 显式声明 | 落地 API |
//! |------|----------|----------|
//! | Windows | `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709`（= sRGB / Rec.709） | `IDXGISwapChain3::SetColorSpace1`（`windows/swapchain.rs`） |
//! | macOS | `kCGColorSpaceSRGB` | `CAMetalLayer.setColorspace`（`macos/swapchain.rs`） |
//! | Linux | `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR` | `VkSwapchainCreateInfoKHR::imageColorSpace`（`linux/swapchain.rs` 的格式筛选） |
//!
//! 三端的**选择逻辑只在本模块**：平台后端只负责把 [`PRESENTED_COLOR_SPACE`] 翻译成各自的 API
//! 常量，不得自行决定（旧实现里 macOS 声明了 sRGB、Windows 完全不声明、Linux 直接把合成器给的
//! `colorSpace` 透传，三端因此有三条不同的隐式推断路径）。
//!
//! **HDR 显式不支持**（不是 TODO 静默）：HDR 呈现需要 `G2084_NONE_P2020`（Windows）、Metal 的
//! `extendedRange`、Vulkan 的 `VK_COLOR_SPACE_HDR10_ST2084_EXT`，并且整条链都要重估 —— 渲染目标
//! 格式与混合空间、`background_linear` 的绝对值、图集纹理格式、golden 容差。`PresentedColorSpace`
//! 因此只有 sRGB 一个变体：接入 HDR 时新增变体，三端各自的 `match` 会在编译期报「未覆盖变体」，
//! 不会漏掉任何一端。

/// 呈现面内容的目标色彩空间。
///
/// 当前只有 sRGB（见模块文档的 HDR 说明）；用枚举而不是裸常量，是为了让三端在接入 HDR 时被迫
/// 逐端处理（`match` 覆盖性检查）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentedColorSpace {
    /// sRGB 编码 + Rec.709 原色 + γ 2.2 近似（`RGB_FULL_G22_NONE_P709`）。
    ///
    /// 三端都必须按它显式声明：Windows 走 `SetColorSpace1`、Linux 只接受
    /// `SRGB_NONLINEAR` 的 surface format、macOS 设置 `kCGColorSpaceSRGB`。
    SrgbRec709,
}

/// 引擎唯一的呈现色彩空间（三端都读它，不得各自决定）。
pub(crate) const PRESENTED_COLOR_SPACE: PresentedColorSpace = PresentedColorSpace::SrgbRec709;

// ── Windows：`IDXGISwapChain3::SetColorSpace1` ───────────────────────────────

/// [`PRESENTED_COLOR_SPACE`] 对应的 DXGI 色彩空间常量。
#[cfg(windows)]
#[must_use]
pub(crate) fn dxgi_color_space() -> windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

    match PRESENTED_COLOR_SPACE {
        PresentedColorSpace::SrgbRec709 => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    }
}

/// `CheckColorSpaceSupport` 的返回值是否允许显式设置（含 `PRESENT` 位）。
#[cfg(windows)]
#[must_use]
pub(crate) fn support_allows_present(support: u32) -> bool {
    use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT;

    support & (DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32) != 0
}

// ── Linux：surface format 的 `colorSpace` 必须与呈现契约一致 ──────────────────

/// [`PRESENTED_COLOR_SPACE`] 对应的 Vulkan 色彩空间。
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn vulkan_color_space() -> ash::vk::ColorSpaceKHR {
    match PRESENTED_COLOR_SPACE {
        PresentedColorSpace::SrgbRec709 => ash::vk::ColorSpaceKHR::SRGB_NONLINEAR,
    }
}

/// 该 surface format 的 `colorSpace` 是否是引擎声明的那个（不做「合成器给什么用什么」）。
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn accepts_color_space(color_space: ash::vk::ColorSpaceKHR) -> bool {
    color_space == vulkan_color_space()
}

// ── macOS：`CAMetalLayer` 的颜色空间必须是 sRGB（不声明时 P3 屏会过饱和） ─────

/// 是否需要给 `CAMetalLayer` 显式设置 sRGB 颜色空间。
#[cfg(target_os = "macos")]
#[must_use]
pub(crate) fn layer_uses_srgb() -> bool {
    matches!(PRESENTED_COLOR_SPACE, PresentedColorSpace::SrgbRec709)
}

#[cfg(test)]
mod tests {
    use super::{PRESENTED_COLOR_SPACE, PresentedColorSpace};

    /// R06-10：引擎唯一的呈现色彩空间是显式声明的 sRGB（不是「各端推断」）。
    #[test]
    fn the_presented_color_space_is_explicit_srgb() {
        assert_eq!(PRESENTED_COLOR_SPACE, PresentedColorSpace::SrgbRec709);
    }

    /// HDR 是**显式声明的不支持**：枚举只有 sRGB 一个变体（新增 HDR 变体时三端的 `match`
    /// 会因未覆盖而编译失败，不会静默漏端）。
    #[test]
    fn hdr_is_explicitly_unsupported() {
        fn single_variant(color_space: PresentedColorSpace) -> bool {
            match color_space {
                PresentedColorSpace::SrgbRec709 => true,
            }
        }
        assert!(single_variant(PRESENTED_COLOR_SPACE));
    }

    #[cfg(windows)]
    #[test]
    fn windows_maps_to_rgb_full_g22_none_p709() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

        assert_eq!(
            super::dxgi_color_space(),
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
        );
        // `PRESENT` 位 = 0x1（DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT）。
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
