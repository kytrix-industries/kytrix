//! 文本子系统：公共契约（P05）。
//!
//! 本模块是**公共模块根**（P01 §4.1）：契约类型直接定义在这里，子模块是实现、一律 `pub(crate)`：
//!
//! - `context`：`TextContext`（`parley::{FontContext, LayoutContext}` + 字体栈 + 形状缓存 + 诊断）
//! - `shape`：`ShapedText`（一次 shape 的只读快照）与 measure
//! - `raster`：skrifa 度量 + swash alpha 光栅
//! - `atlas`：字形 `key → AtlasTile`（消费 `crate::render::atlas`）
//! - `cache`：`ShapeKey` + LRU
//! - `inline_flow`：`InlineBoxSpec → parley::InlineBox`、`(anchor, focus) → Rect`
//!
//! 公开路径因此是 `ui_core::text::{TextStyle, FontStack, InlineBoxSpec, ShapedLine, …}`
//! 文本的**引擎**（字体库/图集/光栅）在 `App` 内部
//! 应用层通过 `App::{refresh_text, set_font_stack, …}` 使用，见 §6.1。
//!
//! # 字形定位与 hinting（复审 P2-2 方案 A + 第三轮复审 P3 收尾）
//!
//! 光栅策略按 Skia/Chrome 的 UI 文本默认：**水平亚像素定位开、hinting 关、ppem 用小数**。
//! 实现与开关都在 `text/raster.rs`：`SUBPIXEL_PHASES = 4`（1/4 px 相位）、`HINTING = false`；
//! ppem 直接取 parley 的物理字号（`Run::font_size()`，**保留小数**）。
//!
//! - **收益**：parley 的落点与 advance 是小数物理像素；旧实现把每个字形 `round` 到整像素、按
//!   整数 ppem 全 hinting，非整数 DPI（125%/150%）下字形内部笔画与字间 advance 不一致，长数字
//!   列会出现字距跳动/个别字形重叠。现在 x 吸附到最近的 1/4 px 相位（误差 ≤ 1/8 px），位图按
//!   该相位光栅；ppem 也用小数（16 px @125% → 20.0、14 px @125% → 17.5），位图尺寸与 parley
//!   的分数 advance 同源，不再有「按 18 ppem 出位图、按 17.5 px 排 advance」的 2.9% 尺寸差。
//!   **三者是一致的一套取舍**：关 hinting 才让小数 ppem 有意义（hinting 本身就要求整数 ppem 的
//!   像素网格）；亚像素定位才接得住 parley 的小数落点；只做其中一项会留下另一种量化误差。
//! - **代价**：`GlyphKey` 多出 `subpixel_x` 相位维（最多 4 档）与 `ppem_q` 量化维，同一字形可能
//!   占 1–4 个 tile，图集命中率下降、容量最多约 4×（512→1024→2048 的阶梯扩容兜底，但可能
//!   提前触发一次整页换代）；小 ppem（≤ 16）关闭 hinting 后笔画略软；非整数 ppem 的位图不再是
//!   「整数字号的像素完美」，125%/150% 下字形边缘更平滑（与 Skia/Chrome 的同一表现）。
//!   图集键里的 ppem 只保留 `1/64 px` 量化值（`atlas::PPEM_KEY_QUANTIZATION`）：只影响缓存
//!   复用，不影响光栅尺寸（光栅收原值 f32）。
//! - **回退**：把 `raster::SUBPIXEL_PHASES` 改为 `1`（整像素定位）或 `raster::HINTING` 改为
//!   `true`（旧的全 hinting）。两者都是编译期常量，改完字形图集自然换代，不需要 migration。
//!   回退小数 ppem 则在 `shape.rs` 恢复 `font_size().round()` —— `ppem_q` 的 1/64 量化对整数
//!   ppem 仍是精确的（其子集），无需额外改动。
//! - **整数 DPI**：100%/200% 下 ppem 本就是整数，advance/行高/断行完全来自 parley，不受本
//!   改动影响；视觉差异仅限关闭 hinting 带来的抗锯齿细节（方案 A 的预期取舍）。
//!
//! # 为什么没有文本像素 golden
//!
//! 仓库**没有**文本光栅的像素级 golden 测试：文本像素同时取决于宿主系统字体（家族解析与
//! fallback 链）、字体版本、设备 scale、平台 AA/合成策略，三平台 CI 上不可能逐位一致 —— 一张
//! Windows 的 golden 在 Linux/macOS 上恒失败，改成 skip 又失去回归价值。文本正确性因此由三层
//! 非像素测试保证：
//!
//! 1. 结构与度量断言（`cargo test -p ui_core --features test-support text`）：字形数、advance、
//!    行高、断行、`GlyphKey` 的相位/ppem 维、`len == w*h` 的 R8 覆盖度；
//! 2. 光栅不变量（`... atlas` / `... raster`）：同一 key 位图确定、不同相位/ppem 不共用 tile、
//!    `missing` 负缓存不误命中、小数 ppem 不被取整；
//! 3. **真机目视**：关 hinting 的视觉取舍必须在真实设备上按 **100% / 125% / 150% / 200%** 查看
//!    （建议同一屏混排 Latin 数字列 + CJK，检查笔画粗细、字距呼吸、上下标与 thin stroked UI）。
//!
//! 若将来要加 golden，必须固定以下变量（否则测试不可复现，也没法跨平台）：
//!
//! - **字体文件**：随仓库携带（或按 sha256 校验的测试字体），不复用系统字体栈；家族名、版本、
//!   可变字体默认实例全部写死；
//! - **scale**：显式 `1.0 / 1.25 / 1.5 / 2.0` 各一组，不读宿主 DPI；
//! - **hinting**：`raster::HINTING` 的编译期值写进 golden 文件名，或在测试里断言当前值；
//! - **相位**：把落点钉在固定布局宽度上（如同一居中宽度），使 `subpixel_x` 确定 —— 同一字形的
//!   4 个相位应各有一张 golden；
//! - 其余：光栅后端（CPU vs GPU 采样）、颜色空间（线性/R8 覆盖度）、图集 padding / UV 取整
//!   规则也必须固定。

use core::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    math::{Rect, Size},
    paint::{color::Color, primitive::PositionedGlyph},
    resources::ResourceId,
};

pub(crate) mod atlas;
pub(crate) mod cache;
pub(crate) mod context;
pub(crate) mod fast_path;
pub(crate) mod inline_flow;
pub(crate) mod raster;
pub(crate) mod shape;
pub(crate) mod standalone;

/// 文本水平对齐（映射到 parley `Alignment`，`text/shape.rs`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextAlign {
    /// 起始侧（LTR 左 / RTL 右）。
    #[default]
    Start,
    /// 结束侧。
    End,
    /// 左。
    Left,
    /// 居中。
    Center,
    /// 右。
    Right,
    /// 两端对齐（末行按 `Start`）。
    Justify,
}

/// 文本倾斜（映射到 parley `FontStyle`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextSlant {
    /// 正体。
    #[default]
    Normal,
    /// 斜体（字体自带字形，否则由字体合成）。
    Italic,
    /// 伪斜（`Oblique(None)`）。
    Oblique,
}

/// 文本装饰（v2.1 E2，`§11.2`）：下划线 / 删除线。
///
/// 装饰**不参与布局**（不改变字形位置、advance、断行、行高）：引擎在 `ShapedLine` 上产出
/// [`ShapedDecoration`] 几何，paint 阶段按普通 `Primitive::Quad` 编码（`§11.2`：不进 shader，
/// 因此不触发 I8 的 shader 契约/golden）。
///
/// `color = None` ⇒ 跟随该 run 的文本色（`TextStyle::color`；`bind_style` 的 `foreground`
/// 覆盖对文本与装饰同时生效）。`thickness` 是逻辑像素线宽（`<= 0` / 非有限值时该装饰不绘制）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextDecoration {
    /// 下划线（基线下方）。
    Underline {
        /// 线色；`None` = 跟随文本色。
        color: Option<Color>,
        /// 线宽（逻辑像素）。
        thickness: f32,
    },
    /// 删除线（字形中部）。
    Strikethrough {
        /// 线色；`None` = 跟随文本色。
        color: Option<Color>,
        /// 线宽（逻辑像素）。
        thickness: f32,
    },
}

impl TextDecoration {
    /// 下划线（颜色跟随文本色）。
    #[must_use]
    pub const fn underline(thickness: f32) -> Self {
        Self::Underline {
            color: None,
            thickness,
        }
    }

    /// 删除线（颜色跟随文本色）。
    #[must_use]
    pub const fn strikethrough(thickness: f32) -> Self {
        Self::Strikethrough {
            color: None,
            thickness,
        }
    }

    /// 显式线色（`None` 清掉覆盖、回到跟随文本色）。
    #[must_use]
    pub const fn with_color(self, color: Option<Color>) -> Self {
        match self {
            Self::Underline { thickness, .. } => Self::Underline { color, thickness },
            Self::Strikethrough { thickness, .. } => Self::Strikethrough { color, thickness },
        }
    }

