//! 窗口背景色：引擎颜色 → 平台编码（共享层；Windows / macOS / Linux 消费）。
//!
//! 背景色没有透明度（alpha 恒 1），但两条呈现路径需要**不同编码**：
//!
//! - **sRGB 编码**（[`background_srgb`]）：DXGI 的 `SetBackgroundColor` 在 UNORM flip swapchain 上
//!   按 sRGB 字节直写（S-139 实测：字面量 linear 值只会得到 `linear × 255`，官方 "recommend
//!   linear" 不适用于该路径）；`NSColor` / `CGColor` 的 sRGB 颜色空间同样要编码后的分量。
//! - **linear**（[`background_linear`]）：Metal 的 `BGRA8Unorm_sRGB` / Vulkan 的 `*_SRGB` 渲染目标
//!   在 clear 时按 linear 值写入（硬件做 sRGB 编码），传编码值会偏亮。macOS（persist clear）与
//!   Linux（Vulkan persist clear）走这条路径。
//!
//! 两条路径都从「alpha 置 1 + [`ColorResolver::resolve`]」的同一份结果出发，颜色语义只有一处。

use crate::paint::color::{Color, ColorResolver};

/// 把颜色变成不透明：只重建 alpha（不改 RGB）。
///
/// [`ColorResolver::resolve`] 输出的是**预乘**值：直接用半透明颜色会让半透明白变灰，因此背景色
/// 必须先置 `alpha = 1.0`（`with_opacity(1.0)` 只乘 alpha，**不会**把 0.5 变成 1）。
fn opaque(color: Color) -> Color {
    match color {
        Color::Srgb { r, g, b, .. } => Color::Srgb {
            r,
            g,
            b,
            alpha: 1.0,
        },
        Color::LinearSrgb { r, g, b, .. } => Color::LinearSrgb {
            r,
            g,
            b,
            alpha: 1.0,
        },
        Color::DisplayP3 { r, g, b, .. } => Color::DisplayP3 {
            r,
            g,
            b,
            alpha: 1.0,
        },
        Color::Oklab { l, a, b, .. } => Color::Oklab {
            l,
            a,
            b,
            alpha: 1.0,
        },
        Color::Oklch { l, c, h, .. } => Color::Oklch {
            l,
            c,
            h,
            alpha: 1.0,
        },
    }
}

/// 引擎颜色 → **sRGB 编码**的 RGBA（未预乘、alpha 恒 1）。
///
/// 给按 sRGB 字节直写的平台入口用：Windows `SetBackgroundColor`、macOS `NSColor` / `CGColor`。
/// Linux（Vulkan sRGB attachment）走 [`background_linear`]，不使用本函数，因此它在 Linux 的
/// 产品构建里不参与编译（测试构建仍覆盖它）。
/// alpha = 1 ⇒ `resolve` 的线性分量就是未预乘的 linear RGB（已 clamp 到 `[0, 1]`）；再按
/// IEC 61966-2-1 的传递函数编码。
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn background_srgb(color: Color) -> [f32; 4] {
    let linear = ColorResolver::default().resolve(opaque(color)).components;
    let encode = |component: f32| {
        let c = component.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    [encode(linear[0]), encode(linear[1]), encode(linear[2]), 1.0]
}

/// 引擎颜色 → **linear** 的 RGBA（alpha 恒 1），给 sRGB 渲染目标的 clear 用。
///
/// macOS 的 Metal persist clear 与 Linux 的 Vulkan persist clear 都用它（硬件按 sRGB 渲染目标做
/// 编码）。Windows 的 `SetBackgroundColor` 走 [`background_srgb`]（按字节直写），因此它在 Windows
/// 的产品构建里不参与编译（测试构建仍覆盖它）。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn background_linear(color: Color) -> [f32; 4] {
    let linear = ColorResolver::default().resolve(opaque(color)).components;
    [linear[0], linear[1], linear[2], 1.0]
}

#[cfg(test)]
mod tests {
    use super::{background_linear, background_srgb};
    use crate::paint::color::Color;

    /// `round(component × 255)`：8-bit 缓冲实际收到的字节。
    fn bytes(rgba: [f32; 4]) -> [u8; 4] {
        rgba.map(|component| (component * 255.0).round() as u8)
    }

