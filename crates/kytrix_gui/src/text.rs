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
//! 公开路径因此是 `kytrix_gui::text::{TextStyle, FontStack, InlineBoxSpec, ShapedLine, …}`
//! （`design/phase-05-text.md` §4.1）。文本的**引擎**（字体库/图集/光栅）在 `App` 内部，
//! 应用层通过 `App::{refresh_text, set_font_stack, …}` 使用，见 §6.1。

use core::ops::Range;

use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    math::{Rect, Size},
    paint::{color::Color, primitive::PositionedGlyph},
};

pub(crate) mod atlas;
pub(crate) mod cache;
pub(crate) mod context;
pub(crate) mod inline_flow;
pub(crate) mod raster;
pub(crate) mod shape;

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

/// 一段文本的样式（**值语义**，不是句柄）。
///
/// `Clone` 而不是 `Copy`：`family` 是 `SmolStr`。字号的单位是**逻辑像素**；物理 ppem 由
/// 光栅阶段按设备 `scale` 取整（`design/phase-05-text.md` §8.3）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// 家族名覆盖；`None` = 用引擎 `FontStack`（D17 的 CJK 显式栈）。
    pub family: Option<SmolStr>,
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
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            family: None,
            size: 14.0,
            weight: 400,
            slant: TextSlant::Normal,
            line_height: None,
            letter_spacing: 0.0,
            color: Color::BLACK,
            alignment: TextAlign::Start,
            wrap: TextWrap::Wrap,
            direction: TextDirection::Auto,
        }
    }
}

impl TextStyle {
    /// 以字号建默认样式。
    #[must_use]
    pub const fn new(size: f32) -> Self {
        Self {
            family: None,
            size,
            weight: 400,
            slant: TextSlant::Normal,
            line_height: None,
            letter_spacing: 0.0,
            color: Color::BLACK,
            alignment: TextAlign::Start,
            wrap: TextWrap::Wrap,
            direction: TextDirection::Auto,
        }
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
        self.family = Some(family.into());
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

    /// 形状缓存的样式哈希（f32 全部按 `to_bits`；字段集即缓存契约，改动即失效）。
    pub(crate) fn hash_into(&self, hasher: &mut ahash::AHasher) {
        use core::hash::Hash;

        self.family.as_deref().hash(hasher);
        self.size.to_bits().hash(hasher);
        self.weight.hash(hasher);
        (self.slant as u8).hash(hasher);
        self.line_height.map(f32::to_bits).hash(hasher);
        self.letter_spacing.to_bits().hash(hasher);
        hash_color(self.color, hasher);
        (self.alignment as u8).hash(hasher);
        (self.wrap as u8).hash(hasher);
        (self.direction as u8).hash(hasher);
    }
}

fn hash_color(color: Color, hasher: &mut ahash::AHasher) {
    use core::hash::Hash;

    let (tag, components): (u8, [f32; 4]) = match color {
        Color::Srgb { r, g, b, alpha } => (0, [r, g, b, alpha]),
        Color::LinearSrgb { r, g, b, alpha } => (1, [r, g, b, alpha]),
        Color::DisplayP3 { r, g, b, alpha } => (2, [r, g, b, alpha]),
        Color::Oklab { l, a, b, alpha } => (3, [l, a, b, alpha]),
        Color::Oklch { l, c, h, alpha } => (4, [l, c, h, alpha]),
    };
    tag.hash(hasher);
    for component in components {
        component.to_bits().hash(hasher);
    }
}

/// 节点载荷：文本 + 样式 + 行内盒（`NodeKind::Text` 的内容）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextContent {
    /// 源文本。
    pub text: SmolStr,
    /// 样式。
    pub style: TextStyle,
    /// 行内盒（`<img>` / `kbd`）：参与断行与行高，位置由引擎算（§12）。
    pub inline_boxes: Box<[InlineBoxSpec]>,
}

impl TextContent {
    /// 构造（无行内盒）。
    #[must_use]
    pub fn new(text: impl Into<SmolStr>, style: TextStyle) -> Self {
        Self {
            text: text.into(),
            style,
            inline_boxes: Box::new([]),
        }
    }

    /// 链式：挂上行内盒（`NodeBuilder::inline_boxes` 的落点）。
    #[must_use]
    pub fn with_inline_boxes(mut self, boxes: impl IntoIterator<Item = InlineBoxSpec>) -> Self {
        self.inline_boxes = boxes.into_iter().collect();
        self
    }
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

/// 一次文本 pass 的统计（P06 帧记录 / L1 断言）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextPassReport {
    /// 真正跑 `RangedBuilder::build` 的次数（形状缓存 miss）。
    pub shaped: usize,
    /// 命中形状缓存、复用 `Rc<ShapedText>` 的次数。
    pub reused: usize,
    /// 形状快照未变、整节点被跳过的次数（B4：空闲帧必须只走这条路）。
    pub skipped: usize,
    /// 真正光栅化的字形数。
    pub rasterized: usize,
    /// 命中字形 tile 缓存的字形数。
    pub tiles_reused: usize,
    /// 出现 `glyph.id == 0`（.notdef）的字形数。
    pub tofu: usize,
    /// 因图集满而走「清空 + 重建」的次数。
    pub atlas_rebuilds: usize,
    /// 本次累积的脏行段数（P07 的增量上传输入）。
    pub dirty_spans: usize,
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
        assert!(style.family.is_none());
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

    #[test]
    fn text_payload_sizes_are_budgeted() {
        assert_eq!(size_of::<TextStyle>(), 72);
        assert_eq!(size_of::<TextContent>(), 112);
        assert_eq!(size_of::<InlineBoxSpec>(), 40);
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
