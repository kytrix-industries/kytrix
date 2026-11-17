//! D23：引擎一等颜色。
//!
//! 唯一职责：把 [`Color`] 解析成 GPU 消费的 **linear sRGB + premultiplied alpha**（[`LinearPremulRgba`]），
//! 并在**同一个**空间里插值（[`ColorResolver::lerp`]）。`color` crate 只在本文件出现（I15 的可验证形式）。
//!
//! **颜色语义只有一套（R05-7）**：合成（sRGB RT + `ONE / INV_SRC_ALPHA`）、AA（覆盖率在预乘空间
//! 相乘）、shader 渐变（`gradient_eval` 的 `mix`）、CPU 动画（[`ColorResolver::lerp`]）全部在
//! **线性预乘**空间完成。引擎不提供第二种插值空间 —— 旧的 `Interpolation::Oklab/Oklch` 是只作用于
//! 死代码的伪配置，已删除。CSS/Skia 默认在 sRGB **编码**空间插值；本引擎刻意选线性光（物理正确，
//! 代价是同一对颜色的中点比 Chrome 更亮，取舍记录见 `.docs/ui_core-审查报告.md` §5.6）。
//!
//! 算法与边界（自建 CSS Color 4 chroma 二分、NaN/clamp 规则）见
//! `design/phase-02-paint-foundation.md` §5.4–§5.5。

use color::{ColorSpaceTag, DynamicColor, Flags};

/// 自建 gamut mapping 的 in-gamut 容差（只吸收 f32 噪声，不改变视觉结果）。
const GAMUT_EPSILON: f32 = 1e-6;
/// 二分迭代次数。**定长**：同平台同输入必须走同一条路径（跨平台允许 ULP 级差异）。
const GAMUT_BISECTION_STEPS: u32 = 24;

/// 引擎一等颜色值（D23 / I15）。
///
/// 所有变体都存「**分量空间 + 非预乘 + 独立 alpha**」；预乘只发生在 [`ColorResolver::resolve`] /
/// [`ColorResolver::interpolate`] 的出口。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    /// sRGB 编码分量，通常 `0..=1`。
    Srgb { r: f32, g: f32, b: f32, alpha: f32 },
    /// 线性 sRGB 分量，通常 `0..=1`。
    LinearSrgb { r: f32, g: f32, b: f32, alpha: f32 },
    /// Display P3（sRGB 传递函数、D65），通常 `0..=1`。
    DisplayP3 { r: f32, g: f32, b: f32, alpha: f32 },
    /// Oklab：`l` 约 `0..=1`，`a`/`b` 约 `-0.4..=0.4`。
    Oklab { l: f32, a: f32, b: f32, alpha: f32 },
    /// Oklch：`l` 约 `0..=1`，`c >= 0`，`h` 单位**度**。
    Oklch { l: f32, c: f32, h: f32, alpha: f32 },
}

impl Color {
    /// 完全透明（绘制层的单位元）。
    pub const TRANSPARENT: Self = Self::Srgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        alpha: 0.0,
    };
    /// 不透明黑（文本/画刷的经典默认）。
    pub const BLACK: Self = Self::Srgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        alpha: 1.0,
    };
    /// 不透明白（`Image` 的默认调制色）。
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

    /// alpha 乘 `factor`（`[0, 1]` clamp，NaN → 0）：`StylePatch::opacity` 的唯一落点。
    ///
    /// 只动 alpha 分量（所有变体都存独立 alpha），因此与 [`ColorResolver::resolve`] 的
    /// 「预乘空间缩放 alpha」逐位等价，且不需要解析器。
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

    /// alpha `<= 0`（NaN 视为透明）：「本图元不产生像素」的**唯一**判定。
    ///
    /// 与 [`LinearPremulRgba::is_transparent`] 同语义，但作用在未预乘的 [`Color`] 上，
    /// 让 paint 阶段（还没有解析器时）与编码器用同一判据。
    #[must_use]
    pub fn is_transparent(self) -> bool {
        let alpha = self.alpha();
        alpha <= 0.0 || alpha.is_nan()
    }

    /// 读取独立 alpha 分量（5 个变体同名分量；`with_opacity`/`is_transparent` 的内部实现）。
    const fn alpha(self) -> f32 {
        match self {
            Self::Srgb { alpha, .. }
            | Self::LinearSrgb { alpha, .. }
            | Self::DisplayP3 { alpha, .. }
            | Self::Oklab { alpha, .. }
            | Self::Oklch { alpha, .. } => alpha,
        }
    }

    /// 8 位 sRGB（`alpha` 也是 8 位）：主题 / 资源 / 设计稿最常见的入口。
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

    /// 该颜色的空间标签与「非预乘分量（含独立 alpha）」。
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