    /// 线色；`None` = 跟随文本色。
    #[must_use]
    pub const fn color(self) -> Option<Color> {
        match self {
            Self::Underline { color, .. } | Self::Strikethrough { color, .. } => color,
        }
    }

    /// 线宽（逻辑像素）。
    #[must_use]
    pub const fn thickness(self) -> f32 {
        match self {
            Self::Underline { thickness, .. } | Self::Strikethrough { thickness, .. } => thickness,
        }
    }

    /// 装饰是否可绘制（线宽有限且为正）。
    #[must_use]
    pub fn is_visible(self) -> bool {
        self.thickness().is_finite() && self.thickness() > 0.0
    }

    /// 形状缓存键的一部分（`Option<Color>` 的 f32 分量按 `to_bits` 哈希）。
    pub(crate) fn hash_into(self, hasher: &mut ahash::AHasher) {
        use core::hash::Hash;

        let (tag, color, thickness) = match self {
            Self::Underline { color, thickness } => (0u8, color, thickness),
            Self::Strikethrough { color, thickness } => (1u8, color, thickness),
        };
        tag.hash(hasher);
        color.map(color_bits).hash(hasher);
        thickness.to_bits().hash(hasher);
    }
}

/// 软换行策略（映射到 `TextWrapMode` / `WordBreak` / `OverflowWrap`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextWrap {
    /// 按 Unicode 断行规则在软换行机会处换行。
    #[default]
    Wrap,
    /// 不换行（超宽时溢出）。
    NoWrap,
    /// 任意字符间可断（日志/哈希列）。
    Anywhere,
}

/// 段落基准方向（映射到 `set_base_direction`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextDirection {
    /// 从首个强方向字符推断。
    #[default]
    Auto,
    /// 强制 LTR。
    Ltr,
    /// 强制 RTL。
    Rtl,
}

/// 溢出裁剪策略（CSS `text-overflow`；R12-9）。
///
/// | 值 | 语义 | 对照 |
/// |---|---|---|
/// | `Clip` | 直接裁剪，不追加任何字形（默认） | CSS `text-overflow: clip`、Qt 默认 |
/// | `Ellipsis` | 在最后一可见行末尾追加省略号（`…`，缺字形时退成 `...`） | CSS `text-overflow: ellipsis`、Flutter `TextOverflow.ellipsis`、`Qt::ElideRight` |
///
/// 省略号的 advance **计入可用宽度**（`shape` 期已塑形，见 `text/shape.rs` 的
/// `clip_with_ellipsis`），与 Chrome `LayoutNG` 一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextOverflow {
    /// 直接裁剪（默认）。
    #[default]
    Clip,
    /// 末尾加省略号。
    Ellipsis,
}

/// 断行严格度（CSS `line-break`；映射到 parley `LineBreak`）。
///
/// 影响 CJK 行首禁则：`Strict`（默认）禁止行首出现 `。」` 等，`Loose` 放宽，
/// `Normal` 居中。多语言行情终端需要按语言切换（与 `locale` 一起决定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextLineBreak {
    /// 最宽松（CSS `line-break: loose`）。
    Loose,
    /// 常规（CSS `line-break: normal`）。
    Normal,
    /// 最严格（CSS `line-break: strict`，parley 默认）。
    Strict,
}

/// 一段文本的样式（**值语义**，不是句柄）。
///
/// `Clone` 而不是 `Copy`：`family` 是 `SmolStr`。字号的单位是**逻辑像素**；物理 ppem 由
/// 光栅阶段按设备 `scale` 换算并**保留小数**（见模块文档「字形定位与 hinting」，如 14 px
/// @125% → 17.5 ppem）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// 字号（逻辑像素）。
    pub size: f32,
    /// CSS 字重（1..=1000；`400` 常规、`700` 粗体）。
    pub weight: u16,
    /// 倾斜。
    pub slant: TextSlant,
    /// 行高倍数（CSS 无单位 `line-height`）；`None` = 字体度量。
    pub line_height: Option<f32>,
    /// 字间距（逻辑像素）。
    pub letter_spacing: f32,
    /// 前景色（未预乘）。
    pub color: Color,
    /// 水平对齐。
    pub alignment: TextAlign,
    /// 软换行策略。
    pub wrap: TextWrap,
    /// 基准方向。
    pub direction: TextDirection,
    /// 溢出裁剪策略（R12-9）。
    pub overflow: TextOverflow,
    /// 行数上限（`0` = 不限；CSS `-webkit-line-clamp`，R12-9）。
    ///
    /// 超过上限的行被丢弃；`overflow == Ellipsis` 时在最后一个可见行末尾加省略号。
    /// 用 `0` 表示「不限」而不是 `Option<u16>`：后者占 4 B，而这一维在建表时几乎恒为 0。
    pub max_lines: u16,
    /// OpenType 特性标签（如 `tnum` 等宽数字、`ss01` 风格集）。
    ///
    /// **为什么必须有**：价格 / 成交量这类高频数字表格要求每列数字宽度一致，否则内容一变
    /// 整列就会抖动、表格无法对齐。CSS 对应 `font-variant-numeric: tabular-nums`，
    /// Qt 对应 `QFont::setFeature`，Flutter 对应 `TextStyle.fontFeatures`。
    ///
    /// 低频字段（家族名覆盖 + OpenType 特性）。None = 两者都用默认。
    ///
    /// 用 Blink 的 `RareInheritedData` 模式：这些字段在绝大多数文本上是空的，却各占
    /// 8–24 B。`TextStyle` 内联在 `TextContent` 里、`TextContent` 又内联在 `Node` 里，
    /// 每个节点都要为它们付内存。合成一个 `Option<Box<_>>`（8 B，null 优化，`None` 不分配）。
    pub extras: Option<Box<TextStyleExtras>>,
}

/// 一个「薄」的只读切片（8 B：单个指针；`Box<[T]>` 是 16 B 的胖指针）。
///
/// **为什么需要**：`TextContent` 在 `NodeKind::Text` 里是装箱的（`Box<TextContent>`），但每个
/// 文本节点仍只有一份载荷；`TextStyle` 内联在 `TextContent` 里、`TextStyle` 又被大量克隆。
/// 低频配置字段用 `Box<[T]>` 的胖指针（指针 + 长度）会白占 8 B。`Option<Box<ThinSlice<T>>>`
/// 靠 `Box` 的空指针优化只有 8 B，且 `None` 不分配。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThinSlice<T>(Box<[T]>);

impl<T> ThinSlice<T> {
    /// 迭代元素。
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }

    /// 元素个数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否为空（构造点保证非空，保留此方法给调用方判空）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 只读切片视图。
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

/// 由迭代器构造；空集合返回 `None`（不分配）。
fn thin_slice<T>(items: impl IntoIterator<Item = T>) -> Option<Box<ThinSlice<T>>> {
    let mut collected: Vec<T> = items.into_iter().collect();
    if collected.is_empty() {
        return None;
    }
    collected.shrink_to_fit();
    Some(Box::new(ThinSlice(collected.into_boxed_slice())))
}

/// `TextStyle` 的低频字段（Blink 的 `RareInheritedData` 模式）。
///
/// 这两个字段在绝大多数文本上是空的，但 `TextStyle` 内联在 `TextContent` 里、`TextContent`
/// 又内联在 `Node` 里 —— 直接把它们摊平会让每个节点多付 32 B。合成一个
/// `Option<Box<TextStyleExtras>>`（8 B，`None` 不分配）后，`TextStyle` 从 80 B 回到 56 B。
///
/// 用 `.clone().unwrap_or_default()` 的访问代价换内存：设置家族名 / OpenType 特性是低频操作，
/// 读取走 [`TextStyle::family`] / [`TextStyle::font_features`] 两个访问器。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStyleExtras {
    /// 家族名覆盖；`None` = 用引擎 `FontStack`（D17 的 CJK 显式栈）。
    pub family: Option<SmolStr>,
    /// OpenType 特性（`tnum` 等宽数字等）。空 = 不请求任何特性。
    pub font_features: Option<Box<FeatureSet>>,
    /// BCP-47 语言标签（如 `zh-Hans`/`ja`/`ko`）：决定 CJK 字形变体、标点宽度与 `locl`。
    ///
    /// 用 `SmolStr` 而不是 `fontique::Language`：`TextStyle` 是**公开**类型，把 fontique 的
    /// 类型放进去等于把字体库拉进公开面（与 `TextContext` 的 crate 私有同理）。
    /// 解析发生在 `shape`（每个样式每次 shape 一次），不在缓存键之外引入隐藏维度。
    pub locale: Option<SmolStr>,
    /// 可变字体轴值（`wght`/`opsz`/`wdth`…）：`None` = 用字体默认实例。
    pub font_variations: Option<Box<FontVariationSet>>,
    /// `font-stretch` 百分比（`100` = 常规；CSS `font-stretch: 87.5%`）。
    pub stretch: Option<u16>,
    /// 词间距（逻辑像素；CSS `word-spacing`）。
    pub word_spacing: Option<f32>,
    /// 断行严格度（CSS `line-break`）。
    pub line_break: Option<TextLineBreak>,
    /// 文本装饰（v2.1 E2，`§11.2`）。
    ///
    /// 收进 `extras` 而不是内联进 `TextStyle`：装饰是低频配置，而 `TextStyle` 内联在
    /// `TextContent` / `Node` 里（`node_size_after_p06` 预算 336 B，TextContent 必须保持
    /// 低占用）。与 `family` / `locale` 等「RareInheritedData」字段同一取舍。
    pub decoration: Option<TextDecoration>,
}

