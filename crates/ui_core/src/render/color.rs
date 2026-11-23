use crate::paint::color::{Color, ColorResolver};

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

#[cfg(any(target_os = "macos", test))]
pub(crate) fn background_srgb(color: Color) -> [f32; 4] {
    let linear = ColorResolver.resolve(opaque(color)).components;
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

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
pub(crate) fn background_linear(color: Color) -> [f32; 4] {
    let linear = ColorResolver.resolve(opaque(color)).components;
    [linear[0], linear[1], linear[2], 1.0]
}

#[cfg(test)]
mod tests {
    use super::{background_linear, background_srgb};
    use crate::paint::color::Color;

    fn bytes(rgba: [f32; 4]) -> [u8; 4] {
        rgba.map(|component| (component * 255.0).round() as u8)
    }

    #[test]
    fn the_background_color_is_srgb_encoded_and_never_translucent() {
        let theme = background_srgb(Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]));
        let [red, green, blue, alpha] = bytes(theme);
        assert_eq!([red, green, blue], [11, 14, 19], "8-bit 缓冲收到的字节");
        assert_eq!(alpha, 255);
        assert_eq!(theme[3], 1.0, "背景色恒不透明");

        assert_eq!(bytes(background_srgb(Color::BLACK)), [0, 0, 0, 255]);
        assert_eq!(bytes(background_srgb(Color::WHITE)), [255, 255, 255, 255]);
        assert_eq!(
            bytes(background_srgb(Color::srgb(1.0, 1.0, 1.0, 0.5))),
            [255, 255, 255, 255]
        );

        assert_eq!(
            bytes(background_srgb(Color::linear_srgb(0.5, 0.5, 0.5, 1.0))),
            [188, 188, 188, 255]
        );

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

        for black in [
            Color::oklab(0.0, 0.0, 0.0, 1.0),
            Color::display_p3(0.0, 0.0, 0.0, 0.5),
            Color::oklch(0.0, 0.0, 0.0, 1.0),
        ] {
            let rgba = background_srgb(black);
            assert_eq!(bytes(rgba), [0, 0, 0, 255], "{black:?}");
            assert_eq!(rgba[3], 1.0, "{black:?} 的 alpha 必须是 1");
        }

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
        let [red, green, blue, alpha] =
            background_linear(Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]));
        for (actual, expected) in [(red, 0.003_348), (green, 0.004_391), (blue, 0.006_508)] {
            assert!(
                (actual - expected).abs() < 5e-5,
                "linear 分量 {actual} 应接近 {expected}"
            );
        }
        assert_eq!(alpha, 1.0, "背景色恒不透明");

        assert_eq!(background_linear(Color::WHITE), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(
            background_linear(Color::srgb(1.0, 1.0, 1.0, 0.0)),
            [1.0, 1.0, 1.0, 1.0]
        );
    }
}