/// 线性 sRGB + **预乘 alpha**：`components == [r·a, g·a, b·a, a]`。
///
/// 这是唯一「到 GPU」的表示，直接写进 `GpuPrimitive.color0/color1`。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinearPremulRgba {
    pub components: [f32; 4],
}

impl LinearPremulRgba {
    /// 全透明。
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

    /// `a >= 1.0`（`resolve` 已把 alpha clamp 到 `[0, 1]`，因此不设 epsilon）。
    #[must_use]
    pub fn is_opaque(self) -> bool {
        self.components[3] >= 1.0
    }

    /// `a <= 0.0`；编码器据此跳过不产生像素的图元。
    #[must_use]
    pub fn is_transparent(self) -> bool {
        self.components[3] <= 0.0
    }

    /// 线性预乘空间的插值：**引擎唯一的颜色插值**（R05-7）。
    ///
    /// 预乘分量直接插值等价于「按 alpha 加权的线性空间混合」，因此端点全透明时不会把颜色
    /// 带进结果（无 color leak）；`t` 先 clamp 到 `[0, 1]`（两端点精确）。
    /// shader 的 `gradient_eval` 用的就是这个数学（预乘分量上的 `mix`）。
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

    /// 预乘空间的 opacity 缩放（4 个分量同乘）。
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

/// 解析结果缓存（P1-2.1）：一帧内同一个颜色会被几十个图元重复用到（主题色），
/// 而宽色域颜色的解析要跑 24 步 chroma 二分 —— 实测 256 个**全部不同**的颜色要 55.9 µs。
///
/// **按位比较**而不是 `PartialEq`：`Color` 含 `f32`，`PartialEq` 下 `NaN != NaN` 会让缓存
/// 永不命中，且 `-0.0 == 0.0` 会让两个不同位模式共享条目。按位键让「命中即可用」成立。
///
/// 直接映射（64 槽）+ 存**完整键**而不是哈希：无分配、O(1)、零误命中（碰撞只是放弃缓存，
/// 重算一次，结果仍正确）。典型主题有 10–50 个不同颜色，64 槽足够。
pub(crate) struct ResolveCache {
    entries: [Option<(ColorBits, LinearPremulRgba)>; Self::SLOTS],
}

/// `Color` 的逐位表示（空间标签 + 4 个分量的位模式）。
type ColorBits = (u8, [u32; 4]);

impl ResolveCache {
    const SLOTS: usize = 64;

    pub(crate) const fn new() -> Self {
        Self {
            entries: [None; Self::SLOTS],
        }
    }

    /// 清空（每帧编码开始时调用：主题色可能随帧变化）。
    pub(crate) fn clear(&mut self) {
        self.entries = [None; Self::SLOTS];
    }