/// 一组 OpenType 特性。
pub type FeatureSet = ThinSlice<TextFeature>;

/// 一组可变字体轴值。
pub type FontVariationSet = ThinSlice<TextFontVariation>;

/// 一个可变字体轴值（4 字节标签 + 轴值，如 `wght = 700.0`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextFontVariation {
    /// 4 字节轴标签（如 `*b"wght"`）。
    pub tag: [u8; 4],
    /// 轴值（字体 fvar 轴的空间，如 `wght` 的 100..=900）。
    pub value: f32,
}

impl TextFontVariation {
    /// 以 4 字节标签 + 轴值构造。
    #[must_use]
    pub const fn new(tag: [u8; 4], value: f32) -> Self {
        Self { tag, value }
    }
}

/// 一组行内盒。
pub type InlineBoxSet = ThinSlice<InlineBoxSpec>;

/// 一个 OpenType 特性（4 字节标签 + 开关值）。
///
/// 标签用 `[u8; 4]` 而不是 `SmolStr`：OpenType 的 tag 恒为 4 个 ASCII 字节，定长表示让
/// `TextStyle` 保持 `Copy`-友好、比较与哈希都便宜。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextFeature {
    /// 4 字节标签（如 `*b"tnum"`）。
    pub tag: [u8; 4],
    /// 特性值：`1` = 开启、`0` = 关闭；其它值是字体的特性专属参数。
    pub value: u16,
}

impl TextFeature {
    /// 等宽数字（`tnum`）：让所有数字占据相同 advance。
    ///
    /// 这是高频数字表格的硬需求 —— 价格列不抖动才能对齐。
    pub const TABULAR_NUMBERS: Self = Self {
        tag: *b"tnum",
        value: 1,
    };

    /// 以 4 字节标签 + 值构造。
    #[must_use]
    pub const fn new(tag: [u8; 4], value: u16) -> Self {
        Self { tag, value }
    }

    /// 标签的字符串形式（调试 / 诊断）。
    #[must_use]
    pub fn tag_str(self) -> String {
        String::from_utf8_lossy(&self.tag).into_owned()
    }
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            size: 14.0,
            weight: 400,
            slant: TextSlant::Normal,
            line_height: None,
            letter_spacing: 0.0,
            color: Color::BLACK,
            alignment: TextAlign::Start,
            wrap: TextWrap::Wrap,
            direction: TextDirection::Auto,
            overflow: TextOverflow::Clip,
            max_lines: 0,
            extras: None,
        }
    }
}

impl TextStyle {
    /// 以字号建默认样式。
    ///
    /// 不是 `const fn`：`extras` 是 `Option<Box<_>>`，`None` 虽不分配，但保持构造函数可演进
    /// （与 `Default` 同一字段集）。
    #[must_use]
    pub fn new(size: f32) -> Self {
        Self {
            size,
            ..Self::default()
        }
    }

    /// 家族名覆盖（`None` = 用引擎 `FontStack`）。
    #[must_use]
    pub fn family(&self) -> Option<&SmolStr> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.family.as_ref())
    }

    /// OpenType 特性集（`None` = 不请求任何特性）。
    #[must_use]
    pub fn font_features(&self) -> Option<&FeatureSet> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.font_features.as_deref())
    }

    /// BCP-47 语言标签（`None` = 用引擎/系统默认）。
    #[must_use]
    pub fn locale(&self) -> Option<&str> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.locale.as_deref())
    }

    /// 可变字体轴值集（`None` = 用字体默认实例）。
    #[must_use]
    pub fn font_variations(&self) -> Option<&FontVariationSet> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.font_variations.as_deref())
    }

    /// `font-stretch` 百分比（`None` = 常规 100%）。
    #[must_use]
    pub fn stretch(&self) -> Option<u16> {
        self.extras.as_ref().and_then(|extras| extras.stretch)
    }

    /// 词间距（逻辑像素；`None` = 0）。
    #[must_use]
    pub fn word_spacing(&self) -> Option<f32> {
        self.extras.as_ref().and_then(|extras| extras.word_spacing)
    }

    /// 断行严格度（`None` = parley 默认 `Strict`）。
    #[must_use]
    pub fn line_break(&self) -> Option<TextLineBreak> {
        self.extras.as_ref().and_then(|extras| extras.line_break)
    }

    /// 行数上限（`None` = 不限）。
    #[must_use]
    pub fn max_lines(&self) -> Option<u16> {
        (self.max_lines > 0).then_some(self.max_lines)
    }

    /// 文本装饰（`None` = 无；v2.1 E2）。
    #[must_use]
    pub fn decoration(&self) -> Option<TextDecoration> {
        self.extras.as_ref().and_then(|extras| extras.decoration)
    }

    /// 溢出裁剪策略。
    #[must_use]
    pub const fn overflow(&self) -> TextOverflow {
        self.overflow
    }

    /// 该样式是否与 `other` 产生**相同的形状结果**（R12-11：忽略 `color` 与 `alignment`）。
    ///
    /// 形状缓存键的等价类就是这个函数：`color` 只影响 run 的 brush、`alignment` 只影响
    /// `align()` 之后的偏移，两者都不改变 glyph id / advance / 断行。缓存命中后必须用
    /// **同一个**等价关系复验（否则「同键不同色」会被判成 miss）。
    #[must_use]
    pub fn same_shaping_inputs(&self, other: &Self) -> bool {
        self.size.to_bits() == other.size.to_bits()
            && self.weight == other.weight
            && self.slant == other.slant
            && self.line_height.map(f32::to_bits) == other.line_height.map(f32::to_bits)
            && self.letter_spacing.to_bits() == other.letter_spacing.to_bits()
            && self.wrap == other.wrap
            && self.direction == other.direction
            && self.overflow == other.overflow
            && self.max_lines == other.max_lines
            && self.extras == other.extras
    }

    /// 取（必要时创建）低频字段容器。
    fn extras_mut(&mut self) -> &mut TextStyleExtras {
        self.extras
            .get_or_insert_with(|| Box::new(TextStyleExtras::default()))
    }

    /// 追加 OpenType 特性（如 [`TextFeature::TABULAR_NUMBERS`]）。
    ///
    /// 高频数字表格应当请求等宽数字：否则内容一变，整列宽度就抖动。
    #[must_use]
    pub fn with_font_features(mut self, features: impl IntoIterator<Item = TextFeature>) -> Self {
        let mut combined: Vec<TextFeature> = self
            .extras
            .as_ref()
            .and_then(|extras| extras.font_features.as_ref())
            .map(|existing| existing.as_slice().to_vec())
            .unwrap_or_default();
        combined.extend(features);
        self.extras_mut().font_features = thin_slice(combined);
        self
    }

    /// 链式：字号。
    #[must_use]
    pub const fn with_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// 链式：家族名覆盖。
    #[must_use]
    pub fn with_family(mut self, family: impl Into<SmolStr>) -> Self {
        self.extras_mut().family = Some(family.into());
        self
    }

    /// 链式：字重。
    #[must_use]
    pub const fn with_weight(mut self, weight: u16) -> Self {
        self.weight = weight;
        self
    }

    /// 链式：颜色。
    #[must_use]
    pub const fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// 链式：行高倍数。
    #[must_use]
    pub const fn with_line_height(mut self, factor: f32) -> Self {
        self.line_height = Some(factor);
        self
    }

    /// 链式：字间距。
    #[must_use]
    pub const fn with_letter_spacing(mut self, spacing: f32) -> Self {
        self.letter_spacing = spacing;
        self
    }

    /// 链式：对齐。
    #[must_use]
    pub const fn with_alignment(mut self, alignment: TextAlign) -> Self {
        self.alignment = alignment;
        self
    }

    /// 链式：换行策略。
    #[must_use]
    pub const fn with_wrap(mut self, wrap: TextWrap) -> Self {
        self.wrap = wrap;
        self
    }

    /// 链式：基准方向。
    #[must_use]
    pub const fn with_direction(mut self, direction: TextDirection) -> Self {
        self.direction = direction;
        self
    }

    /// 链式：溢出裁剪策略 + 行数上限（R12-9）。
    #[must_use]
    pub const fn with_overflow(mut self, overflow: TextOverflow, max_lines: u16) -> Self {
        self.overflow = overflow;
        self.max_lines = max_lines;
        self
    }

    /// 链式：语言标签（BCP-47）。
    #[must_use]
    pub fn with_locale(mut self, locale: impl Into<SmolStr>) -> Self {
        self.extras_mut().locale = Some(locale.into());
        self
    }

    /// 链式：可变字体轴值。
    #[must_use]
    pub fn with_font_variations(
        mut self,
        variations: impl IntoIterator<Item = TextFontVariation>,
    ) -> Self {
        let mut combined: Vec<TextFontVariation> = self
            .extras
            .as_ref()
            .and_then(|extras| extras.font_variations.as_ref())
            .map(|existing| existing.as_slice().to_vec())
            .unwrap_or_default();
        combined.extend(variations);
        self.extras_mut().font_variations = thin_slice(combined);
        self
    }

    /// 链式：`font-stretch` 百分比（`100` = 常规）。
    #[must_use]
    pub fn with_stretch(mut self, stretch: u16) -> Self {
        self.extras_mut().stretch = Some(stretch);
        self
    }

    /// 链式：词间距（逻辑像素）。
    #[must_use]
    pub fn with_word_spacing(mut self, word_spacing: f32) -> Self {
        self.extras_mut().word_spacing = Some(word_spacing);
        self
    }

    /// 链式：断行严格度。
    #[must_use]
    pub fn with_line_break(mut self, line_break: TextLineBreak) -> Self {
        self.extras_mut().line_break = Some(line_break);
        self
    }

    /// 链式：文本装饰（v2.1 E2）。
    #[must_use]
    pub fn with_decoration(mut self, decoration: TextDecoration) -> Self {
        self.extras_mut().decoration = Some(decoration);
        self
    }

    /// 形状缓存的样式哈希（f32 全部按 `to_bits`；字段集即缓存契约，改动即失效）。
    ///
    /// **不含 `color` / `alignment`**（R12-11）：两者不改变 glyph id / advance / 断行。
    /// 等价关系见 [`TextStyle::same_shaping_inputs`]。
    pub(crate) fn hash_into(&self, hasher: &mut ahash::AHasher) {
        use core::hash::Hash;

        self.family().hash(hasher);
        self.size.to_bits().hash(hasher);
        self.weight.hash(hasher);
        (self.slant as u8).hash(hasher);
        self.line_height.map(f32::to_bits).hash(hasher);
        self.letter_spacing.to_bits().hash(hasher);
        (self.wrap as u8).hash(hasher);
        (self.direction as u8).hash(hasher);
        (self.overflow as u8).hash(hasher);
        self.max_lines.hash(hasher);
        // 特性参与缓存键：不同特性集必须 shape 出不同的字形位置。
        self.font_features().map(|set| set.as_slice()).hash(hasher);
        // R12-10：新增的 shaping 输入同样必须进键（否则历史条目会被静默误命中）。
        self.locale().hash(hasher);
        self.stretch().hash(hasher);
        self.word_spacing().map(f32::to_bits).hash(hasher);
        self.line_break().map(|kind| kind as u8).hash(hasher);
        self.font_variations()
            .map(|set| {
                set.iter()
                    .map(|variation| (variation.tag, variation.value.to_bits()))
                    .collect::<Vec<_>>()
            })
            .hash(hasher);
        // v2.1 E2：装饰被烘进 `ShapedLine` 的 `ShapedDecoration` 几何 ⇒ 必须进键（否则
        // 装饰变更会命中旧快照、旧装饰被继续绘制）。`color` 的 f32 用 `to_bits`（NaN 可复现）。
        match self.decoration() {
            Some(decoration) => {
                true.hash(hasher);
                decoration.hash_into(hasher);
            }
            None => false.hash(hasher),
        }
    }
}

