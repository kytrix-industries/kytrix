use core::ops::Range;

use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    math::{DevicePixelRatio, LogicalPx, LogicalRect, LogicalSize, px},
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextAlign {
    #[default]
    Start,
    End,
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextSlant {
    #[default]
    Normal,
    Italic,
    Oblique,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextDecoration {
    Underline {
        color: Option<Color>,
        thickness: LogicalPx,
    },
    Strikethrough {
        color: Option<Color>,
        thickness: LogicalPx,
    },
}

impl TextDecoration {
    #[must_use]
    pub const fn underline(thickness: LogicalPx) -> Self {
        Self::Underline {
            color: None,
            thickness,
        }
    }

    #[must_use]
    pub const fn strikethrough(thickness: LogicalPx) -> Self {
        Self::Strikethrough {
            color: None,
            thickness,
        }
    }

    #[must_use]
    pub const fn with_color(self, color: Option<Color>) -> Self {
        match self {
            Self::Underline { thickness, .. } => Self::Underline { color, thickness },
            Self::Strikethrough { thickness, .. } => Self::Strikethrough { color, thickness },
        }
    }

    #[must_use]
    pub const fn color(self) -> Option<Color> {
        match self {
            Self::Underline { color, .. } | Self::Strikethrough { color, .. } => color,
        }
    }

    #[must_use]
    pub const fn thickness(self) -> LogicalPx {
        match self {
            Self::Underline { thickness, .. } | Self::Strikethrough { thickness, .. } => thickness,
        }
    }

    #[must_use]
    pub fn is_visible(self) -> bool {
        let thickness = self.thickness().value();
        thickness.is_finite() && thickness > 0.0
    }

    pub(crate) fn hash_into(self, hasher: &mut ahash::AHasher) {
        use core::hash::Hash;

        let (tag, color, thickness) = match self {
            Self::Underline { color, thickness } => (0u8, color, thickness),
            Self::Strikethrough { color, thickness } => (1u8, color, thickness),
        };
        tag.hash(hasher);
        color.map(color_bits).hash(hasher);
        thickness.value().to_bits().hash(hasher);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextWrap {
    #[default]
    Wrap,
    NoWrap,
    Anywhere,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextDirection {
    #[default]
    Auto,
    Ltr,
    Rtl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextLineBreak {
    Loose,
    Normal,
    Strict,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub size: LogicalPx,
    pub weight: u16,
    pub slant: TextSlant,
    pub line_height: Option<f32>,
    pub letter_spacing: LogicalPx,
    pub color: Color,
    pub alignment: TextAlign,
    pub wrap: TextWrap,
    pub direction: TextDirection,
    pub overflow: TextOverflow,
    pub max_lines: u16,
    pub extras: Option<Box<TextStyleExtras>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThinSlice<T>(Box<[T]>);

impl<T> ThinSlice<T> {
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

fn thin_slice<T>(items: impl IntoIterator<Item = T>) -> Option<Box<ThinSlice<T>>> {
    let mut collected: Vec<T> = items.into_iter().collect();
    if collected.is_empty() {
        return None;
    }
    collected.shrink_to_fit();
    Some(Box::new(ThinSlice(collected.into_boxed_slice())))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStyleExtras {
    pub family: Option<SmolStr>,
    pub font_features: Option<Box<FeatureSet>>,
    pub locale: Option<SmolStr>,
    pub font_variations: Option<Box<FontVariationSet>>,
    pub stretch: Option<u16>,
    pub word_spacing: Option<LogicalPx>,
    pub line_break: Option<TextLineBreak>,
    pub decoration: Option<TextDecoration>,
}

pub type FeatureSet = ThinSlice<TextFeature>;

pub type FontVariationSet = ThinSlice<TextFontVariation>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextFontVariation {
    pub tag: [u8; 4],
    pub value: f32,
}

impl TextFontVariation {
    #[must_use]
    pub const fn new(tag: [u8; 4], value: f32) -> Self {
        Self { tag, value }
    }
}

pub type InlineBoxSet = ThinSlice<InlineBoxSpec>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextFeature {
    pub tag: [u8; 4],
    pub value: u16,
}

impl TextFeature {
    pub const TABULAR_NUMBERS: Self = Self {
        tag: *b"tnum",
        value: 1,
    };

    #[must_use]
    pub const fn new(tag: [u8; 4], value: u16) -> Self {
        Self { tag, value }
    }

    #[must_use]
    pub fn tag_str(self) -> String {
        String::from_utf8_lossy(&self.tag).into_owned()
    }
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            size: px(14.0),
            weight: 400,
            slant: TextSlant::Normal,
            line_height: None,
            letter_spacing: LogicalPx::ZERO,
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
    #[must_use]
    pub fn new(size: LogicalPx) -> Self {
        Self {
            size,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn family(&self) -> Option<&SmolStr> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.family.as_ref())
    }

    #[must_use]
    pub fn font_features(&self) -> Option<&FeatureSet> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.font_features.as_deref())
    }

    #[must_use]
    pub fn locale(&self) -> Option<&str> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.locale.as_deref())
    }

    #[must_use]
    pub fn font_variations(&self) -> Option<&FontVariationSet> {
        self.extras
            .as_ref()
            .and_then(|extras| extras.font_variations.as_deref())
    }

    #[must_use]
    pub fn stretch(&self) -> Option<u16> {
        self.extras.as_ref().and_then(|extras| extras.stretch)
    }

    #[must_use]
    pub fn word_spacing(&self) -> Option<LogicalPx> {
        self.extras.as_ref().and_then(|extras| extras.word_spacing)
    }

    #[must_use]
    pub fn line_break(&self) -> Option<TextLineBreak> {
        self.extras.as_ref().and_then(|extras| extras.line_break)
    }

    #[must_use]
    pub fn max_lines(&self) -> Option<u16> {
        (self.max_lines > 0).then_some(self.max_lines)
    }

    #[must_use]
    pub fn decoration(&self) -> Option<TextDecoration> {
        self.extras.as_ref().and_then(|extras| extras.decoration)
    }

    #[must_use]
    pub const fn overflow(&self) -> TextOverflow {
        self.overflow
    }

    #[must_use]
    pub fn same_shaping_inputs(&self, other: &Self) -> bool {
        self.size.value().to_bits() == other.size.value().to_bits()
            && self.weight == other.weight
            && self.slant == other.slant
            && self.line_height.map(f32::to_bits) == other.line_height.map(f32::to_bits)
            && self.letter_spacing.value().to_bits() == other.letter_spacing.value().to_bits()
            && self.wrap == other.wrap
            && self.direction == other.direction
            && self.overflow == other.overflow
            && self.max_lines == other.max_lines
            && self.extras == other.extras
    }

    fn extras_mut(&mut self) -> &mut TextStyleExtras {
        self.extras
            .get_or_insert_with(|| Box::new(TextStyleExtras::default()))
    }

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

    #[must_use]
    pub const fn with_size(mut self, size: LogicalPx) -> Self {
        self.size = size;
        self
    }

    #[must_use]
    pub fn with_family(mut self, family: impl Into<SmolStr>) -> Self {
        self.extras_mut().family = Some(family.into());
        self
    }

    #[must_use]
    pub const fn with_weight(mut self, weight: u16) -> Self {
        self.weight = weight;
        self
    }

    #[must_use]
    pub const fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    #[must_use]
    pub const fn with_line_height(mut self, factor: f32) -> Self {
        self.line_height = Some(factor);
        self
    }

    #[must_use]
    pub const fn with_letter_spacing(mut self, spacing: LogicalPx) -> Self {
        self.letter_spacing = spacing;
        self
    }

    #[must_use]
    pub const fn with_alignment(mut self, alignment: TextAlign) -> Self {
        self.alignment = alignment;
        self
    }

    #[must_use]
    pub const fn with_wrap(mut self, wrap: TextWrap) -> Self {
        self.wrap = wrap;
        self
    }

    #[must_use]
    pub const fn with_direction(mut self, direction: TextDirection) -> Self {
        self.direction = direction;
        self
    }

    #[must_use]
    pub const fn with_overflow(mut self, overflow: TextOverflow, max_lines: u16) -> Self {
        self.overflow = overflow;
        self.max_lines = max_lines;
        self
    }

    #[must_use]
    pub fn with_locale(mut self, locale: impl Into<SmolStr>) -> Self {
        self.extras_mut().locale = Some(locale.into());
        self
    }

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

    #[must_use]
    pub fn with_stretch(mut self, stretch: u16) -> Self {
        self.extras_mut().stretch = Some(stretch);
        self
    }

    #[must_use]
    pub fn with_word_spacing(mut self, word_spacing: LogicalPx) -> Self {
        self.extras_mut().word_spacing = Some(word_spacing);
        self
    }

    #[must_use]
    pub fn with_line_break(mut self, line_break: TextLineBreak) -> Self {
        self.extras_mut().line_break = Some(line_break);
        self
    }

    #[must_use]
    pub fn with_decoration(mut self, decoration: TextDecoration) -> Self {
        self.extras_mut().decoration = Some(decoration);
        self
    }

    pub(crate) fn hash_into(&self, hasher: &mut ahash::AHasher) {
        use core::hash::Hash;

        self.family().hash(hasher);
        self.size.value().to_bits().hash(hasher);
        self.weight.hash(hasher);
        (self.slant as u8).hash(hasher);
        self.line_height.map(f32::to_bits).hash(hasher);
        self.letter_spacing.value().to_bits().hash(hasher);
        (self.wrap as u8).hash(hasher);
        (self.direction as u8).hash(hasher);
        (self.overflow as u8).hash(hasher);
        self.max_lines.hash(hasher);
        self.font_features().map(|set| set.as_slice()).hash(hasher);
        self.locale().hash(hasher);
        self.stretch().hash(hasher);
        self.word_spacing()
            .map(|value| value.value().to_bits())
            .hash(hasher);
        self.line_break().map(|kind| kind as u8).hash(hasher);
        self.font_variations()
            .map(|set| {
                set.iter()
                    .map(|variation| (variation.tag, variation.value.to_bits()))
                    .collect::<Vec<_>>()
            })
            .hash(hasher);
        match self.decoration() {
            Some(decoration) => {
                true.hash(hasher);
                decoration.hash_into(hasher);
            }
            None => false.hash(hasher),
        }
    }
}

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

#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub range: Range<usize>,
    pub style: TextStyle,
}

impl TextRun {
    #[must_use]
    pub fn new(range: Range<usize>, style: TextStyle) -> Self {
        Self { range, style }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextContent {
    pub text: SmolStr,
    pub style: TextStyle,
    runs: Option<Box<ThinSlice<TextRun>>>,
    normalized_runs: u16,
    pub inline_boxes: Box<[InlineBoxSpec]>,
}

impl TextContent {
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

    #[must_use]
    pub fn with_inline_boxes(mut self, boxes: impl IntoIterator<Item = InlineBoxSpec>) -> Self {
        self.inline_boxes = boxes.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_runs(self, runs: impl IntoIterator<Item = TextRun>) -> Self {
        let mut content = self;
        content.set_runs(runs);
        content
    }

    pub(crate) fn set_runs(&mut self, runs: impl IntoIterator<Item = TextRun>) {
        let (normalized, altered) = normalize_runs(&self.text, runs);
        self.normalized_runs = u16::try_from(altered).unwrap_or(u16::MAX);
        self.runs = thin_slice(normalized);
    }

    #[must_use]
    pub fn runs(&self) -> &[TextRun] {
        self.runs.as_deref().map_or(&[], |runs| runs.as_slice())
    }

    #[must_use]
    pub(crate) fn normalized_runs(&self) -> u16 {
        self.normalized_runs
    }

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

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontStack {
    families: SmallVec<[SmolStr; 8]>,
    version: u32,
}

impl FontStack {
    #[must_use]
    pub fn system_ui() -> Self {
        Self::new(
            SYSTEM_UI_FAMILIES
                .iter()
                .map(|name| SmolStr::new_static(name)),
        )
    }

    #[must_use]
    pub fn new(families: impl IntoIterator<Item = impl Into<SmolStr>>) -> Self {
        Self {
            families: families.into_iter().map(Into::into).collect(),
            version: 1,
        }
    }

    pub fn families(&self) -> impl ExactSizeIterator<Item = &str> + Clone + '_ {
        self.families.iter().map(SmolStr::as_str)
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum InlineAlign {
    #[default]
    Baseline,
    Sub,
    Super,
    TextTop,
    TextBottom,
    Middle,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InlineBoxPaint {
    Image {
        resource: ResourceId,
        modulation: Color,
    },
    Quad {
        color: Color,
        corner_radius: [LogicalPx; 4],
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InlineBoxSpec {
    pub id: u64,
    pub index: usize,
    pub size: LogicalSize,
    pub baseline: Option<LogicalPx>,
    pub align: InlineAlign,
    pub paint: Option<InlineBoxPaint>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapedLine {
    pub text: SmolStr,
    pub lines: Vec<ShapedTextLine>,
    pub width: LogicalPx,
    pub height: LogicalPx,
    pub first_baseline: Option<LogicalPx>,
    pub atlas_generation: u32,
    pub font_stack_version: u32,
    pub scale: DevicePixelRatio,
}

impl ShapedLine {
    #[must_use]
    pub(crate) fn empty(
        text: SmolStr,
        generation: u32,
        font_stack_version: u32,
        scale: DevicePixelRatio,
    ) -> Self {
        Self {
            text,
            lines: Vec::new(),
            width: LogicalPx::ZERO,
            height: LogicalPx::ZERO,
            first_baseline: None,
            atlas_generation: generation,
            font_stack_version,
            scale,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn size(&self) -> LogicalSize {
        LogicalSize::new(self.width, self.height)
    }

    pub(crate) fn glyph_runs(&self) -> impl Iterator<Item = &ShapedGlyphRun> + '_ {
        self.lines.iter().flat_map(|line| {
            line.items.iter().filter_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => Some(run),
                ShapedTextItem::InlineBox(_) => None,
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapedTextLine {
    pub text_range: Range<usize>,
    pub baseline: LogicalPx,
    pub line_height: LogicalPx,
    pub advance: LogicalPx,
    pub items: Vec<ShapedTextItem>,
    pub decorations: Vec<ShapedDecoration>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShapedDecoration {
    pub rect: LogicalRect,
    pub color: Color,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ShapedTextItem {
    GlyphRun(ShapedGlyphRun),
    InlineBox(ShapedInlineBox),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapedGlyphRun {
    pub color: Color,
    pub text_range: Range<usize>,
    pub glyphs: Vec<PositionedGlyph>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShapedInlineBox {
    pub id: u64,
    pub rect: LogicalRect,
    pub baseline: Option<LogicalPx>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextPassReport {
    pub shaped: usize,
    pub reused: usize,
    pub skipped: usize,
    pub dirty_spans: usize,
    pub fast_path_shaped: usize,
    pub fast_path_queries: usize,
    pub fast_path_ineligible: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextLayoutReport {
    pub laid: usize,
    pub reused: usize,
    pub reruns: usize,
    pub rasterized: usize,
    pub tiles_reused: usize,
    pub atlas_rebuilds: usize,
    pub atlas_evictions: usize,
    pub skipped_glyphs: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontStackStatus {
    Ready,
    EmptyStack,
    NoMatchingFamily,
}
#[derive(Clone, Debug, PartialEq)]
pub enum TextDiagnostic {
    MissingGlyph {
        font: SmolStr,
        glyph: u32,
        text_range: Range<usize>,
    },
    FontStackMiss {
        tried: SmolStr,
    },
    InlineBoxOffBoundary {
        id: u64,
        index: usize,
    },
    InlineBoxOverflow {
        id: u64,
        width: LogicalPx,
        available: LogicalPx,
    },
    SyntheticStyle {
        text_range: Range<usize>,
    },
    ColorGlyphMonochrome {
        font: SmolStr,
        glyph: u32,
        text_range: Range<usize>,
    },
    AtlasFull {
        skipped: usize,
    },
    CanvasTextUnresolved {
        recipes: usize,
    },
    InvalidMeasureWidth {
        width: LogicalPx,
    },
    InvalidTextRun {
        runs: usize,
    },
    TextRunClipped {
        runs: usize,
    },
    InlineBoxImageMissing {
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
    use crate::{
        math::{DevicePixelRatio, LogicalSize, px},
        paint::color::Color,
    };

    #[test]
    fn text_style_defaults_are_neutral() {
        let style = TextStyle::default();

        assert_eq!(style.size, px(14.0));
        assert_eq!(style.weight, 400);
        assert_eq!(style.alignment, super::TextAlign::Start);
        assert_eq!(style.wrap, TextWrap::Wrap);
        assert!(style.family().is_none());
        assert_eq!(style.color, Color::BLACK);
        assert_eq!(style.letter_spacing, px(0.0));
    }

    #[test]
    fn text_style_is_value_semantics() {
        let style = TextStyle::new(px(16.0)).with_weight(700);
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
        let a = TextContent::new("hi", TextStyle::new(px(12.0)));
        let b = TextContent::new("hi", TextStyle::new(px(12.0)));
        let c = TextContent::new("hi", TextStyle::new(px(13.0)));

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn text_content_runs_are_ordered_and_normalized() {
        use super::TextRun;

        let base = TextContent::new("a界b", TextStyle::new(px(16.0)));
        assert!(base.runs().is_empty(), "默认无 runs");

        let red = TextStyle::new(px(16.0)).with_color(Color::WHITE);
        let content = base.with_runs([
            TextRun::new(0..1, red.clone()),
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
            TextContent::new("a界b", TextStyle::new(px(16.0))),
            "有/无 runs 必须可区分"
        );
    }

    #[test]
    fn text_content_clips_runs_to_the_new_text() {
        use super::TextRun;

        let mut content = TextContent::new("abcdef", TextStyle::new(px(16.0))).with_runs([
            TextRun::new(0..2, TextStyle::new(px(16.0)).with_weight(700)),
            TextRun::new(2..6, TextStyle::new(px(16.0)).with_weight(800)),
        ]);
        content.text = smol_str::SmolStr::new_static("abc");
        let altered = content.clip_runs_to_text();
        assert_eq!(altered, 1, "第二个 run 被裁剪");
        assert_eq!(content.runs().len(), 2);
        assert_eq!(content.runs()[1].range, 2..3, "越界端点按新文本内收");
        assert_eq!(content.clip_runs_to_text(), 0);
        assert_eq!(content.runs()[1].range, 2..3);
    }

    #[test]
    fn text_decoration_helpers_resolve_color_and_visibility() {
        let underline = super::TextDecoration::underline(px(1.5));
        assert!(matches!(underline, super::TextDecoration::Underline { .. }));
        assert_eq!(underline.thickness(), px(1.5));
        assert!(underline.color().is_none());
        assert!(underline.is_visible());
        let colored = underline.with_color(Some(Color::WHITE));
        assert_eq!(colored.color(), Some(Color::WHITE));
        let strike = super::TextDecoration::strikethrough(px(2.0));
        assert!(matches!(
            strike,
            super::TextDecoration::Strikethrough { .. }
        ));
        assert!(!super::TextDecoration::underline(px(0.0)).is_visible());
        assert!(!super::TextDecoration::underline(px(f32::NAN)).is_visible());
    }

    #[test]
    fn text_decoration_lives_in_extras() {
        let plain = TextStyle::new(px(16.0));
        assert_eq!(plain.decoration(), None);
        let decorated = plain
            .clone()
            .with_decoration(super::TextDecoration::underline(px(1.0)));
        assert!(decorated.decoration().is_some());
        assert_ne!(plain, decorated);
    }

    #[test]
    fn text_payload_sizes_are_budgeted() {
        assert_eq!(size_of::<TextStyle>(), 56);
        assert_eq!(size_of::<TextContent>(), 112);
        assert_eq!(size_of::<InlineBoxSpec>(), 72);
        assert_eq!(size_of::<ShapedLine>(), 80);
    }

    #[test]
    fn shaped_empty_has_zero_size_and_no_lines() {
        let line = ShapedLine::empty(
            smol_str::SmolStr::new_static(""),
            3,
            1,
            DevicePixelRatio::ONE,
        );

        assert_eq!(line.size(), LogicalSize::ZERO);
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
            width: px(10.0),
            available: px(4.0),
        };

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(InlineAlign::default(), InlineAlign::Baseline);
    }

    #[test]
    fn text_style_hash_matches_for_equal_styles_and_differs_otherwise() {
        use std::hash::Hasher;

        let a = TextStyle::new(px(16.0)).with_weight(700);
        let b = TextStyle::new(px(16.0)).with_weight(700);
        let c = TextStyle::new(px(16.0)).with_weight(400);

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
