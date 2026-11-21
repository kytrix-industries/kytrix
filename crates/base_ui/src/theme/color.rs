//! 颜色数学：OKLab 混合、明度阶梯、透明度、线性合成与 WCAG 对比度。
//!
//! 引擎只提供「颜色值 + 线性预乘解析/插值」（`ui_core::paint::color`，设计 §5.8）。主题需要的另
//! 一半数学——OKLCH 调色板的 OKLab 混合、`fg.muted = text 30% 混入 canvas` 这类配方、状态色的
//! 淡染、明暗阶梯——按设计 §5.8.1 归应用层，因此在这里实现：
//!
//! - 调色板与角色数据是引擎 [`Color`]（`Oklch` 变体）；混合借助 `color` crate（引擎依赖，
//!   CSS Color 4 实现）在 OKLab 空间做，混完回到 OKLCH 分量再装回引擎 `Color`；
//! - **到 GPU 的颜色只有一条路**：[`ColorResolver`] 把 `Color` 解析成线性预乘 RGBA（挂载期一次）；
//! - 对比度按 WCAG 在**线性亮度**上计算，半透明前景先在**线性预乘**空间合成到底色上——这正是引擎
//!   的合成空间（§5.8.4），因此断言的比值就是屏幕上的比值。

use color::{ColorSpaceTag, DynamicColor, Flags, HueDirection};
use ui_core::paint::color::{Color, ColorResolver};

/// 引擎颜色 → `color` crate 的动态颜色（分量空间 + 独立 alpha）。
fn dynamic(color: Color) -> DynamicColor {
    let (cs, components) = match color {
        Color::Srgb { r, g, b, alpha } => (ColorSpaceTag::Srgb, [r, g, b, alpha]),
        Color::LinearSrgb { r, g, b, alpha } => (ColorSpaceTag::LinearSrgb, [r, g, b, alpha]),
        Color::DisplayP3 { r, g, b, alpha } => (ColorSpaceTag::DisplayP3, [r, g, b, alpha]),
        Color::Oklab { l, a, b, alpha } => (ColorSpaceTag::Oklab, [l, a, b, alpha]),
        Color::Oklch { l, c, h, alpha } => (ColorSpaceTag::Oklch, [l, c, h, alpha]),
    };

    DynamicColor {
        cs,
        flags: Flags::default(),
        components,
    }
}

/// 动态颜色 → 引擎颜色（OKLCH 是主题的规范空间；其它空间先转到 OKLCH）。
fn from_dynamic(color: DynamicColor) -> Color {
    let components = if matches!(color.cs, ColorSpaceTag::Oklch) {
        color.components
    } else {
        color.convert(ColorSpaceTag::Oklch).components
    };

    Color::oklch(components[0], components[1], components[2], components[3])
}

/// OKLCH 分量 `[lightness, chroma, hue, alpha]`（诊断、测试与数据对齐用）。
pub(crate) fn oklch_components(color: Color) -> [f32; 4] {
    let converted = dynamic(color).convert(ColorSpaceTag::Oklch);

    converted.components
}

/// Oklab 分量 `[L, a, b]`（图表系列/恐慌贪婪阶梯的「能否区分」断言用）。
#[cfg(test)]
pub(crate) fn oklab(color: Color) -> [f32; 3] {
    let converted = dynamic(color).convert(ColorSpaceTag::Oklab);

    [
        converted.components[0],
        converted.components[1],
        converted.components[2],
    ]
}

/// `target_weight` 比例的 `from → to` 混合，在 **OKLab** 空间（设计 §6.4：同样的明度一步在两种
/// 外观下读起来一样；两色直线的中点不穿过脏灰）。
pub(crate) fn mix_oklab(from: Color, to: Color, target_weight: f32) -> Color {
    let mixed = dynamic(from)
        .interpolate(dynamic(to), ColorSpaceTag::Oklab, HueDirection::Shorter)
        .eval(target_weight.clamp(0.0, 1.0));

    from_dynamic(mixed)
}