/// 颜色分量的位模式（`Color` 没有 `Hash`：f32 用 `to_bits`，五个色彩空间各自编码）。
pub(crate) fn color_bits(color: Color) -> [u32; 5] {
    let (space, [c0, c1, c2, alpha]) = match color {
        Color::Srgb { r, g, b, alpha } => (0u32, [r, g, b, alpha]),
        Color::LinearSrgb { r, g, b, alpha } => (1u32, [r, g, b, alpha]),
        Color::DisplayP3 { r, g, b, alpha } => (2u32, [r, g, b, alpha]),
        Color::Oklab { l, a, b, alpha } => (3u32, [l, a, b, alpha]),
        Color::Oklch { l, c, h, alpha } => (4u32, [l, c, h, alpha]),
    };
    [
        space,
        c0.to_bits(),
        c1.to_bits(),
        c2.to_bits(),
        alpha.to_bits(),
    ]
}

/// 富文本的一段覆盖样式（v2.1 E2，`§11.2`）。
///
/// `range` 是源文本的 **UTF-8 字节区间**（升序、不重叠、落在 char boundary）。
///
/// **覆盖语义：逐属性继承 + 可选覆盖**（不是「整段换一套样式」）：
///
/// - `Option` 字段（`family` / `font_features` / `stretch` / `word_spacing` / `locale` /
///   `font_variations` / `line_break` / `decoration`）：`Some` 覆盖该属性，`None` 继承
///   [`TextContent::style`] 的同名属性（因此 `None` **不能**把兜底样式的非默认值清回默认）。
/// - 其余字段（字号/字重/倾斜/行高/字距/颜色/`wrap`）是具体值，总是取 run 自己的值。
///   其中 `wrap` 例外：实现按区间下发 `NoWrap` / `Anywhere`，但 `Wrap` 无法把兜底样式的
///   `NoWrap` / `Anywhere` 复位 —— **`wrap` 的实际归属是整段的兜底样式**（要可换行就让
///   `TextContent.style` 保持 `Wrap`，不要指望 run 改回）。
/// - 段落级字段（`alignment` / `overflow` / `max_lines` / `direction`）**不生效**：这些是
///   整段文本的属性，由 [`TextContent::style`] 决定。
///
/// 区间非法（越界 / 非 char boundary / 非升序）时在构造期归一化（只缩不挪），并把归一化
/// 计数接到 [`TextDiagnostic::InvalidTextRun`]（`shape` 时可见）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    /// 覆盖的源文本字节区间（半开）。
    pub range: Range<usize>,
    /// 该区间的覆盖样式（逐属性继承 + 可选覆盖，见类型文档）。
    pub style: TextStyle,
}

impl TextRun {
    /// 以字节区间 + 覆盖样式构造。
    #[must_use]
    pub fn new(range: Range<usize>, style: TextStyle) -> Self {
        Self { range, style }
    }
}

/// 节点载荷：文本 + 默认样式 + 富文本 runs + 行内盒（`NodeKind::Text` 的内容）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextContent {
    /// 源文本。
    pub text: SmolStr,
    /// 兜底样式（`runs` 未覆盖的区间用它）。
    pub style: TextStyle,
    /// 富文本逐区间覆盖样式（v2.1 E2）：空 = 全段用 `style`。
    ///
    /// 字段私有：只能经 [`TextContent::with_runs`] 写入，构造点保证「升序、不重叠、落在 char
    /// boundary、在 `text` 范围内」（越界 / 非法区间按构造期归一化裁剪，不 panic）。读取走
    /// [`TextContent::runs`]。
    runs: Option<Box<ThinSlice<TextRun>>>,
    /// 构造期归一化时被改动（内收或丢弃）的 run 数（`§11.2`：非法 runs 不静默）。
    ///
    /// 形状期把它压成一条 [`TextDiagnostic::InvalidTextRun`]；`TextContext` 的诊断环按值去重，
    /// 不会每帧刷屏。归一化后的 `runs` 本身已合法，因此这个计数是唯一的可观测面。
    normalized_runs: u16,
    /// 行内盒（`<img>` / `kbd`）：参与断行与行高，位置由引擎算（§12）。
    pub inline_boxes: Box<[InlineBoxSpec]>,
}

impl TextContent {
    /// 构造（无 runs、无行内盒）。
    #[must_use]
    pub fn new(text: impl Into<SmolStr>, style: TextStyle) -> Self {
        Self {
            text: text.into(),
            style,
            runs: None,
            normalized_runs: 0,
            inline_boxes: Box::new([]),
        }
    }

    /// 链式：挂上行内盒（`NodeBuilder::inline_boxes` 的落点）。
    #[must_use]
    pub fn with_inline_boxes(mut self, boxes: impl IntoIterator<Item = InlineBoxSpec>) -> Self {
        self.inline_boxes = boxes.into_iter().collect();
        self
    }

    /// 链式：富文本 runs（`NodeBuilder::runs` 的落点；v2.1 E2）。
    ///
    /// 越界 / 非 char boundary / 非升序的区间在这里**归一化**（向字符边界内收、重叠部分丢弃）；
    /// 空区间丢弃。归一化只让覆盖范围变小，不会把样式挪到别的字符上。被改动（内收或丢弃）的
    /// run 数会在 `shape` 期成为一条 [`TextDiagnostic::InvalidTextRun`]（不静默）。
    #[must_use]
    pub fn with_runs(self, runs: impl IntoIterator<Item = TextRun>) -> Self {
        let mut content = self;
        content.set_runs(runs);
        content
    }