    /// 解析（命中直接返回；未命中计算并写入）。
    pub(crate) fn resolve(&mut self, resolver: &ColorResolver, color: Color) -> LinearPremulRgba {
        let bits = color_bits(color);
        // 槽位选择：把 4 个分量的位模式混合成一个下标。分量完全相同 ⇒ 下标相同。
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

/// `Color` → 逐位表示（空间标签 + 分量位模式）。
fn color_bits(color: Color) -> ColorBits {
    let (space, parts) = color.dynamic_parts();
    (
        match space {
            ColorSpaceTag::Srgb => 0,
            ColorSpaceTag::LinearSrgb => 1,
            ColorSpaceTag::DisplayP3 => 2,
            ColorSpaceTag::Oklab => 3,
            ColorSpaceTag::Oklch => 4,
            // `color` crate 的空间是开放的；未知空间退化成「不缓存」（下标恒定 ⇒ 永远不命中
            // 也不会误命中，因为键里的分量位模式仍然是完整的）。用 255 而不是 0：0 已被
            // Srgb 占用，混用会让两种不同空间在极端情况下互相命中。
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

/// 主题策略对象：把 [`Color`] 解析成 GPU 消费的线性预乘值，并在这**唯一**的空间里插值。
///
/// 引擎的插值空间只有一个：**线性预乘 sRGB**（与合成、AA、渐变同一套）。CPU 侧的
/// `resolve`/`lerp` 与 shader 的 `gradient_eval` 是同一份数学（R05-7）。因此本类型没有
/// 「插值空间」配置 —— 旧的 `Interpolation::Oklab/Oklch` 是一份只作用于死代码的伪配置，已删除；
/// 渐变（shader 内 `mix`）与动画（[`ColorResolver::lerp`]）现在走同一条路径。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorResolver;

impl ColorResolver {
    /// 解析成 GPU 消费的线性预乘 RGBA。
    ///
    /// 声明在 sRGB 族空间（`Srgb`/`LinearSrgb`）的颜色只做**逐通道 clamp**；
    /// 宽色域空间（`DisplayP3`/`Oklab`/`Oklch`）走 CSS Color 4 的 **chroma 二分** gamut mapping。
    #[must_use]
    pub fn resolve(&self, color: Color) -> LinearPremulRgba {
        let dynamic = normalized_dynamic(color);
        let alpha = dynamic.components[3];
        let linear = resolve_linear(dynamic);
        premultiply(linear, alpha)
    }

    /// 两端各自 `resolve` 后，在**线性预乘空间**插值（`t` 先 clamp 到 `[0, 1]`）。
    ///
    /// 恒有 `lerp(a, b, 0.0) == resolve(a)`、`lerp(a, b, 1.0) == resolve(b)`；这也是 shader 的
    /// `gradient_eval` 所用的数学（`mix` 在预乘分量上），因此渐变、动画、合成、AA 的语义完全一致。
    #[must_use]
    pub fn lerp(&self, from: Color, to: Color, t: f32) -> LinearPremulRgba {
        LinearPremulRgba::lerp(self.resolve(from), self.resolve(to), t)
    }

    /// 同 [`Self::lerp`]，但把线性预乘结果解回**非预乘的线性** [`Color`]（动画驱动的样式值用它）。
    ///
    /// `alpha == 0` 时返回全透明（避免除零把颜色分量放大成 inf）。
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

/// 端点归一化：`alpha` 恒 clamp 到 `[0, 1]`；Oklch 的 `L`/`C`/`h` 各自收敛到定义域。
///
/// `resolve` 与 `interpolate` **共用**这一函数，保证两端点语义一致（含 `h = 1e9` 之类的病态输入）。
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

/// `resolve` 的空间选择：sRGB 族只 clamp，宽色域走 gamut mapping。
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

/// 任意空间 →（Oklch → chroma 二分）→ sRGB 色域内的线性分量。
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

/// 二分出「sRGB 色域内最大的 chroma」；调用方已保证 `(l, c, h)` 越界。
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

/// 非有限值统一收敛到 `0.0`，然后 clamp 到 `[0, 1]`。
fn clamp_01(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// 非有限值（含 `+inf`）统一收敛到 `0.0`，否则 clamp 到 `[0, +inf)`（chroma 的定义域）。
fn clamp_nonneg(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// 非有限值（含 `±inf`）统一收敛到 `0.0`，否则 `rem_euclid(360)`（色相角）。
fn hue_deg(value: f32) -> f32 {
    if value.is_finite() {
        value.rem_euclid(360.0)
    } else {
        0.0
    }
}

/// `parley::style::Brush` 是标记 trait（`Clone + PartialEq + Default + Debug`），本类型自动满足。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorBrush {
    pub color: Color,
}

impl Default for ColorBrush {
    /// 默认画刷 = 不透明黑（文本的经典默认）；与 `Color::default()`（透明，绘制单位元）刻意不同。
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

    /// 把线性预乘结果解回「线性、非预乘」三分量（测试断言用）。
    fn linear_of(color: Color) -> [f32; 3] {
        let resolved = resolver().resolve(color);
        let alpha = resolved.components[3];
        [
            resolved.components[0] / alpha,
            resolved.components[1] / alpha,
            resolved.components[2] / alpha,
        ]
    }

    /// 反解到 Oklch（断言 hue 用）。
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
        // sRGB EOTF: ((0.5 + 0.055) / 1.055)^2.4 ≈ 0.21404114
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
        // 越界分量只 clamp，不做 chroma 归约。
        let clamped = resolver().resolve(Color::linear_srgb(1.5, -1.0, 0.5, 1.0));
        assert_eq!(clamped.components[..3], [1.0, 0.0, 0.5]);
    }

    #[test]
    fn resolve_oklab_variant() {
        // Oklab 的 L 是感知明度（约等于线性亮度的立方根），L = 0.5 → Y ≈ 0.125。
        let resolved = resolver().resolve(Color::oklab(0.5, 0.0, 0.0, 1.0));
        for value in &resolved.components[..3] {
            assert!((value - 0.125).abs() < 2e-3, "got {value}");
        }
    }

    #[test]
    fn oklch_in_gamut_is_passthrough() {
        let color = Color::oklch(0.6, 0.10, 30.0, 1.0);
        // 独立期望：直接用 `color` crate 做同一转换（不经过我们的 helper）。
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

    /// R05-7：引擎唯一的插值空间是**线性预乘**（与 shader `gradient_eval`、合成、AA 同一套）。
    #[test]
    fn lerp_is_linear_premultiplied() {
        let mid = resolver().lerp(Color::BLACK, Color::WHITE, 0.5);
        // 线性光中点 = 0.5（sRGB 编码空间的中点会得到 0.2140 的线性值 ⇒ 视觉上更暗）。
        for value in &mid.components[..3] {
            assert!((value - 0.5).abs() < 1e-6, "channel {value}");
        }
        assert!((mid.components[3] - 1.0).abs() < f32::EPSILON);
        // 端点精确。
        assert_eq!(
            resolver().lerp(Color::BLACK, Color::WHITE, 0.0).components,
            resolver().resolve(Color::BLACK).components
        );
        assert_eq!(
            resolver().lerp(Color::BLACK, Color::WHITE, 1.0).components,
            resolver().resolve(Color::WHITE).components
        );
    }

    /// 预乘空间插值不会把「透明端的颜色」泄漏到结果里（旧的 sRGB 空间实现在这里会出灰边）。
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

    /// 端点必须等于 `resolve` 的同色结果（宽色域输入也要先 gamut-map）。
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

    /// alpha 参与加权（预乘插值）：0.5 alpha 的白与不透明黑的中点 = alpha 0.75、预乘 rgb 0.25。
    #[test]
    fn lerp_blends_premultiplied_alpha() {
        let mid = resolver().lerp(Color::srgb(1.0, 1.0, 1.0, 0.5), Color::BLACK, 0.5);
        for value in &mid.components[..3] {
            assert!((value - 0.25).abs() < 1e-6, "channel {value}");
        }
        assert!((mid.components[3] - 0.75).abs() < 1e-6);
    }

    /// `lerp_color` 把线性预乘结果解回非预乘的线性色（动画驱动的样式值用它）。
    #[test]
    fn lerp_color_unpremultiplies() {
        let mid = resolver().lerp_color(
            Color::linear_srgb(0.0, 0.0, 0.0, 1.0),
            Color::linear_srgb(1.0, 1.0, 1.0, 1.0),
            0.5,
        );
        assert_eq!(mid, Color::linear_srgb(0.5, 0.5, 0.5, 1.0));
        // 全透明端点 ⇒ 全透明（不产生 inf）。
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
        // h = 1e9 与归一化后的 h 必须给出同一结果（B-N6）。
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

    /// 依赖 sanity（不是我们的契约）：证明 `DynamicColor` 的 alpha 往返没有被我们破坏。
    #[test]
    fn alpha_color_roundtrip_is_stable() {
        let linear = AlphaColor::<color::LinearSrgb>::new([0.1, 0.2, 0.3, 1.0]);
        let dynamic = DynamicColor::from_alpha_color(linear);
        let back = dynamic.to_alpha_color::<color::LinearSrgb>();
        assert!((back.components[0] - 0.1).abs() < 1e-6);
    }
    /// P1-2.1：缓存必须与直接解析**逐位相同**（否则增量渲染与全量渲染会出不同的像素）。
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
        // 两轮：第一轮全 miss，第二轮全 hit —— 两轮都必须与直接解析逐位相同。
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

    /// 缓存的键是**位模式**：`-0.0` 与 `0.0` 是两个不同的键。
    ///
    /// 用 `PartialEq` 做键会让它们共享条目；虽然解析结果恰好相同，但「按位键」是缓存正确性
    /// 的可依赖契约 —— 它保证「命中 ⇒ 输入的每一位都相同」，未来解析若引入符号相关行为也不会
    /// 悄悄出错。
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

    /// 缓存必须在 `clear` 后重新计算（主题切换时颜色会变）。
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

    /// 槽位碰撞不得产生错误结果（碰撞只是放弃缓存）。
    #[test]
    fn resolve_cache_survives_slot_collisions() {
        let resolver = resolver();
        let mut cache = ResolveCache::new();
        // 远超槽位数量的不同颜色：必然发生碰撞。
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