    #[test]
    fn the_background_color_is_srgb_encoded_and_never_translucent() {
        // 实测 S-139：本路径的 UNORM flip swapchain 把 float 原样写进 8-bit 缓冲，所以必须传
        // sRGB 编码分量；主题色 `#0B0E13` 只有编码后才会落成 (11, 14, 19)。
        let theme = background_srgb(Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]));
        let [red, green, blue, alpha] = bytes(theme);
        assert_eq!([red, green, blue], [11, 14, 19], "8-bit 缓冲收到的字节");
        assert_eq!(alpha, 255);
        assert_eq!(theme[3], 1.0, "背景色恒不透明");

        assert_eq!(bytes(background_srgb(Color::BLACK)), [0, 0, 0, 255]);
        assert_eq!(bytes(background_srgb(Color::WHITE)), [255, 255, 255, 255]);
        // 半透明白：先不透明化（`with_opacity(1.0)` 不会把 0.5 变成 1，预乘会变灰）。
        assert_eq!(
            bytes(background_srgb(Color::srgb(1.0, 1.0, 1.0, 0.5))),
            [255, 255, 255, 255]
        );

        // linear 0.5 → sRGB encode 0.7354 → 188（原样写入只会得到 128）。
        assert_eq!(
            bytes(background_srgb(Color::linear_srgb(0.5, 0.5, 0.5, 1.0))),
            [188, 188, 188, 255]
        );

        // 5 个变体都必须按不透明色处理：半透明白若漏掉 `alpha: 1.0`，预乘后会变灰（188）。
        for white in [
            Color::srgb(1.0, 1.0, 1.0, 0.5),
            Color::linear_srgb(1.0, 1.0, 1.0, 0.5),
            Color::display_p3(1.0, 1.0, 1.0, 0.5),
            Color::oklab(1.0, 0.0, 0.0, 0.5),
            Color::oklch(1.0, 0.0, 0.0, 0.5),
        ] {
            let rgba = background_srgb(white);
            assert_eq!(
                bytes(rgba),
                [255, 255, 255, 255],
                "{white:?} 必须是不透明白"
            );
            assert_eq!(rgba[3], 1.0, "{white:?} 的 alpha 必须是 1");
        }

        // 宽色域变体的已知黑端点：与 sRGB 同样的字节结果 + alpha 恒 1。
        for black in [
            Color::oklab(0.0, 0.0, 0.0, 1.0),
            Color::display_p3(0.0, 0.0, 0.0, 0.5),
            Color::oklch(0.0, 0.0, 0.0, 1.0),
        ] {
            let rgba = background_srgb(black);
            assert_eq!(bytes(rgba), [0, 0, 0, 255], "{black:?}");
            assert_eq!(rgba[3], 1.0, "{black:?} 的 alpha 必须是 1");
        }

        // 分量恒在 `[0, 1]`（越界 / NaN 输入由 resolver 与 encode 收敛）。
        for color in [
            Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
            Color::BLACK,
            Color::WHITE,
            Color::srgb(2.0, -1.0, f32::NAN, 1.0),
            Color::display_p3(1.0, 0.0, 0.0, 0.25),
            Color::oklch(0.5, 0.4, 30.0, 0.0),
        ] {
            for component in background_srgb(color) {
                assert!((0.0..=1.0).contains(&component), "{color:?} → {component}");
            }
        }
    }

    #[test]
    fn the_linear_clear_color_is_the_resolved_linear_value() {
        // 主题色 `#0B0E13` 的 linear 分量（IEC 61966-2-1 sRGB 传递函数的逆）。
        let [red, green, blue, alpha] =
            background_linear(Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]));
        for (actual, expected) in [(red, 0.003_348), (green, 0.004_391), (blue, 0.006_508)] {
            assert!(
                (actual - expected).abs() < 5e-5,
                "linear 分量 {actual} 应接近 {expected}"
            );
        }
        assert_eq!(alpha, 1.0, "背景色恒不透明");

        // linear 白必须是 1：把 sRGB 编码值当 clear 会偏亮（255 而不是 1）。
        assert_eq!(background_linear(Color::WHITE), [1.0, 1.0, 1.0, 1.0]);
        // 半透明输入先不透明化：alpha 不影响 RGB。
        assert_eq!(
            background_linear(Color::srgb(1.0, 1.0, 1.0, 0.0)),
            [1.0, 1.0, 1.0, 1.0]
        );
    }
}