    /// 替换 runs（语义同 [`TextContent::with_runs`]，供 `NodeBuilder::runs` 就地写）。
    pub(crate) fn set_runs(&mut self, runs: impl IntoIterator<Item = TextRun>) {
        let (normalized, altered) = normalize_runs(&self.text, runs);
        self.normalized_runs = u16::try_from(altered).unwrap_or(u16::MAX);
        self.runs = thin_slice(normalized);
    }

    /// 富文本 runs（空切片 = 全段用 [`TextContent::style`]）。
    #[must_use]
    pub fn runs(&self) -> &[TextRun] {
        self.runs.as_deref().map_or(&[], |runs| runs.as_slice())
    }

    /// 构造期被归一化改动（内收或丢弃）的 run 数；`0` = 全部合法。
    ///
    /// 形状期成为 [`TextDiagnostic::InvalidTextRun`] 的计数（`§11.2`：非法 runs 归一化 +
    /// 不静默）。也进形状缓存键与命中判据：诊断随快照复用，不能命中一份「没有这条诊断」的
    /// 旧快照。
    #[must_use]
    pub(crate) fn normalized_runs(&self) -> u16 {
        self.normalized_runs
    }

    /// `bind_text` 换整段文本后裁剪 runs（v2.1 E2，`§11.2`）：保留 runs，越界部分按新文本
    /// 长度内收 / 丢弃。返回**被改动**（裁剪或丢弃）的 run 数，调用方据此写
    /// [`TextDiagnostic::TextRunClipped`]（不静默改变样式归属）。
    pub(crate) fn clip_runs_to_text(&mut self) -> usize {
        let Some(runs) = self.runs.take() else {
            return 0;
        };
        let mut altered = 0usize;
        let mut kept: Vec<TextRun> = Vec::with_capacity(runs.len());
        for mut run in runs.iter().cloned() {
            let start = ceil_char_boundary(&self.text, run.range.start);
            let end = floor_char_boundary(&self.text, run.range.end);
            if start != run.range.start || end != run.range.end {
                altered += 1;
            }
            if start >= end {
                continue;
            }
            run.range = start..end;
            kept.push(run);
        }
        self.runs = thin_slice(kept);
        altered
    }
}

/// 归一化 runs：裁剪到文本范围 / 字符边界，丢弃空区间与非升序区间的重叠部分。
fn normalize_runs(text: &str, runs: impl IntoIterator<Item = TextRun>) -> (Vec<TextRun>, usize) {
    let mut normalized = Vec::with_capacity(4);
    let mut altered = 0usize;
    let mut previous_end = 0usize;
    for mut run in runs {
        let raw = run.range.clone();
        let start = ceil_char_boundary(text, run.range.start).max(previous_end);
        let end = floor_char_boundary(text, run.range.end);
        if start >= end {
            altered += 1;
            continue;
        }
        if start != raw.start || end != raw.end {
            altered += 1;
        }
        run.range = start..end;
        previous_end = end;
        normalized.push(run);
    }
    (normalized, altered)
}

/// 向上取最近的 char boundary（`index >= text.len()` 时夹到 `len`）。
fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// 向下取最近的 char boundary。
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 字体栈（D17）：有序候选家族名 + 版本号。
///
/// **跨平台同一个列表**：不存在的家族名由 fontique 的 `Query::set_families` 静默跳过
/// （`query.rs:66-70`），因此不需要 `#[cfg(target_os)]`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontStack {
    families: SmallVec<[SmolStr; 8]>,
    version: u32,
}

impl FontStack {
    /// 平台默认栈：Latin/UI 在前，CJK 在后（缺字形的脚本会继续往后找），最后 generic 兜底。
    #[must_use]
    pub fn system_ui() -> Self {
        Self::new(
            SYSTEM_UI_FAMILIES
                .iter()
                .map(|name| SmolStr::new_static(name)),
        )
    }

    /// 自定义栈（按序）；`version` 从 1 起。
    #[must_use]
    pub fn new(families: impl IntoIterator<Item = impl Into<SmolStr>>) -> Self {
        Self {
            families: families.into_iter().map(Into::into).collect(),
            version: 1,
        }
    }

    /// 候选名（按序）。
    pub fn families(&self) -> impl ExactSizeIterator<Item = &str> + Clone + '_ {
        self.families.iter().map(SmolStr::as_str)
    }

    /// 当前版本（每次内容变更 +1；形状缓存键的一部分）。
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// 替换候选集：返回**新**版本号（内容相同则不变）。
    pub fn set_families(&mut self, families: impl IntoIterator<Item = impl Into<SmolStr>>) -> u32 {
        let next: SmallVec<[SmolStr; 8]> = families.into_iter().map(Into::into).collect();
        if next != self.families {
            self.families = next;
            self.version = self.version.wrapping_add(1);
        }
        self.version
    }
}

impl Default for FontStack {
    fn default() -> Self {
        Self::system_ui()
    }
}

/// D17 的显式栈：Latin/UI → CJK → 其他 → generic（generic 由 `TextContext` 追加）。
pub(crate) const SYSTEM_UI_FAMILIES: &[&str] = &[
    "Segoe UI Variable Text",
    "Segoe UI",
    "SF Pro Text",
    "Helvetica Neue",
    "Noto Sans",
    "DejaVu Sans",
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "PingFang SC",
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Noto Sans SC",
    "WenQuanYi Micro Hei",
];

/// 行内盒的垂直对齐（parley `VerticalAlign` 的 8 个常量，线性表）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum InlineAlign {
    /// 基线对齐（默认）。
    #[default]
    Baseline,
    /// 下标。
    Sub,
    /// 上标。
    Super,
    /// 文本顶部。
    TextTop,
    /// 文本底部。
    TextBottom,
    /// 中线。
    Middle,
    /// 行顶。
    Top,
    /// 行底。
    Bottom,
}

/// 行内盒的应用声明绘制方式（v2.1 E2，`§11.2`）。
///
/// 引擎在 `paint/pass.rs::emit_text` 里按 [`ShapedInlineBox::rect`] 生成既有 8 类图元
/// （I2：不新增 `Primitive` 变体）：`Quad` 走 `Primitive::Quad`，`Image` 走 `Primitive::Image`。
/// 引擎负责在 paint 阶段把 `ResourceId` 解析成 `ResourceImage.tile`；资源未加载 / 已卸载时
/// **跳过该图元**并压 `TextDiagnostic::InlineBoxImageMissing`（不做占位图、不 panic）。
///
/// 无 `App` 通道的 [`crate::window::Window::frame`] 没有 `ResourceCache`，`Image` 一律跳过；
/// 那条路径把跳过的 id 记在 `Window::inline_box_image_diagnostics`（同样是 D17 的可观测面）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InlineBoxPaint {
    /// 图片（`<img>` / 行内图标）：`ResourceCache` 解析后的 tile 按 `rect` 拉伸绘制。
    Image {
        /// 资源句柄（`ResourceCache::insert*` 返回的 `ResourceImage.id`）。
        resource: ResourceId,
        /// 调制色（`Color::WHITE` = 原色）。
        modulation: Color,
    },
    /// 装饰盒（`kbd` 圆角块 / 行内代码底色）。
    Quad {
        /// 填充色。
        color: Color,
        /// 四角半径 `[tl, tr, br, bl]`（逻辑像素）。
        corner_radius: [f32; 4],
    },
}

/// 行内盒说明（`<img>` / `kbd` / 行内图标）：**尺寸由应用给，位置由引擎算**。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InlineBoxSpec {
    /// 应用句柄（映射回子节点 / 资源）；原样出现在 [`ShapedInlineBox::id`]。
    pub id: u64,
    /// 文本**字节偏移**（必须落在 char boundary）。
    pub index: usize,
    /// 盒尺寸（逻辑像素）。
    pub size: Size,
    /// 盒内基线（相对盒顶，逻辑像素）；`None` = 底边对齐文本基线。
    pub baseline: Option<f32>,
    /// 盒在行内的垂直对齐。
    pub align: InlineAlign,
    /// 引擎绘制方式（v2.1 E2）：`None` = 只算落点、不绘制（E1 的现状语义）。
    pub paint: Option<InlineBoxPaint>,
}

/// 一段文本的**只读**布局结果（一个 parley `Layout` 的快照，字形已装箱）。
///
/// 坐标是**文本盒局部**逻辑像素（原点 = 内容盒左上角）；`glyphs[].position` 已含 run 偏移与基线。
/// P06 加节点 `LayoutBox.bounds.origin` 后才是 `Primitive::GlyphRun` 要求的绝对坐标。
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedLine {
    /// 源文本（P10 的选区映射以此 `&str` 为字节基准）。
    pub text: SmolStr,
    /// 逐行结果（空文本 = 0 行）。
    pub lines: Vec<ShapedTextLine>,
    /// 内容宽（`max(line.advance)`，不含悬挂空白）。
    pub width: f32,
    /// 内容高（`Σ line.line_height`）。
    pub height: f32,
    /// 首行基线 y（文本盒局部）；空文本 = `None`。
    pub first_baseline: Option<f32>,
    /// 生成时的图集代际：`AtlasTile::is_stale(generation)` 为真 ⇒ 字形矩形已失效。
    pub atlas_generation: u32,
    /// 生成时的字体栈版本（诊断 / 断言）。
    pub font_stack_version: u32,
    /// 生成时的设备 scale（断言：`scale * glyph.size` 必须是整数）。
    pub scale: f32,
}