/// 同一个颜色换一个 alpha（颜色分量与空间不变）。
pub(crate) fn with_alpha(color: Color, alpha: f32) -> Color {
    match color {
        Color::Srgb { r, g, b, .. } => Color::srgb(r, g, b, alpha),
        Color::LinearSrgb { r, g, b, .. } => Color::linear_srgb(r, g, b, alpha),
        Color::DisplayP3 { r, g, b, .. } => Color::display_p3(r, g, b, alpha),
        Color::Oklab { l, a, b, .. } => Color::oklab(l, a, b, alpha),
        Color::Oklch { l, c, h, .. } => Color::oklch(l, c, h, alpha),
    }
}

/// 明度方向的一步（chroma 与 hue 不动）：浅色外观变暗、深色外观变亮（设计 §6.4 R2）。
pub(crate) fn step(color: Color, amount: f32, lighter: bool) -> Color {
    let [lightness, chroma, hue, alpha] = oklch_components(color);
    let lightness = if lighter {
        lightness + amount
    } else {
        lightness - amount
    };

    Color::oklch(lightness.clamp(0.0, 1.0), chroma, hue, alpha)
}

/// 状态色/涨跌色 chip 的底色：把颜色几乎全部混进画布（深色外观混得少一点，因为它向上的空间更小）。
pub(crate) fn tint(color: Color, canvas: Color, dark: bool) -> Color {
    mix_oklab(color, canvas, if dark { 0.90 } else { 0.96 })
}

/// 不透明黑加一个 alpha（阴影、遮罩）。
pub(crate) fn black_with_alpha(alpha: f32) -> Color {
    Color::oklch(0.0, 0.0, 0.0, alpha)
}

/// `veil` 合成在 `base` 之上：引擎的合成空间（线性预乘，§5.8.4）里的 source-over。
///
/// 用在「底色已知且不透明」的组件配方上（tonal 按钮的 hover 洗色）：配方在主题加载时把结果算成
/// 一个颜色，运行期只有一种颜色要过渡。返回非预乘的线性颜色。
///
/// **合成空间是用户裁决的视觉基线**：旧实现（`Hsla::blend` → sRGB 编码通道混合）与这里只对
/// 半透明洗色的结果不同；对 tonal 的四个不透明填充，8 位 sRGB 差是 Light hover 7、Light active
/// 12、Dark hover 34、Dark active 43（/255，逐通道最大）。差异表与 golden 在
/// `theme/components.rs` 的测试里，登记在 `spec-deltas/theme.md` §6；M2 视觉基准确认这一条。
pub(crate) fn over(base: Color, veil: Color) -> Color {
    let resolver = ColorResolver;
    let base = resolver.resolve(base).components;
    let veil = resolver.resolve(veil).components;
    let veil_alpha = veil[3];
    let alpha = veil_alpha + base[3] * (1.0 - veil_alpha);
    debug_assert!(alpha > 0.0, "a veil over an opaque base is opaque");
    let channel = |veil: f32, base: f32| (veil + base * (1.0 - veil_alpha)) / alpha;

    Color::linear_srgb(
        channel(veil[0], base[0]),
        channel(veil[1], base[1]),
        channel(veil[2], base[2]),
        alpha,
    )
}

/// WCAG 对比度（`(较亮 + 0.05) / (较暗 + 0.05)`），按**线性亮度**算。
///
/// 半透明前景先在**线性预乘**空间合成到（不透明的）背景上：断言的比值就是引擎合成出来的像素。
/// 旧实现把半透明前景合成在 sRGB 编码空间（gpui `Rgba::blend` 的产物）；本模块改为线性空间，
/// 与引擎的合成本质一致——两者只在半透明前景的对比度上有可解释的差异，不透明的 58 条对比度地板
/// 逐位相同。
pub(crate) fn contrast(foreground: Color, background: Color) -> f32 {
    let resolver = ColorResolver;
    let background = resolver.resolve(background).components;
    let foreground = resolver.resolve(foreground).components;
    debug_assert!(
        background[3] >= 1.0,
        "contrast() expects an opaque background (or one already composited over an opaque one)"
    );
    let alpha = foreground[3];
    let luminance = |color: [f32; 4]| 0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2];
    let foreground = luminance([
        foreground[0] + background[0] * (1.0 - alpha),
        foreground[1] + background[1] * (1.0 - alpha),
        foreground[2] + background[2] * (1.0 - alpha),
        1.0,
    ]);
    let background = luminance(background);
    let (brighter, darker) = if foreground > background {
        (foreground, background)
    } else {
        (background, foreground)
    };

    (brighter + 0.05) / (darker + 0.05)
}

