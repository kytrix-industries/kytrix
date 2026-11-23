use color::{ColorSpaceTag, DynamicColor, Flags};

const GAMUT_EPSILON: f32 = 1e-6;
const GAMUT_BISECTION_STEPS: u32 = 24;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    Srgb { r: f32, g: f32, b: f32, alpha: f32 },
    LinearSrgb { r: f32, g: f32, b: f32, alpha: f32 },
    DisplayP3 { r: f32, g: f32, b: f32, alpha: f32 },
    Oklab { l: f32, a: f32, b: f32, alpha: f32 },
    Oklch { l: f32, c: f32, h: f32, alpha: f32 },
}

impl Color {
    pub const TRANSPARENT: Self = Self::Srgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        alpha: 0.0,
    };
    pub const BLACK: Self = Self::Srgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        alpha: 1.0,
    };
    pub const WHITE: Self = Self::Srgb {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        alpha: 1.0,
    };

    #[must_use]
    pub const fn srgb(r: f32, g: f32, b: f32, alpha: f32) -> Self {
        Self::Srgb { r, g, b, alpha }
    }

    #[must_use]
    pub const fn linear_srgb(r: f32, g: f32, b: f32, alpha: f32) -> Self {
        Self::LinearSrgb { r, g, b, alpha }
    }

    #[must_use]
    pub const fn display_p3(r: f32, g: f32, b: f32, alpha: f32) -> Self {
        Self::DisplayP3 { r, g, b, alpha }
    }

    #[must_use]
    pub const fn oklab(l: f32, a: f32, b: f32, alpha: f32) -> Self {
        Self::Oklab { l, a, b, alpha }
    }

    #[must_use]
    pub const fn oklch(l: f32, c: f32, h: f32, alpha: f32) -> Self {
        Self::Oklch { l, c, h, alpha }
    }

    #[must_use]
    pub fn with_opacity(self, factor: f32) -> Self {
        let factor = if factor.is_nan() {
            0.0
        } else {
            factor.clamp(0.0, 1.0)
        };
        let alpha = (self.alpha() * factor).clamp(0.0, 1.0);
        match self {
            Self::Srgb { r, g, b, .. } => Self::Srgb { r, g, b, alpha },
            Self::LinearSrgb { r, g, b, .. } => Self::LinearSrgb { r, g, b, alpha },
            Self::DisplayP3 { r, g, b, .. } => Self::DisplayP3 { r, g, b, alpha },
            Self::Oklab { l, a, b, .. } => Self::Oklab { l, a, b, alpha },
            Self::Oklch { l, c, h, .. } => Self::Oklch { l, c, h, alpha },
        }
    }

    #[must_use]
    pub fn is_transparent(self) -> bool {
        let alpha = self.alpha();
        alpha <= 0.0 || alpha.is_nan()
    }

    const fn alpha(self) -> f32 {
        match self {
            Self::Srgb { alpha, .. }
            | Self::LinearSrgb { alpha, .. }
            | Self::DisplayP3 { alpha, .. }
            | Self::Oklab { alpha, .. }
            | Self::Oklch { alpha, .. } => alpha,
        }
    }

    #[must_use]
    pub fn from_rgba8(rgba: [u8; 4]) -> Self {
        let [r, g, b, a] = rgba;
        Self::Srgb {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            alpha: f32::from(a) / 255.0,
        }
    }

    fn dynamic_parts(self) -> (ColorSpaceTag, [f32; 4]) {
        match self {
            Self::Srgb { r, g, b, alpha } => (ColorSpaceTag::Srgb, [r, g, b, alpha]),
            Self::LinearSrgb { r, g, b, alpha } => (ColorSpaceTag::LinearSrgb, [r, g, b, alpha]),
            Self::DisplayP3 { r, g, b, alpha } => (ColorSpaceTag::DisplayP3, [r, g, b, alpha]),
            Self::Oklab { l, a, b, alpha } => (ColorSpaceTag::Oklab, [l, a, b, alpha]),
            Self::Oklch { l, c, h, alpha } => (ColorSpaceTag::Oklch, [l, c, h, alpha]),
        }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::TRANSPARENT
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinearPremulRgba {
    pub components: [f32; 4],
}

impl LinearPremulRgba {
    pub const TRANSPARENT: Self = Self {
        components: [0.0; 4],
    };

    #[must_use]
    pub const fn from_components(components: [f32; 4]) -> Self {
        Self { components }
    }

    #[must_use]
    pub const fn to_array(self) -> [f32; 4] {
        self.components
    }

    #[must_use]
    pub fn is_opaque(self) -> bool {
        self.components[3] >= 1.0
    }

    #[must_use]
    pub fn is_transparent(self) -> bool {
        self.components[3] <= 0.0
    }

    #[must_use]
    pub fn lerp(from: Self, to: Self, t: f32) -> Self {
        let t = clamp_01(t);
        let mut components = [0.0f32; 4];
        for (index, component) in components.iter_mut().enumerate() {
            let start = from.components[index];
            *component = start + (to.components[index] - start) * t;
        }
        Self { components }
    }
}

impl std::ops::Mul<f32> for LinearPremulRgba {
    type Output = Self;

    fn mul(self, rhs: f32) -> Self {
        Self {
            components: [
                self.components[0] * rhs,
                self.components[1] * rhs,
                self.components[2] * rhs,
                self.components[3] * rhs,
            ],
        }
    }
}

pub(crate) struct ResolveCache {
    entries: [Option<(ColorBits, LinearPremulRgba)>; Self::SLOTS],
}

type ColorBits = (u8, [u32; 4]);

impl ResolveCache {
    const SLOTS: usize = 64;

    pub(crate) const fn new() -> Self {
        Self {
            entries: [None; Self::SLOTS],
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries = [None; Self::SLOTS];
    }

    pub(crate) fn resolve(&mut self, resolver: &ColorResolver, color: Color) -> LinearPremulRgba {
        let bits = color_bits(color);
        let slot = (bits.0 as usize
            ^ bits.1[0].rotate_left(7) as usize
            ^ bits.1[1].rotate_left(13) as usize
            ^ bits.1[2].rotate_left(19) as usize
            ^ bits.1[3].rotate_left(25) as usize)
            % Self::SLOTS;
        if let Some((cached_bits, resolved)) = self.entries[slot]
            && cached_bits == bits
        {
            return resolved;
        }
        let resolved = resolver.resolve(color);
        self.entries[slot] = Some((bits, resolved));
        resolved
    }
}

fn color_bits(color: Color) -> ColorBits {
    let (space, parts) = color.dynamic_parts();
    (
        match space {
            ColorSpaceTag::Srgb => 0,
            ColorSpaceTag::LinearSrgb => 1,
            ColorSpaceTag::DisplayP3 => 2,
            ColorSpaceTag::Oklab => 3,
            ColorSpaceTag::Oklch => 4,
            _ => u8::MAX,
        },
        [
            parts[0].to_bits(),
            parts[1].to_bits(),
            parts[2].to_bits(),
            parts[3].to_bits(),
        ],
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorResolver;

impl ColorResolver {
    #[must_use]
    pub fn resolve(&self, color: Color) -> LinearPremulRgba {
        let dynamic = normalized_dynamic(color);
        let alpha = dynamic.components[3];
        let linear = resolve_linear(dynamic);
        premultiply(linear, alpha)
    }

    #[must_use]
    pub fn lerp(&self, from: Color, to: Color, t: f32) -> LinearPremulRgba {
        LinearPremulRgba::lerp(self.resolve(from), self.resolve(to), t)
    }

    #[must_use]
    pub fn lerp_color(&self, from: Color, to: Color, t: f32) -> Color {
        let mixed = self.lerp(from, to, t).components;
        let alpha = mixed[3];
        if alpha <= 0.0 {
            return Color::linear_srgb(0.0, 0.0, 0.0, 0.0);
        }
        Color::linear_srgb(mixed[0] / alpha, mixed[1] / alpha, mixed[2] / alpha, alpha)
    }
}

fn premultiply(linear: [f32; 3], alpha: f32) -> LinearPremulRgba {
    LinearPremulRgba {
        components: [
            linear[0] * alpha,
            linear[1] * alpha,
            linear[2] * alpha,
            alpha,
        ],
    }
}

fn normalized_dynamic(color: Color) -> DynamicColor {
    let (cs, parts) = color.dynamic_parts();
    let alpha = clamp_01(parts[3]);
    let components = if matches!(cs, ColorSpaceTag::Oklch) {
        [
            clamp_01(parts[0]),
            clamp_nonneg(parts[1]),
            hue_deg(parts[2]),
            alpha,
        ]
    } else {
        [parts[0], parts[1], parts[2], alpha]
    };
    DynamicColor {
        cs,
        flags: Flags::default(),
        components,
    }
}

fn resolve_linear(dynamic: DynamicColor) -> [f32; 3] {
    if matches!(dynamic.cs, ColorSpaceTag::Srgb | ColorSpaceTag::LinearSrgb) {
        let linear = to_linear_srgb(dynamic);
        return [
            clamp_01(linear[0]),
            clamp_01(linear[1]),
            clamp_01(linear[2]),
        ];
    }
    gamut_mapped_linear(dynamic)
}

fn gamut_mapped_linear(dynamic: DynamicColor) -> [f32; 3] {
    let oklch = dynamic.convert(ColorSpaceTag::Oklch);
    let l = clamp_01(oklch.components[0]);
    let c = clamp_nonneg(oklch.components[1]);
    let h = hue_deg(oklch.components[2]);
    let chroma = if in_srgb_gamut(l, c, h) {
        c
    } else {
        bisect_chroma(l, c, h)
    };
    let linear = oklch_to_linear_srgb(l, chroma, h);
    [
        clamp_01(linear[0]),
        clamp_01(linear[1]),
        clamp_01(linear[2]),
    ]
}

fn bisect_chroma(l: f32, c: f32, h: f32) -> f32 {
    let mut low = 0.0_f32;
    let mut high = c;
    for _ in 0..GAMUT_BISECTION_STEPS {
        let mid = 0.5 * (low + high);
        if in_srgb_gamut(l, mid, h) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn to_linear_srgb(dynamic: DynamicColor) -> [f32; 3] {
    let linear = dynamic.convert(ColorSpaceTag::LinearSrgb);
    [
        linear.components[0],
        linear.components[1],
        linear.components[2],
    ]
}

fn oklch_to_linear_srgb(l: f32, c: f32, h: f32) -> [f32; 3] {
    to_linear_srgb(DynamicColor {
        cs: ColorSpaceTag::Oklch,
        flags: Flags::default(),
        components: [l, c, h, 1.0],
    })
}

fn in_srgb_gamut(l: f32, c: f32, h: f32) -> bool {
    oklch_to_linear_srgb(l, c, h)
        .iter()
        .all(|value| value.is_finite() && *value >= -GAMUT_EPSILON && *value <= 1.0 + GAMUT_EPSILON)
}

fn clamp_01(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn clamp_nonneg(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn hue_deg(value: f32) -> f32 {
    if value.is_finite() {
        value.rem_euclid(360.0)
    } else {
        0.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorBrush {
    pub color: Color,
}

impl Default for ColorBrush {
    fn default() -> Self {
        Self {
            color: Color::BLACK,
        }
    }
}

impl From<Color> for ColorBrush {
    fn from(color: Color) -> Self {
        Self { color }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use color::{AlphaColor, ColorSpaceTag, DynamicColor, Flags};

    use super::{
        Color, ColorBrush, ColorResolver, GAMUT_BISECTION_STEPS, LinearPremulRgba, ResolveCache,
        clamp_01, clamp_nonneg, hue_deg,
    };

    fn resolver() -> ColorResolver {
        ColorResolver
    }

    fn linear_of(color: Color) -> [f32; 3] {
        let resolved = resolver().resolve(color);
        let alpha = resolved.components[3];
        [
            resolved.components[0] / alpha,
            resolved.components[1] / alpha,
            resolved.components[2] / alpha,
        ]
    }

    fn oklch_of(color: Color) -> [f32; 3] {
        let linear = linear_of(color);
        let back = DynamicColor {
            cs: ColorSpaceTag::LinearSrgb,
            flags: Flags::default(),
            components: [linear[0], linear[1], linear[2], 1.0],
        }
        .convert(ColorSpaceTag::Oklch);
        [back.components[0], back.components[1], back.components[2]]
    }

    #[test]
    fn resolve_srgb_is_linear_buffer() {
        let resolved = resolver().resolve(Color::srgb(0.5, 0.5, 0.5, 1.0));
        let expected = 0.214_041_14_f32;
        for value in &resolved.components[..3] {
            assert!((value - expected).abs() < 1e-4, "got {value}");
        }
        assert!((resolved.components[3] - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_alpha_is_clamped() {
        let low = resolver().resolve(Color::srgb(1.0, 1.0, 1.0, -1.0));
        assert!(low.components[3].abs() < f32::EPSILON);
        let high = resolver().resolve(Color::srgb(1.0, 1.0, 1.0, 2.0));
        assert!((high.components[3] - 1.0).abs() < f32::EPSILON);
        let nan = resolver().resolve(Color::srgb(1.0, 1.0, 1.0, f32::NAN));
        assert!(nan.components[3].abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_premultiplies() {
        let resolved = resolver().resolve(Color::srgb(1.0, 1.0, 1.0, 0.5));
        assert!((resolved.components[0] - 0.5).abs() < 1e-6);
        assert!((resolved.components[3] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn linear_premul_opacity_flags() {
        let opaque = LinearPremulRgba::from_components([0.2, 0.4, 0.6, 1.0]);
        assert!(opaque.is_opaque());
        assert!(!opaque.is_transparent());
        let nearly = LinearPremulRgba::from_components([0.2, 0.4, 0.6, 0.999]);
        assert!(!nearly.is_opaque());
        let clear = LinearPremulRgba::TRANSPARENT;
        assert!(clear.is_transparent());
        assert!(!clear.is_opaque());
        assert_eq!(opaque.to_array(), [0.2, 0.4, 0.6, 1.0]);
    }

    #[test]
    fn opacity_mul_scales_all_components() {
        let color = LinearPremulRgba::from_components([0.4, 0.6, 0.8, 0.8]);
        let half = color * 0.5;
        assert_eq!(half.components, [0.2, 0.3, 0.4, 0.4]);
    }

    #[test]
    fn resolve_linear_srgb_variant_is_passthrough() {
        let resolved = resolver().resolve(Color::linear_srgb(0.25, 0.5, 0.75, 1.0));
        assert!((resolved.components[0] - 0.25).abs() < 1e-6);
        assert!((resolved.components[1] - 0.5).abs() < 1e-6);
        assert!((resolved.components[2] - 0.75).abs() < 1e-6);
        let clamped = resolver().resolve(Color::linear_srgb(1.5, -1.0, 0.5, 1.0));
        assert_eq!(clamped.components[..3], [1.0, 0.0, 0.5]);
    }

    #[test]
    fn resolve_oklab_variant() {
        let resolved = resolver().resolve(Color::oklab(0.5, 0.0, 0.0, 1.0));
        for value in &resolved.components[..3] {
            assert!((value - 0.125).abs() < 2e-3, "got {value}");
        }
    }

    #[test]
    fn oklch_in_gamut_is_passthrough() {
        let color = Color::oklch(0.6, 0.10, 30.0, 1.0);
        let expected = DynamicColor {
            cs: ColorSpaceTag::Oklch,
            flags: Flags::default(),
            components: [0.6, 0.10, 30.0, 1.0],
        }
        .convert(ColorSpaceTag::LinearSrgb);
        let resolved = resolver().resolve(color);
        for index in 0..3 {
            assert!(
                (resolved.components[index] - expected.components[index]).abs() < 1e-6,
                "channel {index}"
            );
        }
    }

    #[test]
    fn oklch_out_of_gamut_reduces_chroma() {
        let color = Color::oklch(0.9, 0.40, 140.0, 1.0);
        let linear = linear_of(color);
        for value in linear {
            assert!((0.0..=1.0).contains(&value), "channel {value}");
        }
        let back = oklch_of(color);
        assert!(back[1] < 0.40, "chroma {}", back[1]);
    }

    #[test]
    fn gamut_mapping_is_deterministic() {
        let color = Color::oklch(0.85, 0.35, 200.0, 0.7);
        let first = resolver().resolve(color).components;
        let second = resolver().resolve(color).components;
        for index in 0..4 {
            assert_eq!(
                first[index].to_bits(),
                second[index].to_bits(),
                "channel {index}"
            );
        }
        assert_eq!(GAMUT_BISECTION_STEPS, 24);
    }

    #[test]
    fn oklch_lightness_clamped() {
        let dark = resolver().resolve(Color::oklch(-0.5, 0.0, 0.0, 1.0));
        assert!(dark.components[..3].iter().all(|value| *value <= 1e-3));
        let bright = resolver().resolve(Color::oklch(1.5, 0.0, 0.0, 1.0));
        assert!(bright.components[..3].iter().all(|value| *value >= 0.999));
    }

    #[test]
    fn display_p3_red_maps_in_gamut() {
        let resolved = resolver().resolve(Color::display_p3(1.0, 0.0, 0.0, 1.0));
        for value in &resolved.components[..3] {
            assert!((0.0..=1.0).contains(value), "channel {value}");
        }
        assert!(
            resolved.components[0] >= 0.99,
            "r {}",
            resolved.components[0]
        );
    }

    #[test]
    fn lerp_is_linear_premultiplied() {
        let mid = resolver().lerp(Color::BLACK, Color::WHITE, 0.5);
        for value in &mid.components[..3] {
            assert!((value - 0.5).abs() < 1e-6, "channel {value}");
        }
        assert!((mid.components[3] - 1.0).abs() < f32::EPSILON);
        assert_eq!(
            resolver().lerp(Color::BLACK, Color::WHITE, 0.0).components,
            resolver().resolve(Color::BLACK).components
        );
        assert_eq!(
            resolver().lerp(Color::BLACK, Color::WHITE, 1.0).components,
            resolver().resolve(Color::WHITE).components
        );
    }

    #[test]
    fn lerp_has_no_color_leak_from_transparent_endpoints() {
        let start = resolver().lerp(
            Color::srgb(1.0, 0.0, 0.0, 0.0),
            Color::srgb(0.0, 0.0, 1.0, 1.0),
            0.0,
        );
        assert!(start.components[3].abs() < f32::EPSILON);
        for value in &start.components[..3] {
            assert!(value.abs() < 1e-6, "channel {value}");
        }
        let end = resolver().lerp(
            Color::srgb(1.0, 0.0, 0.0, 0.0),
            Color::srgb(0.0, 0.0, 1.0, 1.0),
            1.0,
        );
        assert!((end.components[2] - 1.0).abs() < 1e-6);
        assert!(end.components[0].abs() < 1e-6);
    }

    #[test]
    fn lerp_clamps_t() {
        let from = Color::srgb(0.1, 0.2, 0.3, 1.0);
        let to = Color::oklch(0.8, 0.1, 120.0, 1.0);
        let low = resolver().lerp(from, to, -1.0).components;
        let zero = resolver().lerp(from, to, 0.0).components;
        for index in 0..4 {
            assert_eq!(low[index].to_bits(), zero[index].to_bits());
        }
        let high = resolver().lerp(from, to, 2.0).components;
        let one = resolver().lerp(from, to, 1.0).components;
        for index in 0..4 {
            assert_eq!(high[index].to_bits(), one[index].to_bits());
        }
        let nan = resolver().lerp(from, to, f32::NAN).components;
        for index in 0..4 {
            assert!(nan[index].is_finite(), "NaN 的 t 必须收敛：{nan:?}");
        }
    }

    #[test]
    fn lerp_endpoints_match_resolve() {
        let colors = [
            Color::display_p3(0.1, 0.9, 0.2, 1.0),
            Color::display_p3(1.0, 0.0, 0.0, 0.6),
            Color::srgb(0.2, 0.4, 0.6, 1.0),
            Color::oklch(0.7, 0.2, 140.0, 1.0),
        ];
        let other = Color::oklch(0.5, 0.1, 20.0, 0.8);
        for color in colors {
            let expected = resolver().resolve(color).components;
            for t in [0.0_f32, 1.0] {
                let (from, to) = if t == 0.0 {
                    (color, other)
                } else {
                    (other, color)
                };
                let actual = resolver().lerp(from, to, t).components;
                for index in 0..4 {
                    assert!(
                        (actual[index] - expected[index]).abs() < 1e-6,
                        "color={color:?} t={t} channel={index}: {} vs {}",
                        actual[index],
                        expected[index]
                    );
                }
            }
        }
    }

    #[test]
    fn lerp_blends_premultiplied_alpha() {
        let mid = resolver().lerp(Color::srgb(1.0, 1.0, 1.0, 0.5), Color::BLACK, 0.5);
        for value in &mid.components[..3] {
            assert!((value - 0.25).abs() < 1e-6, "channel {value}");
        }
        assert!((mid.components[3] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn lerp_color_unpremultiplies() {
        let mid = resolver().lerp_color(
            Color::linear_srgb(0.0, 0.0, 0.0, 1.0),
            Color::linear_srgb(1.0, 1.0, 1.0, 1.0),
            0.5,
        );
        assert_eq!(mid, Color::linear_srgb(0.5, 0.5, 0.5, 1.0));
        let clear = resolver().lerp_color(
            Color::srgb(1.0, 0.0, 0.0, 0.0),
            Color::srgb(0.0, 1.0, 0.0, 0.0),
            0.5,
        );
        assert!(clear.is_transparent());
        let text = format!("{clear:?}");
        assert!(!text.contains("inf"), "不得出现 inf：{text}");
    }
    #[test]
    fn hue_extremes_are_normalized() {
        let wrapped = resolver().resolve(Color::oklch(0.6, 0.1, 1e9, 1.0));
        let normalized = resolver().resolve(Color::oklch(0.6, 0.1, 1e9_f32.rem_euclid(360.0), 1.0));
        for index in 0..4 {
            assert_eq!(
                wrapped.components[index].to_bits(),
                normalized.components[index].to_bits(),
                "channel {index}"
            );
        }
        assert!(hue_deg(f32::INFINITY).abs() < f32::EPSILON);
        assert!(hue_deg(f32::NEG_INFINITY).abs() < f32::EPSILON);
        assert!((hue_deg(-10.0) - 350.0).abs() < 1e-3);
    }

    #[test]
    fn defaults_are_explicit() {
        assert_eq!(Color::default(), Color::TRANSPARENT);
        assert_eq!(ColorBrush::default().color, Color::BLACK);
        assert_eq!(ColorResolver, ColorResolver);
    }

    #[test]
    fn size_budget() {
        assert_eq!(size_of::<Color>(), 20);
        assert_eq!(size_of::<LinearPremulRgba>(), 16);
    }

    #[test]
    fn clamp_01_handles_nan() {
        assert!(clamp_01(f32::NAN).abs() < f32::EPSILON);
        assert!((clamp_01(2.0) - 1.0).abs() < f32::EPSILON);
        assert!(clamp_01(-1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn clamp_nonneg_handles_infinity() {
        assert!(clamp_nonneg(f32::INFINITY).abs() < f32::EPSILON);
        assert!(clamp_nonneg(f32::NEG_INFINITY).abs() < f32::EPSILON);
        assert!(clamp_nonneg(f32::NAN).abs() < f32::EPSILON);
        assert!((clamp_nonneg(-3.0)).abs() < f32::EPSILON);
        assert!((clamp_nonneg(2.5) - 2.5).abs() < f32::EPSILON);
    }

    #[test]
    fn alpha_color_roundtrip_is_stable() {
        let linear = AlphaColor::<color::LinearSrgb>::new([0.1, 0.2, 0.3, 1.0]);
        let dynamic = DynamicColor::from_alpha_color(linear);
        let back = dynamic.to_alpha_color::<color::LinearSrgb>();
        assert!((back.components[0] - 0.1).abs() < 1e-6);
    }
    #[test]
    fn resolve_cache_matches_direct_resolve() {
        let resolver = resolver();
        let colors = [
            Color::TRANSPARENT,
            Color::BLACK,
            Color::WHITE,
            Color::srgb(0.2, 0.4, 0.6, 0.8),
            Color::linear_srgb(0.1, 0.5, 0.9, 1.0),
            Color::display_p3(0.3, 0.6, 0.2, 0.5),
            Color::oklab(0.5, 0.1, -0.1, 1.0),
            Color::oklch(0.6, 0.3, 30.0, 0.7),
        ];
        let mut cache = ResolveCache::new();
        for round in 0..2 {
            for color in colors {
                let cached = cache.resolve(&resolver, color);
                let direct = resolver.resolve(color);
                assert_eq!(
                    cached.components.map(f32::to_bits),
                    direct.components.map(f32::to_bits),
                    "round {round} color {color:?} 的缓存结果必须与直接解析逐位相同"
                );
            }
        }
    }

    #[test]
    fn resolve_cache_keys_on_bits_not_equality() {
        assert_eq!(
            Color::srgb(0.0, 0.0, 0.0, 1.0),
            Color::srgb(-0.0, 0.0, 0.0, 1.0),
            "PartialEq 认为它们相等（这是 `Color` 的既有语义）"
        );
        let (a_space, a) = super::color_bits(Color::srgb(0.0, 0.0, 0.0, 1.0));
        let (b_space, b) = super::color_bits(Color::srgb(-0.0, 0.0, 0.0, 1.0));
        assert_eq!(a_space, b_space);
        assert_ne!(a, b, "位模式必须区分 -0.0 与 0.0");
    }

    #[test]
    fn resolve_cache_clear_forces_recompute() {
        let resolver = resolver();
        let mut cache = ResolveCache::new();
        let color = Color::oklch(0.6, 0.3, 30.0, 1.0);
        let first = cache.resolve(&resolver, color);
        cache.clear();
        let second = cache.resolve(&resolver, color);
        assert_eq!(
            first.components.map(f32::to_bits),
            second.components.map(f32::to_bits),
            "clear 之后重算的结果必须一致"
        );
    }

    #[test]
    fn resolve_cache_survives_slot_collisions() {
        let resolver = resolver();
        let mut cache = ResolveCache::new();
        for index in 0..ResolveCache::SLOTS * 4 {
            let color = Color::oklch(
                0.5,
                0.05 * (index % 8) as f32,
                (index * 13 % 360) as f32,
                1.0,
            );
            let cached = cache.resolve(&resolver, color);
            let direct = resolver.resolve(color);
            assert_eq!(
                cached.components.map(f32::to_bits),
                direct.components.map(f32::to_bits),
                "碰撞时 index {index} 的结果必须仍然正确"
            );
        }
    }
}