impl ShapedLine {
    /// 空文本快照（`measure` 判零的依据）。
    #[must_use]
    pub(crate) fn empty(
        text: SmolStr,
        generation: u32,
        font_stack_version: u32,
        scale: f32,
    ) -> Self {
        Self {
            text,
            lines: Vec::new(),
            width: 0.0,
            height: 0.0,
            first_baseline: None,
            atlas_generation: generation,
            font_stack_version,
            scale,
        }
    }

    /// 内容尺寸（逻辑像素）。
    #[must_use]
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// 所有字形 run（按绘制序）。
    pub fn glyph_runs(&self) -> impl Iterator<Item = &ShapedGlyphRun> + '_ {
        self.lines.iter().flat_map(|line| {
            line.items.iter().filter_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => Some(run),
                ShapedTextItem::InlineBox(_) => None,
            })
        })
    }
}

/// 一行。
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedTextLine {
    /// 该行覆盖的源文本字节区间。
    pub text_range: Range<usize>,
    /// 基线 y（文本盒局部）。
    pub baseline: f32,
    /// 行高（含 leading）。
    pub line_height: f32,
    /// 行内推进。
    pub advance: f32,
    /// 行的项：字形 run 与行内盒**按绘制序**交替。
    pub items: Vec<ShapedTextItem>,
    /// 该行的文本装饰几何（v2.1 E2，`§11.2`）：按绘制序（兜底样式在前、run 装饰在后）。
    /// 坐标是文本盒局部逻辑像素；paint 按普通 `Primitive::Quad` 编码。
    pub decorations: Vec<ShapedDecoration>,
}

/// 一条文本装饰的已定位几何（v2.1 E2，`§11.2`）。
///
/// 由 `shape` 的 rasterize 阶段从「字符 → 行内 x」映射算出：`rect.y` 由该行 `baseline` 与装饰
/// 所属样式的字号决定（下划线在基线下 `em × 0.10`，删除线在基线上方 `em × 0.30`），
/// `rect` 的 x 跨度是该 run 在本行的可见字符范围（区间内 cluster 的视觉并集，RTL/bidi 同规则；
/// 硬换行行尾的 `\n` / `\r` 不计入）。装饰**不参与布局**、也不进 shader。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedDecoration {
    /// 文本盒局部矩形（逻辑像素）。
    pub rect: Rect,
    /// 已解析的线色（`TextDecoration::color = None` 时取文本色 / `bind_style` 覆盖色）。
    pub color: Color,
}

/// 行内项。
#[derive(Clone, Debug, PartialEq)]
pub enum ShapedTextItem {
    /// 一段同字体同色的字形。
    GlyphRun(ShapedGlyphRun),
    /// 一个行内盒。
    InlineBox(ShapedInlineBox),
}

/// 一段同字体同色的字形 run。
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedGlyphRun {
    /// 前景色（未预乘）。
    pub color: Color,
    /// 该 run 覆盖的源文本字节区间。
    pub text_range: Range<usize>,
    /// 已定位且已装箱的字形（顺序 = 绘制顺序）。
    ///
    /// 水平亚像素相位已经烘进位图与 tile 落点，这里的位置/尺寸乘 `ShapedLine::scale` 后恒为
    /// 整数物理像素（见模块文档「字形定位与 hinting」）。
    pub glyphs: Vec<PositionedGlyph>,
}

/// 一个行内盒的落点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedInlineBox {
    /// [`InlineBoxSpec::id`]。
    pub id: u64,
    /// 文本盒局部矩形。
    pub rect: Rect,
    /// 盒内基线（相对 `rect` 顶边）；`None` = 底边对齐。
    pub baseline: Option<f32>,
}

/// 图集租约的打包值：高 32 位 = `generation`，低 32 位 = `eviction_epoch`。
///
/// 打包成一个 `AtomicU64` 而不是两个 `AtomicU32`：两者必须**原子地一起前进**，否则
/// `is_stale` 可能读到「新代际 + 旧纪元」的撕裂组合（换代与淘汰相邻发生时短暂假活）。
/// 唯一写入者见 [`crate::app::App`] 的租约同步点。
#[must_use]
pub(crate) const fn pack_atlas_lease(generation: u32, eviction_epoch: u32) -> u64 {
    (generation as u64) << 32 | eviction_epoch as u64
}

/// 一次**无节点归属**的 shaping 的长期句柄（v2.1 E1，`§11.1`）。
///
/// 由 [`crate::app::App::shape_text`] 产出，应用只读：字形位置 / 行 / 行内盒走
/// [`Self::line`]，内容尺寸走 [`Self::size`]，光栅 scale 走 [`Self::scale`]，行内盒落点走
/// [`crate::app::App::shaped_inline_boxes`]。句柄**没有公开构造器**，也不给应用
/// `AtlasTile` / `GlyphRun` 的构造权（`§11.1`：应用永不构造裸 `GlyphRun` / `AtlasTile`）。
///
/// 句柄带**图集租约** `(generation, eviction_epoch)`（v2.1 E1b）：
/// - 图集换代（DPI 变化 / 字形容量逃逸 / 字体栈切换）⇒ `generation` 变化；
/// - 逐 tile LRU 淘汰（容量压力下任何 tile 被复用）⇒ `eviction_epoch` 变化。
///
/// 任一变化后 [`Self::is_stale`] 为真，`Canvas::push_shaped_text` 拒绝编码（不画旧 UV，等价
/// D-5 的「唯一重光栅入口」）；应用应丢弃句柄并重新 `shape_text`。
#[derive(Debug)]
pub struct ShapedTextHandle {
    /// 塑形快照（只读）。
    line: Arc<ShapedLine>,
    /// 行内盒落点（顺序 = `shape_text` 传入的 `inline_boxes`，`id` 原样保留）。
    inline_boxes: Box<[ShapedInlineBox]>,
    /// 构造时的图集代际。
    generation: u32,
    /// 构造时的图集累计淘汰纪元（任何 tile 被 LRU 淘汰即前进）。
    eviction_epoch: u32,
    /// 构造时的光栅 scale（App 级默认 scale；`line().scale` 的同一值）。
    scale: f32,
    /// 与 `App` 共享的**当前**图集租约（打包的 `(generation, eviction_epoch)`）。
    lease: Arc<AtomicU64>,
}

impl ShapedTextHandle {
    /// 引擎内部构造（唯一调用者：`text::standalone::shape`）。
    pub(crate) fn new(
        line: Arc<ShapedLine>,
        inline_boxes: Box<[ShapedInlineBox]>,
        generation: u32,
        eviction_epoch: u32,
        scale: f32,
        lease: Arc<AtomicU64>,
    ) -> Self {
        Self {
            line,
            inline_boxes,
            generation,
            eviction_epoch,
            scale,
            lease,
        }
    }

    /// 只读快照（字形位置、行、行内盒）。
    ///
    /// # Panics: none
    #[must_use]
    pub fn line(&self) -> &ShapedLine {
        &self.line
    }

    /// 图集租约是否已失效：`true` = 句柄里的 `AtlasTile` 已失效（旧 UV / 已被复用），必须
    /// 重新 `shape_text`；此时 `Canvas::push_shaped_text` 返回 `false` 且不产生任何图元。
    ///
    /// `generation`（整页换代）或 `eviction_epoch`（逐 tile 淘汰）任一前进即为真。
    ///
    /// **已写入 `Canvas` 的旧 tile** 不靠本方法兜底（句柄已不在 Canvas 里）：编码期用
    /// `AtlasAllocator::is_alive` 逐 tile 判活，失效 run 不编码并计入
    /// [`TextDiagnostic::CanvasTextStale`]（v2.1 E1d）。
    ///
    /// # Panics: none
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.lease.load(Ordering::Relaxed) != pack_atlas_lease(self.generation, self.eviction_epoch)
    }

    /// 内容尺寸（逻辑像素）。
    ///
    /// # Panics: none
    #[must_use]
    pub fn size(&self) -> Size {
        self.line.size()
    }

    /// 本次 shaping 使用的光栅 scale（`§11.1` 的 App 级默认 scale；默认 1.0）。
    ///
    /// 句柄内的 tile 位图按 `字号 × scale` 的物理 ppem 光栅；应用把句柄画到 DPI 不同的窗口
    /// （或窗口 DPI 变化）后必须重新 `shape_text` —— 引擎不会自动重光栅已有句柄。
    ///
    /// # Panics: none
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// 行内盒落点（crate 内部：`App::shaped_inline_boxes` 的唯一出口）。
    pub(crate) fn inline_boxes(&self) -> &[ShapedInlineBox] {
        &self.inline_boxes
    }
}