/// 引擎解析后的颜色 → 8 位 sRGB（`[r, g, b, a]`）：主题审计与旧调色板数值对齐的出口。
///
/// 走的是**发布路径**（`ColorResolver`：宽色域色做 CSS Color 4 chroma 二分），所以 `+1` 的容差
/// 覆盖的是 8 位舍入，不是两种 gamut mapping 的差异。
#[cfg(test)]
pub(crate) fn to_srgb8(color: Color) -> [u8; 4] {
    let components = ColorResolver.resolve(color).components;
    let alpha = components[3];
    if alpha <= 0.0 {
        return [0, 0, 0, 0];
    }
    let encode = |linear: f32| {
        let linear = (linear / alpha).clamp(0.0, 1.0);
        let encoded = if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };

        (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
    };

    [
        encode(components[0]),
        encode(components[1]),
        encode(components[2]),
        (alpha * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use ui_core::paint::color::{Color, ColorResolver};

    use super::{
        black_with_alpha, contrast, mix_oklab, oklch_components, over, step, tint, to_srgb8,
        with_alpha,
    };

    #[test]
    fn a_mix_lands_in_oklab_not_in_the_stored_space() {
        let mixed = mix_oklab(
            Color::oklch(0.0, 0.0, 0.0, 1.0),
            Color::oklch(1.0, 0.0, 0.0, 1.0),
            0.5,
        );
        let [lightness, chroma, _, alpha] = oklch_components(mixed);

        assert!(
            (lightness - 0.5).abs() < 1e-4,
            "OKLab 的黑白中点是 L 0.5，不是 L {lightness}"
        );
        assert!(chroma < 1e-4, "两个无彩色颜色的混合必须无彩色：C {chroma}");
        assert!((alpha - 1.0).abs() < 1e-6);
        assert!(matches!(mixed, Color::Oklch { .. }), "混合结果回到 OKLCH");
    }

    #[test]
    fn mix_endpoints_are_exact() {
        let from = Color::oklch(0.3, 0.1, 20.0, 1.0);
        let to = Color::oklch(0.7, 0.05, 200.0, 1.0);

        // 端点回到 OKLCH 时经过一次 OKLab 往返，允许 f32 噪声（不是设计误差）。
        for (actual, expected, name) in [
            (mix_oklab(from, to, 0.0), from, "from"),
            (mix_oklab(from, to, 1.0), to, "to"),
            (mix_oklab(from, to, 2.0), to, "clamped"),
        ] {
            let actual = oklch_components(actual);
            let expected = oklch_components(expected);
            for channel in 0..4 {
                assert!(
                    (actual[channel] - expected[channel]).abs() < 1e-6,
                    "{name} 通道 {channel}: {} vs {}",
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn with_alpha_replaces_only_the_alpha() {
        let color = Color::oklch(0.4, 0.2, 30.0, 1.0);
        let veil = with_alpha(color, 0.25);
        let [lightness, chroma, hue, alpha] = oklch_components(veil);

        assert!((lightness - 0.4).abs() < 1e-6);
        assert!((chroma - 0.2).abs() < 1e-6);
        assert!((hue - 30.0).abs() < 1e-4);
        assert!((alpha - 0.25).abs() < 1e-6);
    }

    #[test]
    fn a_step_moves_only_the_lightness_in_the_appearance_direction() {
        let color = Color::oklch(0.5, 0.1, 200.0, 1.0);
        let lighter = step(color, 0.075, true);
        let darker = step(color, 0.075, false);

        assert!((oklch_components(lighter)[0] - 0.575).abs() < 1e-6);
        assert!((oklch_components(darker)[0] - 0.425).abs() < 1e-6);
        assert!((oklch_components(lighter)[1] - 0.1).abs() < 1e-6);
        assert!((oklch_components(lighter)[2] - 200.0).abs() < 1e-4);
        // 越界的 L 被夹紧。
        assert!(oklch_components(step(color, 5.0, true))[0] <= 1.0);
        assert!(oklch_components(step(color, 5.0, false))[0] >= 0.0);
    }

    #[test]
    fn a_tint_lands_close_to_the_canvas() {
        let canvas = Color::oklch(0.978, 0.0, 0.0, 1.0);
        let tinted = tint(Color::oklch(0.53, 0.19, 26.0, 1.0), canvas, false);
        let [_, chroma, _, _] = oklch_components(tinted);
        let canvas_chroma = oklch_components(canvas)[1];

        assert!(chroma < 0.02, "96% 混入画布后只剩一点彩度：C {chroma}");
        assert!((oklch_components(tinted)[0] - 0.958).abs() < 0.01);
        assert!(chroma > canvas_chroma, "仍是可辨认的淡染");
    }

    #[test]
    fn a_veil_over_an_opaque_base_keeps_the_base_alpha() {
        let base = Color::oklch(0.5, 0.1, 200.0, 1.0);
        let veil = with_alpha(Color::oklch(0.0, 0.0, 0.0, 1.0), 0.5);
        let composited = over(base, veil);

        assert!((oklch_components(composited)[3] - 1.0).abs() < 1e-6);
        assert!(
            oklch_components(composited)[0] < oklch_components(base)[0],
            "黑色 veil 必须把底色压暗"
        );
    }

    /// `over` 的合成空间就是引擎的：线性预乘里的 source-over，不是「先编码成 sRGB 再加权」。
    ///
    /// 两种算法对半透明洗色的结果不同（线性合成在 sRGB 编码里更亮），这是 P2-1a 用户裁决接受的
    /// 视觉变化；本测试把它钉在**算法**这一层，和主题数据无关——把 `over` 改回 sRGB 混合就会红。
    #[test]
    fn a_veil_over_an_opaque_base_is_source_over_in_linear_space() {
        let resolver = ColorResolver;
        let base = Color::srgb(0.2, 0.3, 0.4, 1.0);
        let veil = Color::srgb(0.9, 0.9, 0.9, 0.25);
        let base_linear = resolver.resolve(base).components;
        let veil_linear = resolver.resolve(veil).components;
        let alpha = veil_linear[3];

        let composited = resolver.resolve(over(base, veil)).components;
        for channel in 0..3 {
            let expected = veil_linear[channel] + base_linear[channel] * (1.0 - alpha);
            assert!(
                (composited[channel] - expected).abs() < 1e-6,
                "线性 source-over：通道 {channel} 期望 {expected}，实际 {}",
                composited[channel]
            );
        }

        // 同一对颜色在**旧的** sRGB 编码空间混合是 [96, 115, 134]；线性空间是 [130, 137, 148]。
        // 这一行就是用户裁决的差异在算法层的最小复现（主题里的差异表见 `theme/components.rs`）。
        let old_srgb_blend = |channel: usize| {
            let encode = |linear: f32| {
                if linear <= 0.003_130_8 {
                    linear * 12.92
                } else {
                    1.055 * linear.powf(1.0 / 2.4) - 0.055
                }
            };
            let base_encoded = encode(base_linear[channel]);
            let veil_encoded = encode(veil_linear[channel] / alpha);
            (base_encoded * (1.0 - alpha) + veil_encoded * alpha) * 255.0
        };
        let new_srgb8 = to_srgb8(over(base, veil));
        for (channel, actual) in new_srgb8.iter().take(3).enumerate() {
            let old = old_srgb_blend(channel).round();
            assert!(
                f32::from(*actual) > old + 5.0,
                "线性合成必须明显比旧的 sRGB 混合亮：通道 {channel} 新 {actual} vs 旧 {old}"
            );
        }
        assert_eq!(
            [new_srgb8[0], new_srgb8[1], new_srgb8[2]],
            [130, 137, 148],
            "线性空间 source-over 的逐值基线"
        );
    }

    #[test]
    fn contrast_is_21_between_black_and_white_and_1_for_one_color() {
        let black = Color::oklch(0.0, 0.0, 0.0, 1.0);
        let white = Color::oklch(1.0, 0.0, 0.0, 1.0);

        assert!((contrast(white, black) - 21.0).abs() < 0.01);
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(black, black) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn contrast_composites_a_translucent_foreground_in_linear_space() {
        let background = Color::oklch(1.0, 0.0, 0.0, 1.0);
        let half_black = black_with_alpha(0.5);
        let ratio = contrast(half_black, background);
        // 50% 黑在**线性**空间合成到白上 = 线性 0.5（不是 sRGB 0.5，也不是 OKLCH L 0.5）。
        let reference = contrast(Color::linear_srgb(0.5, 0.5, 0.5, 1.0), background);

        assert!(
            (ratio - reference).abs() < 1e-3,
            "线性空间合成：{ratio} vs {reference}"
        );
        assert!(
            (ratio - 1.909).abs() < 1e-3,
            "白底上 50% 黑的对比度 ≈ 1.91:1（线性亮度 0.5），实际 {ratio}"
        );
        assert!((1.0..21.0).contains(&ratio));
    }

    #[test]
    fn srgb8_comes_from_the_engine_resolver() {
        assert_eq!(
            to_srgb8(Color::oklch(1.0, 0.0, 0.0, 1.0)),
            [255, 255, 255, 255]
        );
        assert_eq!(to_srgb8(Color::oklch(0.0, 0.0, 0.0, 1.0)), [0, 0, 0, 255]);
        assert_eq!(to_srgb8(Color::TRANSPARENT), [0, 0, 0, 0]);
        // sRGB 0.5 的中灰在 OKLCH 不是 L 0.5，但 8 位往返必须回到 128。
        assert_eq!(
            to_srgb8(Color::srgb(0.5, 0.5, 0.5, 1.0)),
            [128, 128, 128, 255]
        );
        // 半透明：8 位 alpha 也要跟着走。
        assert_eq!(
            to_srgb8(Color::srgb(1.0, 1.0, 1.0, 0.5)),
            [255, 255, 255, 128]
        );
    }

    /// 引擎解析：sRGB → 线性预乘；alpha 参与预乘。
    #[test]
    fn the_engine_resolver_premultiplies_alpha_and_unpremultiplies_nothing_else() {
        let resolver = ColorResolver;
        let resolved = resolver.resolve(Color::srgb(1.0, 0.5, 0.0, 0.5));

        assert!((resolved.components[0] - 0.5).abs() < 1e-6, "r·a");
        assert!(
            (resolved.components[1] - 0.107_020_57).abs() < 1e-5,
            "线性 g = 0.21404 · 0.5"
        );
        assert_eq!(resolved.components[2], 0.0);
        assert_eq!(resolved.components[3], 0.5);
    }

    /// OKLCH 输入：引擎做 sRGB gamut mapping（chroma 二分），结果在 [0,1] 内。
    #[test]
    fn the_engine_resolver_accepts_oklch_and_maps_it_into_gamut() {
        let resolver = ColorResolver;
        let in_gamut = resolver.resolve(Color::oklch(0.5, 0.0, 0.0, 1.0));
        assert!(
            (in_gamut.components[0] - 0.125).abs() < 2e-3,
            "OKLCH/OKLab L 0.5 的灰 ≈ 线性 0.125：{}",
            in_gamut.components[0]
        );

        let saturated = resolver.resolve(Color::oklch(0.9, 0.4, 140.0, 1.0));
        for channel in &saturated.components[..3] {
            assert!(
                (0.0..=1.0).contains(channel),
                "gamut mapping 后越界：{channel}"
            );
        }
    }
}