/// 一次文本 pass 的统计（P06 帧记录 / L1 断言）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextPassReport {
    /// 真正跑 `RangedBuilder::build` 的次数（形状缓存 miss）。
    pub shaped: usize,
    /// 命中形状缓存、复用 `Rc<ShapedText>` 的次数。
    pub reused: usize,
    /// 形状快照未变、整节点被跳过的次数（B4：空闲帧必须只走这条路）。
    pub skipped: usize,
    /// 本次累积的脏矩形数（P07 / R06-3 的 AOI 增量上传输入）。
    pub dirty_spans: usize,
    /// 走数字快速路径完成的塑形次数（R12 的 `DigitFastPath` 命中数）。
    pub fast_path_shaped: usize,
    /// 进入快速路径判定的塑形次数（命中率分母：`fast_path_shaped / fast_path_queries`）。
    ///
    /// 包含被静态条件拒绝的候选（RTL / letter-spacing / 断行机会 / 字符集），因此这是
    /// **真实热路径命中率**，而不是只统计已 eligible 文本的自证式口径。
    pub fast_path_queries: usize,
    /// 候选被静态条件拒绝的次数（`fast_path_queries` 的子集，D-2）。
    ///
    /// 与「验证降级」（`FastPathStats::rejected`，字形/advance 等价性不成立）区分开：
    /// 前者是文本形态不适合，后者是宿主字体的 shaping 行为不满足快速路径前提。
    pub fast_path_ineligible: usize,
}

/// `App::layout_text` 的统计（B1 的第二阶段：按内容宽重排 + 光栅）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextLayoutReport {
    /// 本次重排并挂上快照的文本节点数。
    pub laid: usize,
    /// 宽度/代际未变、直接复用既有 `ShapedLine` 的节点数。
    pub reused: usize,
    /// 整 pass 因图集换代而重跑的轮数（0 = 未重跑）。
    pub reruns: usize,
    /// 本 pass 真正光栅化的字形数。
    ///
    /// `GlyphAtlas::take_stats` 是**单一消费者**语义：光栅发生在第二阶段，所以统计只能
    /// 在这里聚合；第一阶段（`TextPassReport`）取到的永远是 0。
    pub rasterized: usize,
    /// 命中字形 tile 缓存的字形数。
    pub tiles_reused: usize,
    /// 因图集满而走「清空 + 重建」的次数。
    pub atlas_rebuilds: usize,
    /// 本次 pass 逐 tile LRU 淘汰的字形数（R12-1：容量逃逸时不再整页清空）。
    pub atlas_evictions: usize,
    /// 本 pass **最后一轮重跑**仍因「字形图集整页都是本帧工作集」而没画出的字形数（R12-1）。
    ///
    /// 与 [`TextLayoutReport::rasterized`] 是同一聚合口径（每轮重跑都清零重算，报告取最后
    /// 一轮），因此恒 `<= rasterized`：每个被跳过的字形都先光栅过一次。
    ///
    /// 这些字形**不进** `missing` 负缓存：对应节点会重新入下一帧的重排队列、带「待重排」标记
    /// 重光栅，并压一条 [`TextDiagnostic::AtlasFull`]。连续
    /// `text::atlas::ATLAS_FULL_ESCALATION_FRAMES`（默认 3）个 pass 仍非零时，
    /// `App::layout_text` 改为返回 [`crate::error::EngineError::AtlasExhausted`]。
    pub skipped_glyphs: usize,
}

/// 字体栈的健康状态（N4：测试与诊断的**唯一**跳过判据，避免散落 `eprintln!`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontStackStatus {
    /// 至少一个候选家族（或 generic 兜底）可用。
    Ready,
    /// 栈为空：形状退到 generic 兜底，并记 [`TextDiagnostic::FontStackMiss`]。
    EmptyStack,
    /// 栈里所有候选都不存在：同上，退 generic。
    NoMatchingFamily,
}
/// 一条文本诊断（D17：**不静默替换**，缺失字形/字体必须可观测）。
#[derive(Clone, Debug, PartialEq)]
pub enum TextDiagnostic {
    /// 该字体覆盖不到的字形（`glyph.id == 0`）。
    MissingGlyph {
        /// 字体标签（`<blob 字节数>#<TTC 索引>`）。
        font: SmolStr,
        /// 字形 id（0 = .notdef）。
        glyph: u32,
        /// 该字形所属 run 的源文本字节区间。
        text_range: Range<usize>,
    },
    /// 字体栈里**一个候选都没命中**，已退到 generic。
    FontStackMiss {
        /// 尝试过的候选（逗号分隔）。
        tried: SmolStr,
    },
    /// 行内盒的 `index` 不在 char boundary（已吸附到最近的合法边界）。
    InlineBoxOffBoundary {
        /// 盒句柄。
        id: u64,
        /// 原始（非法）偏移。
        index: usize,
    },
    /// 行内盒比整行可用宽度还宽（已降级为独立行）。
    InlineBoxOverflow {
        /// 盒句柄。
        id: u64,
        /// 盒宽（逻辑像素）。
        width: f32,
        /// 该行可用宽度（逻辑像素）。
        available: f32,
    },
    /// 该 run 依赖平台的合成粗体/斜体（v1 **不应用** 合成，只记录限制）。
    SyntheticStyle {
        /// 该 run 覆盖的源文本字节区间。
        text_range: Range<usize>,
    },
    /// 彩色字形（COLR/CBDT/sbix）被压成**单色覆盖度**渲染（R12-8）。
    ///
    /// 引擎当前只有 R8 mask 图集；彩色字形按 alpha 投影后仍能看见形状（不再是静默空白），
    /// RGBA 彩色图集是后续项。这条诊断的作用是：不让「彩色 → 单色」这个降级不可观测。
    ColorGlyphMonochrome {
        /// 字体标签（`<blob 字节数>#<TTC 索引>`）。
        font: SmolStr,
        /// 字形 id。
        glyph: u32,
        /// 该字形所属 run 的源文本字节区间。
        text_range: Range<usize>,
    },
    /// 字形图集整页都是本帧工作集 ⇒ 本帧少画了 `skipped` 个字形，下一帧重试（R12-1 / P1-①）。
    ///
    /// **每个节点每轮最多一条**（不按字形刷屏）：`skipped` 是该节点本轮被跳过的字形总数。
    /// 命中的节点会重新入下一帧的重排队列并重光栅，因此这条诊断通常只连续出现 1–2 帧；
    /// 连续 `ATLAS_FULL_ESCALATION_FRAMES`（默认 3）个 pass 仍出现则说明容量确实不足，
    /// `App::layout_text` 改为返回 `EngineError::AtlasExhausted`（可见失败优于静默缺字）。
    AtlasFull {
        /// 该节点本轮没画出的字形数（`>= 1`）。
        skipped: usize,
    },
    /// Canvas 内文本因图集租约失效而在**编码期**被丢弃（v2.1 E1b + E1d，`§11.1` 的 D-5 canvas 半边）。
    ///
    /// `Canvas` 内容归应用所有：`push_shaped_text` 之后图集换代（DPI 变化 / 字体栈切换 /
    /// 容量逃逸）**或逐 tile LRU 淘汰**（不换代）会让已写入的 `AtlasTile` 指向旧 UV，编码期为
    /// 避免花字整段丢弃失效 run（逐 tile 用分配身份 `(slot, slot_epoch)` 判活）。
    /// 引擎**无法**代应用重建 Canvas —— 应用应在句柄 `is_stale()` 后重新 `shape_text` 并用新
    /// 句柄重建 Canvas。这条诊断让「少字」不静默（D17：可见失败优于静默缺字）。
    CanvasTextStale {
        /// 本帧被丢弃的 `GlyphRun` 数（`>= 1`；每个 run 一段文本）。
        runs: usize,
    },
    /// `TextContent.runs` 里有非法区间（越界 / 不在 char boundary / 非升序），已在构造期按
    /// 归一化裁剪（v2.1 E2，`§11.2`）。**不静默**：这条诊断让「应用写错区间、样式被缩小或
    /// 丢弃」可观测（`runs` 已合法，这是唯一信号）。
    ///
    /// 每个内容一条计数（不是逐 run 刷屏）；`TextContext` 的诊断环按值去重，同一内容重排
    /// 不会让环增长。
    InvalidTextRun {
        /// 被改动（内收或丢弃）的 run 数（`>= 1`）。
        runs: usize,
    },
    /// `bind_text` 换整段文本后 runs 被裁剪 / 丢弃（v2.1 E2，`§11.2`：不静默改变样式归属）。
    TextRunClipped {
        /// 被改动（内收或丢弃）的 run 数（`>= 1`）。
        runs: usize,
    },
    /// 行内盒的图片资源未加载 / 已卸载：该 `Primitive::Image` 被跳过（v2.1 E2，`§11.2`）。
    ///
    /// 引擎不画占位图、不 panic（D17：跳过必须可观测）；应用在资源就绪后重画该节点。
    InlineBoxImageMissing {
        /// [`InlineBoxSpec::id`]（应用句柄）。
        id: u64,
    },
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{
        FontStack, InlineAlign, InlineBoxSpec, ShapedLine, TextContent, TextDiagnostic, TextStyle,
        TextWrap,
    };
    use crate::{math::Size, paint::color::Color};

    #[test]
    fn text_style_defaults_are_neutral() {
        let style = TextStyle::default();

        assert_eq!(style.size, 14.0);
        assert_eq!(style.weight, 400);
        assert_eq!(style.alignment, super::TextAlign::Start);
        assert_eq!(style.wrap, TextWrap::Wrap);
        assert!(style.family().is_none());
        assert_eq!(style.color, Color::BLACK);
        assert_eq!(style.letter_spacing, 0.0);
    }

    #[test]
    fn text_style_is_value_semantics() {
        let style = TextStyle::new(16.0).with_weight(700);
        let copy = style.clone();
        let changed = copy.with_color(Color::WHITE);

        assert_eq!(style.weight, 700, "clone must not alias");
        assert_eq!(style.color, Color::BLACK);
        assert_eq!(changed.color, Color::WHITE);
    }

    #[test]
    fn font_stack_system_ui_contains_cjk_candidates() {
        let stack = FontStack::system_ui();
        let names: Vec<&str> = stack.families().collect();

        assert!(names.contains(&"Microsoft YaHei UI"), "{names:?}");
        assert!(names.contains(&"PingFang SC"), "{names:?}");
        assert!(names.contains(&"Noto Sans CJK SC"), "{names:?}");
        assert!(names.iter().any(|name| name.contains("Segoe UI")));
        assert_eq!(stack.version(), 1);
    }

    #[test]
    fn font_stack_version_bumps_only_on_change() {
        let mut stack = FontStack::new(["A", "B"]);
        assert_eq!(stack.version(), 1);

        assert_eq!(
            stack.set_families(["A", "B"]),
            1,
            "same content keeps version"
        );
        assert_eq!(stack.set_families(["A", "C"]), 2);
        assert_eq!(stack.families().collect::<Vec<_>>(), vec!["A", "C"]);
    }

    #[test]
    fn text_content_equality_is_text_and_style() {
        let a = TextContent::new("hi", TextStyle::new(12.0));
        let b = TextContent::new("hi", TextStyle::new(12.0));
        let c = TextContent::new("hi", TextStyle::new(13.0));

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// v2.1 E2：`TextRun` 覆盖语义 + `TextContent.runs` 访问器（空 = 全段兜底样式）。
    #[test]
    fn text_content_runs_are_ordered_and_normalized() {
        use super::TextRun;

        let base = TextContent::new("a界b", TextStyle::new(16.0));
        assert!(base.runs().is_empty(), "默认无 runs");

        let red = TextStyle::new(16.0).with_color(Color::WHITE);
        let content = base.with_runs([
            TextRun::new(0..1, red.clone()),
            // `界` 占 3 字节（1..4）：越界 + 中段边界的区间必须向内收到 char boundary。
            TextRun::new(1..2, red.clone()),
            TextRun::new(4..6, red.clone()),
        ]);
        let runs = content.runs();
        assert_eq!(runs.len(), 2, "非边界空区间被归一化丢弃：{runs:?}");
        assert_eq!(runs[0].range, 0..1);
        assert_eq!(runs[1].range, 4..5, "越界端点夹到文本长度");
        assert_eq!(runs[0].style, red);
        assert_eq!(
            content.normalized_runs(),
            2,
            "构造期被改动/丢弃的 run 数必须可观测（`InvalidTextRun` 诊断来源）"
        );
        assert_eq!(content.clone(), content, "runs 参与 TextContent 相等性");
        assert_ne!(
            content,
            TextContent::new("a界b", TextStyle::new(16.0)),
            "有/无 runs 必须可区分"
        );
    }

    /// `bind_text` 换文本后的裁剪：内收 / 丢弃越界 run，返回被改动的数量。
    #[test]
    fn text_content_clips_runs_to_the_new_text() {
        use super::TextRun;

        let mut content = TextContent::new("abcdef", TextStyle::new(16.0)).with_runs([
            TextRun::new(0..2, TextStyle::new(16.0).with_weight(700)),
            TextRun::new(2..6, TextStyle::new(16.0).with_weight(800)),
        ]);
        content.text = smol_str::SmolStr::new_static("abc");
        let altered = content.clip_runs_to_text();
        assert_eq!(altered, 1, "第二个 run 被裁剪");
        assert_eq!(content.runs().len(), 2);
        assert_eq!(content.runs()[1].range, 2..3, "越界端点按新文本内收");
        // 再次裁剪是幂等的（没有新的改动）。
        assert_eq!(content.clip_runs_to_text(), 0);
        assert_eq!(content.runs()[1].range, 2..3);
    }

    /// v2.1 E2：装饰构造器 + 可绘制判定（非有限 / 非正线宽不画）。
    #[test]
    fn text_decoration_helpers_resolve_color_and_visibility() {
        let underline = super::TextDecoration::underline(1.5);
        assert!(matches!(underline, super::TextDecoration::Underline { .. }));
        assert_eq!(underline.thickness(), 1.5);
        assert!(underline.color().is_none());
        assert!(underline.is_visible());
        let colored = underline.with_color(Some(Color::WHITE));
        assert_eq!(colored.color(), Some(Color::WHITE));
        let strike = super::TextDecoration::strikethrough(2.0);
        assert!(matches!(
            strike,
            super::TextDecoration::Strikethrough { .. }
        ));
        assert!(!super::TextDecoration::underline(0.0).is_visible());
        assert!(!super::TextDecoration::underline(f32::NAN).is_visible());
    }

    /// v2.1 E2：装饰收进 `TextStyleExtras`（不撑大 `TextStyle` 的内联尺寸；真实尺寸断言在
    /// `text_payload_sizes_are_budgeted`）。
    #[test]
    fn text_decoration_lives_in_extras() {
        let plain = TextStyle::new(16.0);
        assert_eq!(plain.decoration(), None);
        let decorated = plain
            .clone()
            .with_decoration(super::TextDecoration::underline(1.0));
        assert!(decorated.decoration().is_some());
        assert_ne!(plain, decorated);
    }

    #[test]
    fn text_payload_sizes_are_budgeted() {
        // P1-2.3 新增 font_features（等宽数字 tnum），同时把 family 与它一起收进
        // `TextStyleExtras`（Blink 的 RareInheritedData 模式）：TextStyle 反而从 72 降到 56。
        // R12-9/R12-10 的新字段全部收进 `extras` 的 `Box` 与既有 padding
        // （`overflow`/`max_lines` 3 B 落在 f32/Option<Box> 的尾巴里）⇒ 尺寸不变。
        // v2.1 E2：`runs: Option<Box<ThinSlice<TextRun>>>` 是 runs 的最小内联表示（8 B）。
        // E2b：`normalized_runs: u16`（构造期归一化诊断计数）让 TextContent 从 104 到 112；
        // `TextContent` 在 `NodeKind::Text` 里是装箱的，`Node` 尺寸不变（328 B，上限 336 B）。
        // `ShapedLine` 不变（80 B；装饰几何在堆上的 `ShapedTextLine.decorations`）。
        assert_eq!(size_of::<TextStyle>(), 56);
        assert_eq!(size_of::<TextContent>(), 112);
        assert_eq!(size_of::<InlineBoxSpec>(), 72);
        assert_eq!(size_of::<ShapedLine>(), 80);
    }

    #[test]
    fn shaped_empty_has_zero_size_and_no_lines() {
        let line = ShapedLine::empty(smol_str::SmolStr::new_static(""), 3, 1, 1.0);

        assert_eq!(line.size(), Size::ZERO);
        assert!(line.lines.is_empty());
        assert!(line.first_baseline.is_none());
        assert_eq!(line.glyph_runs().count(), 0);
        assert_eq!(line.atlas_generation, 3);
    }

    #[test]
    fn diagnostics_are_comparable_values() {
        let a = TextDiagnostic::InlineBoxOffBoundary { id: 7, index: 3 };
        let b = TextDiagnostic::InlineBoxOffBoundary { id: 7, index: 3 };
        let c = TextDiagnostic::InlineBoxOverflow {
            id: 7,
            width: 10.0,
            available: 4.0,
        };

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(InlineAlign::default(), InlineAlign::Baseline);
    }

    #[test]
    fn text_style_hash_matches_for_equal_styles_and_differs_otherwise() {
        use std::hash::Hasher;

        let a = TextStyle::new(16.0).with_weight(700);
        let b = TextStyle::new(16.0).with_weight(700);
        let c = TextStyle::new(16.0).with_weight(400);

        let mut ha = ahash::AHasher::default();
        a.hash_into(&mut ha);
        let mut hb = ahash::AHasher::default();
        b.hash_into(&mut hb);
        let mut hc = ahash::AHasher::default();
        c.hash_into(&mut hc);

        assert_eq!(ha.finish(), hb.finish());
        assert_ne!(ha.finish(), hc.finish());
    }
}
