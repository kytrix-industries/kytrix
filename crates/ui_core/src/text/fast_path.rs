use std::{hash::Hasher, rc::Rc};

use parley::PositionedLayoutItem;
use smol_str::SmolStr;
use unicode_linebreak::linebreaks;

use super::{
    ShapedGlyphRun, ShapedLine, ShapedTextItem, ShapedTextLine, TextAlign, TextDirection,
    TextStyle,
    raster::{GlyphVariation, split_subpixel_x},
};
use crate::{
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, physical_px, px},
    paint::{color::Color, primitive::PositionedGlyph},
};

pub(crate) const ALPHABET: [char; 17] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', ',', '+', '-', '%', '/', ':',
];

pub(crate) const ADVANCE_TOLERANCE: f32 = 0.01;

fn alphabet_index(character: char) -> Option<usize> {
    ALPHABET
        .iter()
        .position(|candidate| *candidate == character)
}

fn has_line_break_opportunity(text: &str) -> bool {
    linebreaks(text).any(|(index, _)| index < text.len())
}

struct FastFont {
    font: parley::FontData,
    variation_coords: Vec<i16>,
    synthesis: fontique::Synthesis,
    chars: [Option<FastChar>; ALPHABET.len()],
    metrics: FastMetrics,
}

#[derive(Clone, Copy, Debug)]
struct FastChar {
    glyph: u32,
    advance: LogicalPx,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FastMetrics {
    pub baseline: LogicalPx,
    pub line_height: LogicalPx,
}

#[derive(Clone, Debug)]
pub(crate) struct SimpleShaped {
    glyphs: Vec<SimpleGlyph>,
    width: LogicalPx,
    metrics: FastMetrics,
    font: parley::FontData,
    variation_coords: Vec<i16>,
    synthesis: fontique::Synthesis,
    text_len: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SimpleGlyph {
    pub glyph: u32,
    pub x: LogicalPx,
}

impl SimpleShaped {
    pub(crate) fn text_len(&self) -> usize {
        self.text_len
    }

    #[cfg(test)]
    pub(crate) fn width(&self) -> LogicalPx {
        self.width
    }

    pub(crate) fn size(&self) -> LogicalSize {
        LogicalSize::new(self.width, self.metrics.line_height)
    }

    pub(crate) fn metrics(&self) -> FastMetrics {
        self.metrics
    }

    #[cfg(test)]
    pub(crate) fn glyph_at(&self, index: usize) -> u32 {
        self.glyphs.get(index).map_or(0, |glyph| glyph.glyph)
    }

    pub(crate) fn x_at(&self, index: usize) -> LogicalPx {
        self.glyphs.get(index).map_or(self.width, |glyph| glyph.x)
    }

    pub(crate) fn caret_x(&self, byte_index: usize) -> LogicalPx {
        self.x_at(byte_index.min(self.text_len))
    }

    pub(crate) fn byte_index_at(&self, x: LogicalPx) -> usize {
        for index in 0..self.glyphs.len() {
            let edge = self.x_at(index);
            let next = self.x_at(index + 1);
            if x <= edge + (next - edge) * 0.5 {
                return index;
            }
        }
        self.text_len
    }

    pub(crate) fn selection_rect(
        &self,
        content_width: LogicalPx,
        alignment: TextAlign,
        direction: TextDirection,
        anchor: usize,
        focus: usize,
    ) -> Option<LogicalRect> {
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let offset = simple_alignment_offset(self.width, content_width, alignment, direction);
        let x0 = offset + self.caret_x(start);
        let x1 = offset + self.caret_x(end);
        if (x1 - x0).abs().value() <= f32::EPSILON {
            return None;
        }
        Some(LogicalRect::new(
            x0,
            px(0.0),
            x1 - x0,
            self.metrics.line_height,
        ))
    }

    pub(crate) fn rasterize(
        &self,
        text: SmolStr,
        style: &TextStyle,
        color: Color,
        ctx: &mut super::shape::RasterCtx<'_>,
        layout_offset: LogicalPx,
    ) -> Result<ShapedLine, crate::error::EngineError> {
        let ppem = ctx.scale.to_physical(style.size).value().clamp(1.0, 512.0);
        let variation = GlyphVariation::of(&self.variation_coords, self.synthesis, ppem);
        let baseline = self.metrics.baseline;
        let mut glyphs = Vec::with_capacity(self.glyphs.len());
        for glyph in &self.glyphs {
            let (base_x, subpixel_x) =
                split_subpixel_x(ctx.scale.to_physical(glyph.x + layout_offset).value());
            let tile = ctx.glyph_atlas.ensure(
                ctx.raster,
                ctx.atlas,
                &self.font,
                glyph.glyph,
                ppem,
                subpixel_x,
                Some(&variation),
            )?;
            let Some(tile) = tile else {
                continue;
            };
            let origin_x = base_x + tile.left as f32;
            let origin_y = ctx.scale.to_physical(baseline).value().round() - tile.top as f32;
            glyphs.push(PositionedGlyph {
                tile: tile.tile,
                position: LogicalPoint::new(
                    ctx.scale.to_logical(physical_px(origin_x)),
                    ctx.scale.to_logical(physical_px(origin_y)),
                ),
                size: LogicalSize::new(
                    ctx.scale.to_logical(physical_px(tile.width as f32)),
                    ctx.scale.to_logical(physical_px(tile.height as f32)),
                ),
            });
        }
        let line = ShapedTextLine {
            text_range: 0..text.len(),
            baseline,
            line_height: self.metrics.line_height,
            advance: self.width,
            items: vec![ShapedTextItem::GlyphRun(ShapedGlyphRun {
                color,
                text_range: 0..text.len(),
                glyphs,
            })],
            decorations: Vec::new(),
        };
        Ok(ShapedLine {
            text,
            lines: vec![line],
            width: self.width,
            height: self.metrics.line_height,
            first_baseline: Some(baseline),
            atlas_generation: ctx.generation,
            color_atlas_generation: ctx.glyph_atlas.color_generation(),
            font_stack_version: 0,
            scale: ctx.scale,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FastPathStats {
    pub queries: usize,
    pub hits: usize,
    pub glyphs: usize,
    pub rejected: usize,
    pub verified: usize,
    pub ineligible_rtl: usize,
    pub ineligible_letter_spacing: usize,
    pub ineligible_wrap: usize,
    pub ineligible_alphabet: usize,
}

impl FastPathStats {
    pub(crate) fn ineligible(&self) -> usize {
        self.ineligible_rtl
            + self.ineligible_letter_spacing
            + self.ineligible_wrap
            + self.ineligible_alphabet
    }
}

pub(crate) struct DigitFastPath {
    enabled: bool,
    fonts: ahash::HashMap<FastKey, FastEntry>,
    stats: FastPathStats,
}

enum FastEntry {
    Ready {
        font: Rc<FastFont>,
        style: TextStyle,
    },
    Unsupported {
        style: TextStyle,
    },
}

impl FastEntry {
    fn style(&self) -> &TextStyle {
        match self {
            Self::Ready { style, .. } | Self::Unsupported { style } => style,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FastKey {
    font_identity: u64,
    style_digest: u64,
}

impl FastKey {
    fn new(font_identity: u64, style: &TextStyle) -> Self {
        let mut hasher = ahash::AHasher::default();
        style.hash_into(&mut hasher);
        Self {
            font_identity,
            style_digest: hasher.finish(),
        }
    }
}

impl DigitFastPath {
    pub(crate) fn new() -> Self {
        Self {
            enabled: true,
            fonts: ahash::HashMap::default(),
            stats: FastPathStats::default(),
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn reset(&mut self) {
        self.fonts.clear();
        self.stats = FastPathStats::default();
    }

    pub(crate) fn take_stats(&mut self) -> FastPathStats {
        core::mem::take(&mut self.stats)
    }

    pub(crate) fn try_shape(
        &mut self,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<crate::paint::color::ColorBrush>,
        family: &parley::FontFamily<'static>,
        font_identity: u64,
        text: &str,
        style: &TextStyle,
    ) -> Option<SimpleShaped> {
        if !self.enabled || text.is_empty() {
            return None;
        }
        self.stats.queries += 1;
        if let Err(reason) = eligibility(text, style) {
            match reason {
                Ineligible::Rtl => self.stats.ineligible_rtl += 1,
                Ineligible::LetterSpacing => self.stats.ineligible_letter_spacing += 1,
                Ineligible::Wrap => self.stats.ineligible_wrap += 1,
                Ineligible::Alphabet => self.stats.ineligible_alphabet += 1,
            }
            return None;
        }
        let key = FastKey::new(font_identity, style);
        if let Some(entry) = self
            .fonts
            .get(&key)
            .filter(|entry| entry.style().same_shaping_inputs(style))
        {
            let FastEntry::Ready { font, .. } = entry else {
                return None;
            };
            let simple = build_simple(font, text)?;
            self.stats.hits += 1;
            self.stats.glyphs += simple.glyphs.len();
            return Some(simple);
        }
        let font = match verify(font_cx, layout_cx, family, style) {
            Some(font) => {
                self.stats.verified += 1;
                let font = Rc::new(font);
                self.fonts.insert(
                    key,
                    FastEntry::Ready {
                        font: Rc::clone(&font),
                        style: style.clone(),
                    },
                );
                font
            }
            None => {
                self.stats.rejected += 1;
                self.fonts.insert(
                    key,
                    FastEntry::Unsupported {
                        style: style.clone(),
                    },
                );
                return None;
            }
        };
        let simple = build_simple(&font, text)?;
        self.stats.hits += 1;
        self.stats.glyphs += simple.glyphs.len();
        Some(simple)
    }
}

impl Default for DigitFastPath {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn simple_alignment_offset(
    line_advance: LogicalPx,
    content_width: LogicalPx,
    alignment: TextAlign,
    direction: TextDirection,
) -> LogicalPx {
    if !content_width.is_finite() || content_width <= line_advance {
        return LogicalPx::ZERO;
    }
    let extra = content_width - line_advance;
    let rtl = matches!(direction, TextDirection::Rtl);
    match alignment {
        TextAlign::Start => {
            if rtl {
                extra
            } else {
                LogicalPx::ZERO
            }
        }
        TextAlign::End => {
            if rtl {
                LogicalPx::ZERO
            } else {
                extra
            }
        }
        TextAlign::Left => LogicalPx::ZERO,
        TextAlign::Right => extra,
        TextAlign::Center => extra * 0.5,
        TextAlign::Justify => {
            if rtl {
                extra
            } else {
                LogicalPx::ZERO
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ineligible {
    Rtl,
    LetterSpacing,
    Wrap,
    Alphabet,
}

fn eligibility(text: &str, style: &TextStyle) -> Result<(), Ineligible> {
    if style.direction == TextDirection::Rtl {
        return Err(Ineligible::Rtl);
    }
    if style.letter_spacing != LogicalPx::ZERO {
        return Err(Ineligible::LetterSpacing);
    }
    if !text
        .chars()
        .all(|character| alphabet_index(character).is_some())
    {
        return Err(Ineligible::Alphabet);
    }
    if style.wrap != super::TextWrap::NoWrap && has_line_break_opportunity(text) {
        return Err(Ineligible::Wrap);
    }
    Ok(())
}

fn build_simple(font: &FastFont, text: &str) -> Option<SimpleShaped> {
    let mut glyphs = Vec::with_capacity(text.len());
    let mut x = LogicalPx::ZERO;
    for character in text.chars() {
        let slot = font.chars[alphabet_index(character)?];
        let fast_char = slot?;
        if fast_char.glyph == 0 {
            return None;
        }
        glyphs.push(SimpleGlyph {
            glyph: fast_char.glyph,
            x,
        });
        x += fast_char.advance;
    }
    Some(SimpleShaped {
        glyphs,
        width: x,
        metrics: font.metrics,
        font: font.font.clone(),
        variation_coords: font.variation_coords.clone(),
        synthesis: font.synthesis,
        text_len: text.len(),
    })
}

fn verify(
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<crate::paint::color::ColorBrush>,
    family: &parley::FontFamily<'static>,
    style: &TextStyle,
) -> Option<FastFont> {
    let mut chars: [Option<FastChar>; ALPHABET.len()] = [None; ALPHABET.len()];
    let mut font: Option<parley::FontData> = None;
    let mut variation_coords: Vec<i16> = Vec::new();
    let mut synthesis = fontique::Synthesis::default();
    let mut metrics: Option<FastMetrics> = None;
    for (index, character) in ALPHABET.iter().copied().enumerate() {
        let buffer = [character as u8];
        let probe = core::str::from_utf8(&buffer).ok()?;
        let layout = probe_layout(font_cx, layout_cx, family, style, probe)?;
        let (id, advance, probe_font, coords, probe_synthesis) = single_glyph(&layout)?;
        if id == 0 {
            return None;
        }
        if let Some(existing) = &font {
            if existing != &probe_font {
                return None;
            }
        } else {
            font = Some(probe_font);
            variation_coords = coords;
            synthesis = probe_synthesis;
        }
        let line = layout.lines().next()?;
        let line_metrics = line.metrics();
        let fast_metrics = FastMetrics {
            baseline: px(line_metrics.baseline),
            line_height: px(line_metrics.line_height),
        };
        match &metrics {
            Some(existing) if existing != &fast_metrics => return None,
            Some(_) => {}
            None => metrics = Some(fast_metrics),
        }
        chars[index] = Some(FastChar {
            glyph: id,
            advance: px(advance),
        });
    }
    let font = font?;
    let metrics = metrics?;
    let fast = FastFont {
        font,
        variation_coords,
        synthesis,
        chars,
        metrics,
    };

    let mut pairs = String::with_capacity(ALPHABET.len() * ALPHABET.len() * 2);
    for first in ALPHABET {
        for second in ALPHABET {
            pairs.push(first);
            pairs.push(second);
        }
    }
    let layout = probe_layout(font_cx, layout_cx, family, style, &pairs)?;
    verify_glyph_sequence(&layout, &fast, &pairs)?;

    let mut sample = String::with_capacity(ALPHABET.len() + 24);
    sample.extend(ALPHABET);
    sample.push_str("1,234.56-7/8%:+9");
    let layout = probe_layout(font_cx, layout_cx, family, style, &sample)?;
    verify_glyph_sequence(&layout, &fast, &sample)?;
    Some(fast)
}

fn probe_layout(
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<crate::paint::color::ColorBrush>,
    family: &parley::FontFamily<'static>,
    style: &TextStyle,
    text: &str,
) -> Option<parley::Layout<crate::paint::color::ColorBrush>> {
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.set_base_direction(match style.direction {
        TextDirection::Auto => parley::BaseDirection::Auto,
        TextDirection::Ltr => parley::BaseDirection::Ltr,
        TextDirection::Rtl => parley::BaseDirection::Rtl,
    });
    builder.push_default(parley::StyleProperty::FontFamily(family.clone()));
    super::shape::push_shaping_defaults(&mut builder, style);
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    Some(layout)
}

fn single_glyph(
    layout: &parley::Layout<crate::paint::color::ColorBrush>,
) -> Option<(u32, f32, parley::FontData, Vec<i16>, fontique::Synthesis)> {
    let line = layout.lines().next()?;
    for item in line.items() {
        let PositionedLayoutItem::GlyphRun(run) = item else {
            continue;
        };
        let mut glyphs = run.positioned_glyphs();
        let glyph = glyphs.next()?;
        if glyphs.next().is_some() {
            return None;
        }
        let advance = run.run().advance();
        let coords: Vec<i16> = run
            .run()
            .normalized_coords()
            .iter()
            .map(|coord| coord.to_bits())
            .collect();
        return Some((
            glyph.id,
            advance,
            run.run().font().clone(),
            coords,
            run.run().synthesis(),
        ));
    }
    None
}

fn verify_glyph_sequence(
    layout: &parley::Layout<crate::paint::color::ColorBrush>,
    font: &FastFont,
    text: &str,
) -> Option<()> {
    let expected = text.chars().count();
    let mut glyph_x: Vec<f32> = Vec::with_capacity(expected);
    let mut glyph_ids: Vec<u32> = Vec::with_capacity(expected);
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(run) = item else {
                continue;
            };
            for glyph in run.positioned_glyphs() {
                glyph_x.push(glyph.x);
                glyph_ids.push(glyph.id);
            }
        }
    }
    if glyph_x.len() != expected {
        return None;
    }
    let mut x = 0.0f32;
    for (position, character) in text.chars().enumerate() {
        let fast_char = font.chars[alphabet_index(character)?]?;
        if (glyph_x[position] - x).abs() > ADVANCE_TOLERANCE {
            return None;
        }
        if glyph_ids[position] != fast_char.glyph {
            return None;
        }
        x += fast_char.advance.value();
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::{
        ADVANCE_TOLERANCE, ALPHABET, DigitFastPath, FastEntry, FastFont, Ineligible, SimpleShaped,
        alphabet_index, eligibility, has_line_break_opportunity, probe_layout,
        verify_glyph_sequence,
    };
    use crate::{
        math::px,
        paint::color::ColorBrush,
        text::{
            FontStack, TextAlign, TextContent, TextDirection, TextFontVariation, TextStyle,
            TextWrap,
            context::TextContext,
            shape::{self, family_list},
        },
    };

    fn harness() -> (
        parley::FontContext,
        parley::LayoutContext<ColorBrush>,
        parley::FontFamily<'static>,
    ) {
        (
            parley::FontContext::new(),
            parley::LayoutContext::new(),
            family_list(&FontStack::system_ui(), None),
        )
    }

    fn has_system_fonts(font_cx: &mut parley::FontContext) -> bool {
        font_cx.collection.family_names().next().is_some()
    }

    struct ParleyReference {
        glyphs: Vec<(u32, f32)>,
        width: f32,
        baseline: f32,
        line_height: f32,
    }

    fn parley_reference(
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        text: &str,
        style: &TextStyle,
    ) -> ParleyReference {
        let layout =
            probe_layout(font_cx, layout_cx, family, style, text).expect("探针布局必须能建出来");
        let mut glyphs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                    for glyph in run.positioned_glyphs() {
                        glyphs.push((glyph.id, glyph.x));
                    }
                }
            }
        }
        let (baseline, line_height) = layout.lines().next().map_or((0.0, 0.0), |line| {
            let metrics = line.metrics();
            (metrics.baseline, metrics.line_height)
        });
        ParleyReference {
            glyphs,
            width: layout.width(),
            baseline,
            line_height,
        }
    }

    fn assert_matches_reference(simple: &SimpleShaped, reference: &ParleyReference, text: &str) {
        assert_eq!(
            reference.glyphs.len(),
            simple.text_len(),
            "{text}: 一字符一字形"
        );
        for (index, (glyph_id, x)) in reference.glyphs.iter().enumerate() {
            assert_eq!(
                *glyph_id,
                simple.glyph_at(index),
                "{text}: 第 {index} 个字形 id"
            );
            assert!(
                (*x - simple.x_at(index).value()).abs() <= ADVANCE_TOLERANCE,
                "{text}: 第 {index} 个字形 x {x} vs 快速路径 {}",
                simple.x_at(index)
            );
        }
        assert!(
            (reference.width - simple.width().value()).abs() <= ADVANCE_TOLERANCE,
            "{text}: 宽度 {} vs 快速路径 {}",
            reference.width,
            simple.width()
        );
        assert!(
            (reference.baseline - simple.metrics().baseline.value()).abs() <= ADVANCE_TOLERANCE,
            "{text}: baseline {} vs 快速路径 {}",
            reference.baseline,
            simple.metrics().baseline
        );
        assert!(
            (reference.line_height - simple.metrics().line_height.value()).abs()
                <= ADVANCE_TOLERANCE,
            "{text}: line_height {} vs 快速路径 {}",
            reference.line_height,
            simple.metrics().line_height
        );
    }

    fn ready_fonts(fast: &DigitFastPath) -> Vec<Rc<FastFont>> {
        fast.fonts
            .values()
            .filter_map(|entry| match entry {
                FastEntry::Ready { font, .. } => Some(Rc::clone(font)),
                FastEntry::Unsupported { .. } => None,
            })
            .collect()
    }

    fn parley_snapshot_glyphs(shaped: &shape::ShapedText) -> (Vec<(u32, f32, f32)>, f32, f32) {
        let layout = shaped.layout();
        let mut glyphs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                    for glyph in run.positioned_glyphs() {
                        glyphs.push((glyph.id, glyph.x, glyph.advance));
                    }
                }
            }
        }
        let (baseline, line_height) = layout.lines().next().map_or((0.0, 0.0), |line| {
            let metrics = line.metrics();
            (metrics.baseline, metrics.line_height)
        });
        (glyphs, baseline, line_height)
    }

    fn shape_fast(
        fast: &mut DigitFastPath,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        text: &str,
        style: &TextStyle,
    ) -> SimpleShaped {
        fast.try_shape(font_cx, layout_cx, family, 7, text, style)
            .unwrap_or_else(|| panic!("快速路径拒绝了文本 '{text}'：必须显式失败而不是跳过"))
    }

    #[test]
    fn alphabet_indices_are_stable() {
        assert_eq!(ALPHABET.len(), 17);
        assert_eq!(alphabet_index('0'), Some(0));
        assert_eq!(alphabet_index('9'), Some(9));
        assert_eq!(alphabet_index('.'), Some(10));
        assert_eq!(alphabet_index(':'), Some(16));
        assert_eq!(alphabet_index('a'), None);
        assert_eq!(alphabet_index(' '), None);
        assert_eq!(alphabet_index('中'), None);
    }

    #[test]
    fn eligibility_requires_the_fast_path_alphabet() {
        let style = TextStyle::new(px(12.0));
        assert_eq!(eligibility("1,234.56", &style), Ok(()));
        assert_eq!(
            eligibility("-1.50%", &style),
            Ok(()),
            "UAX#14：`HY × NU` 不允许在负号后断行 ⇒ 涨跌幅可走单行快速路径"
        );
        assert_eq!(
            eligibility("12/34", &style),
            Ok(()),
            "UAX#14：数值序列中的 `/` 不产生断行机会"
        );
        assert_eq!(eligibility("1 234", &style), Err(Ineligible::Alphabet));
        assert_eq!(eligibility("abc", &style), Err(Ineligible::Alphabet));
        assert_eq!(
            eligibility("-1.50%", &style.clone().with_wrap(TextWrap::NoWrap)),
            Ok(())
        );
        assert_eq!(
            eligibility("1.+", &style),
            Err(Ineligible::Wrap),
            "`.+` 在 `IS` 后、`PR` 前存在真实 UAX#14 断行机会 ⇒ 单行假设不成立"
        );
        assert_eq!(
            eligibility("1,234.56", &style.clone().with_letter_spacing(px(0.5))),
            Err(Ineligible::LetterSpacing)
        );
        assert_eq!(
            eligibility(
                "1,234.56",
                &style
                    .clone()
                    .with_font_features([crate::text::TextFeature::TABULAR_NUMBERS])
            ),
            Ok(()),
            "OpenType 特性交给验证探针判定（tnum 验证通过即可命中）"
        );
        assert_eq!(
            eligibility("1,234.56", &style.with_direction(TextDirection::Rtl)),
            Err(Ineligible::Rtl)
        );
    }

    #[test]
    fn eligibility_rejects_rtl_paragraphs_and_the_alphabet_has_only_ascii() {
        let style = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        assert_eq!(
            eligibility("42150", &style.clone().with_direction(TextDirection::Rtl)),
            Err(Ineligible::Rtl),
            "Simple 快照只能由 eligibility 放行的样式构造：显式 Rtl 必须被拒绝"
        );
        let mut assembled = String::with_capacity(ALPHABET.len());
        for character in ALPHABET {
            assert!(
                character.is_ascii(),
                "快速路径字符集必须全是 ASCII（ASCII 里没有 R/AL 类，任何段落方向都不会形成 \
                 RTL run）：ALPHABET 出现了 {character:?}"
            );
            assembled.push(character);
        }
        assert_eq!(
            eligibility(&assembled, &style),
            Ok(()),
            "完整字符集在 NoWrap 下必须可命中（否则「全 LTR run」前提会被别的拒绝理由掩盖）"
        );
    }

    #[test]
    fn uax14_break_opportunity_matches_parley() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(px(12.0));
        let mut texts: Vec<String> = Vec::with_capacity(ALPHABET.len() * ALPHABET.len() + 8);
        for first in ALPHABET {
            for second in ALPHABET {
                let mut pair = String::with_capacity(2);
                pair.push(first);
                pair.push(second);
                texts.push(pair);
            }
        }
        for sample in [
            "1.+",
            "1/-2",
            "-1.50%",
            "12/34",
            "1,234.56",
            "+42,000",
            "1:2",
            "0.00000001",
        ] {
            texts.push(String::from(sample));
        }
        for text in texts {
            let mut layout = probe_layout(&mut font_cx, &mut layout_cx, &family, &style, &text)
                .expect("probe layout");
            layout.break_all_lines(Some(0.0));
            let parley_has_internal_break = layout.lines().count() > 1;
            assert_eq!(
                has_line_break_opportunity(&text),
                parley_has_internal_break,
                "UAX#14 断行机会与 parley 不一致: {text:?}"
            );
        }
    }

    #[test]
    fn ineligible_reasons_are_counted() {
        let (mut font_cx, mut layout_cx, family) = harness();
        let style = TextStyle::new(px(12.0));
        let mut fast = DigitFastPath::new();
        let cases: [(&str, TextStyle); 5] = [
            ("abc", style.clone()),
            ("1 234", style.clone()),
            ("1,234.56", style.clone().with_letter_spacing(px(0.5))),
            ("1.+", style.clone()),
            ("1,234.56", style.with_direction(TextDirection::Rtl)),
        ];
        for (text, case_style) in &cases {
            assert!(
                fast.try_shape(&mut font_cx, &mut layout_cx, &family, 9, text, case_style)
                    .is_none(),
                "{text:?} must be rejected by the static conditions"
            );
        }
        let stats = fast.take_stats();
        assert_eq!(stats.queries, cases.len(), "静态拒绝也计入候选分母");
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.ineligible_alphabet, 2, "abc / 含空格");
        assert_eq!(stats.ineligible_letter_spacing, 1);
        assert_eq!(stats.ineligible_wrap, 1);
        assert_eq!(stats.ineligible_rtl, 1);
        assert_eq!(stats.ineligible(), cases.len());
        assert_eq!(stats.verified + stats.rejected, 0, "静态拒绝不得进入验证");
    }

    #[test]
    fn fast_path_matches_parley_advances_and_positions() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(px(12.0));
        let tnum_style = TextStyle::new(px(12.0))
            .with_font_features([crate::text::TextFeature::TABULAR_NUMBERS]);
        let mut fast = DigitFastPath::new();
        let probes: [(&str, &TextStyle); 10] = [
            ("0", &style),
            ("1,234.56", &style),
            ("9999999", &style),
            ("-1.50%", &style),
            ("12/34", &style),
            ("1:2", &style),
            ("0.00000001", &style),
            ("+42,000", &style),
            ("1,234.56", &tnum_style),
            ("-1.50%", &tnum_style),
        ];
        for (text, probe_style) in probes {
            let simple = shape_fast(
                &mut fast,
                &mut font_cx,
                &mut layout_cx,
                &family,
                text,
                probe_style,
            );
            let reference =
                parley_reference(&mut font_cx, &mut layout_cx, &family, text, probe_style);
            assert_matches_reference(&simple, &reference, text);
        }
        let total = probes.len();
        let stats = fast.take_stats();
        assert_eq!(stats.queries, total);
        assert_eq!(stats.hits, total, "所有探针都必须走快速路径");
        assert_eq!(stats.verified, 2, "普通样式与 tnum 样式各验证一次");
        assert_eq!(stats.rejected, 0);
        assert_eq!(stats.ineligible(), 0, "全部探针都满足静态条件");
    }

    #[test]
    fn disabling_the_fast_path_returns_none() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let text = "1,234.56";
        let content = TextContent::new(text, style.clone());

        let mut enabled = TextContext::new();
        let fast_shaped = enabled.shape_cached(&content).0;
        let stats = enabled.take_fast_path_stats();
        assert_eq!((stats.queries, stats.hits), (1, 1), "开启时必须走快速路径");
        assert_eq!((stats.verified, stats.rejected), (1, 0));

        let mut disabled = TextContext::new();
        disabled.set_digit_fast_path(false);
        assert!(!disabled.digit_fast_path_enabled());
        let parley_shaped = disabled.shape_cached(&content).0;
        let stats = disabled.take_fast_path_stats();
        assert_eq!(
            (stats.queries, stats.hits, stats.verified, stats.rejected),
            (0, 0, 0, 0),
            "关闭后不得查询 / 命中 / 验证"
        );

        let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &style);
        let (parley_glyphs, baseline, line_height) = parley_snapshot_glyphs(&parley_shaped);
        assert_eq!(parley_glyphs.len(), reference.glyphs.len(), "字形数");
        for (index, ((id, x, _advance), (reference_id, reference_x))) in parley_glyphs
            .iter()
            .zip(reference.glyphs.iter())
            .enumerate()
        {
            assert_eq!(
                id, reference_id,
                "关闭后第 {index} 个 glyph id 必须与参考相同"
            );
            assert_eq!(
                x.to_bits(),
                reference_x.to_bits(),
                "关闭后第 {index} 个字形 x 必须与参考逐位相等：{x} vs {reference_x}"
            );
        }
        assert_eq!(
            baseline.to_bits(),
            reference.baseline.to_bits(),
            "baseline 逐位相等"
        );
        assert_eq!(
            line_height.to_bits(),
            reference.line_height.to_bits(),
            "行高逐位相等"
        );
        assert_eq!(
            parley_shaped.max_content_size().width.value().to_bits(),
            reference.width.to_bits(),
            "max-content 宽度逐位相等"
        );

        let mut fast = DigitFastPath::new();
        let simple = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &style,
        );
        assert_eq!(simple.text_len(), parley_glyphs.len());
        for (index, (id, x, _advance)) in parley_glyphs.iter().enumerate() {
            assert_eq!(
                simple.glyph_at(index),
                *id,
                "第 {index} 个字形 id 必须与 parley 相同"
            );
            assert!(
                (simple.x_at(index).value() - x).abs() <= ADVANCE_TOLERANCE,
                "第 {index} 个字形 x {} vs parley {x}",
                simple.x_at(index)
            );
        }
        let width = parley_shaped.max_content_size().width;
        assert!(
            (fast_shaped.max_content_size().width - width).abs() <= px(ADVANCE_TOLERANCE),
            "端到端 max-content 宽度必须一致"
        );
        for byte_index in 0..=text.len() {
            let fast_caret = fast_shaped.caret_rect(width, TextAlign::Start, byte_index);
            let parley_caret = parley_shaped.caret_rect(width, TextAlign::Start, byte_index);
            assert!(
                (fast_caret.origin.x - parley_caret.origin.x).abs() <= px(ADVANCE_TOLERANCE),
                "byte {byte_index}: caret x {} vs {}",
                fast_caret.origin.x,
                parley_caret.origin.x
            );
            assert!(
                (fast_caret.size.height - parley_caret.size.height).abs() <= px(ADVANCE_TOLERANCE),
                "byte {byte_index}: 行高 {} vs {}",
                fast_caret.size.height,
                parley_caret.size.height
            );
        }
    }

    #[test]
    fn fast_path_alignment_matches_parley() {
        let (mut font_cx, _, _) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let text = "1,234.56";
        let content_width = 200.0f32;
        let alignments = [
            TextAlign::Start,
            TextAlign::End,
            TextAlign::Left,
            TextAlign::Right,
            TextAlign::Center,
            TextAlign::Justify,
        ];
        for direction in [TextDirection::Auto, TextDirection::Ltr, TextDirection::Rtl] {
            let style = TextStyle::new(px(12.0))
                .with_wrap(TextWrap::NoWrap)
                .with_direction(direction);
            let content = TextContent::new(text, style);

            let mut enabled = TextContext::new();
            let fast = enabled.shape_cached(&content).0;
            let stats = enabled.take_fast_path_stats();
            if direction == TextDirection::Rtl {
                assert_eq!(
                    (stats.queries, stats.hits),
                    (1, 0),
                    "RTL 数字必须在 eligibility 直接退回 parley（候选计入分母）"
                );
                assert_eq!(stats.ineligible_rtl, 1);
                continue;
            }
            assert_eq!(stats.queries, 1, "{direction:?}");
            assert_eq!(stats.hits, 1, "{direction:?}");

            let mut disabled = TextContext::new();
            disabled.set_digit_fast_path(false);
            let parley = disabled.shape_cached(&content).0;

            for alignment in alignments {
                for byte_index in 0..=text.len() {
                    let fast_caret = fast.caret_rect(px(content_width), alignment, byte_index);
                    let parley_caret = parley.caret_rect(px(content_width), alignment, byte_index);
                    assert!(
                        (fast_caret.origin.x - parley_caret.origin.x).abs()
                            <= px(ADVANCE_TOLERANCE),
                        "{direction:?} {alignment:?} byte {byte_index}: caret {} vs {}",
                        fast_caret.origin.x,
                        parley_caret.origin.x
                    );
                }
                let fast_selection = fast.selection_rects(px(content_width), alignment, 1, 5);
                let parley_selection = parley.selection_rects(px(content_width), alignment, 1, 5);
                assert_eq!(
                    fast_selection.len(),
                    parley_selection.len(),
                    "{direction:?} {alignment:?}: selection rect count"
                );
                for (fast_rect, parley_rect) in fast_selection.iter().zip(&parley_selection) {
                    assert!(
                        (fast_rect.origin.x - parley_rect.origin.x).abs() <= px(ADVANCE_TOLERANCE),
                        "{direction:?} {alignment:?}: selection x {} vs {}",
                        fast_rect.origin.x,
                        parley_rect.origin.x
                    );
                    assert!(
                        (fast_rect.size.width - parley_rect.size.width).abs()
                            <= px(ADVANCE_TOLERANCE),
                        "{direction:?} {alignment:?}: selection width {} vs {}",
                        fast_rect.size.width,
                        parley_rect.size.width
                    );
                }
            }
        }
    }

    #[test]
    fn alignment_offset_is_bounded_for_degenerate_widths() {
        use super::simple_alignment_offset;
        for width in [f32::NAN, f32::INFINITY, -1.0, 0.0, 10.0] {
            let offset =
                simple_alignment_offset(px(10.0), px(width), TextAlign::Right, TextDirection::Ltr);
            assert!(
                offset.is_finite() && offset >= px(0.0),
                "width={width}: offset={offset}"
            );
            assert_eq!(offset, px(0.0), "width={width} must not create free space");
        }
        assert!(
            (simple_alignment_offset(px(10.0), px(20.0), TextAlign::Center, TextDirection::Ltr)
                - 5.0)
                .abs()
                .value()
                < 1e-6
        );
    }

    #[test]
    fn ineligible_text_falls_back() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(px(12.0));
        let mut fast = DigitFastPath::new();
        for text in ["1 234", "abc", "数字", "1.5 USDT", "1.+"] {
            assert!(
                fast.try_shape(&mut font_cx, &mut layout_cx, &family, 1, text, &style)
                    .is_none(),
                "{text} must not use the fast path"
            );
        }
        let stats = fast.take_stats();
        assert_eq!(
            stats.queries, 5,
            "所有候选都计入分母（旧实现只在 eligible 后计数）"
        );
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.ineligible_alphabet, 4, "含空格/非 ASCII 的四个候选");
        assert_eq!(stats.ineligible_wrap, 1, "`1.+` 有真实断行机会");
        assert_eq!(
            (stats.verified, stats.rejected),
            (0, 0),
            "静态拒绝不得进入验证（省一次探针塑形）"
        );
    }

    #[test]
    fn sequence_probe_rejects_contextual_glyph_changes() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(px(12.0));
        let font = super::verify(&mut font_cx, &mut layout_cx, &family, &style)
            .expect("host font must pass verification for the digit alphabet");
        let text = "1,234.56-7/8";
        let layout = probe_layout(&mut font_cx, &mut layout_cx, &family, &style, text)
            .expect("probe layout");
        assert_eq!(verify_glyph_sequence(&layout, &font, text), Some(()));
        let changed = "1,234.57";
        let layout = probe_layout(&mut font_cx, &mut layout_cx, &family, &style, changed)
            .expect("probe layout");
        assert_eq!(
            verify_glyph_sequence(&layout, &font, "1,234.56"),
            None,
            "序列校验必须发现 glyph id 与单字符探针不一致"
        );
    }

    #[test]
    fn font_tables_are_keyed_by_size() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let mut fast = DigitFastPath::new();
        let small = TextStyle::new(px(10.0));
        let large = TextStyle::new(px(22.0));
        let first = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            "123",
            &small,
        );
        let second = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            "123",
            &large,
        );
        assert!(
            second.width() > first.width(),
            "larger font must have larger advances"
        );
        let stats = fast.take_stats();
        assert_eq!(
            (stats.queries, stats.verified),
            (2, 2),
            "不同字号各自验证一次"
        );
        assert_eq!(fast.fonts.len(), 2);
    }

    #[test]
    fn caret_and_selection_use_prefix_advances() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let mut fast = DigitFastPath::new();
        let simple = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            "12345",
            &style,
        );
        assert_eq!(simple.caret_x(0), px(0.0));
        assert!((simple.caret_x(5) - simple.width()).abs() <= px(1e-4));
        assert!(simple.caret_x(2) > simple.caret_x(1));
        let rect = simple
            .selection_rect(px(200.0), TextAlign::Start, TextDirection::Ltr, 1, 3)
            .expect("non-empty selection");
        assert!((rect.origin.x - simple.caret_x(1)).abs() <= px(1e-4));
        assert!((rect.size.width - (simple.caret_x(3) - simple.caret_x(1))).abs() <= px(1e-4));
        assert!(
            simple
                .selection_rect(px(200.0), TextAlign::Start, TextDirection::Ltr, 2, 2)
                .is_none(),
            "collapsed selection"
        );
    }

    #[test]
    fn different_line_heights_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let doubled = base.clone().with_line_height(2.0);
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let normal = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let tall = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &doubled,
        );
        let stats = fast.take_stats();
        assert_eq!(stats.queries, 2);
        assert_eq!(stats.hits, 2);
        assert_eq!(
            stats.verified, 2,
            "两种行高必须各自验证一次（旧键在这里只会验证 1 次）"
        );
        assert_eq!(stats.rejected, 0);
        assert_eq!(fast.fonts.len(), 2, "两种行高必须是两条验证表项");
        assert!(
            (tall.metrics().line_height.value() - base.size.value() * 2.0).abs()
                <= ADVANCE_TOLERANCE,
            "line_height 必须由 parley 探针实测：{}",
            tall.metrics().line_height
        );
        assert!(
            (normal.metrics().line_height - tall.metrics().line_height).abs() > px(1.0),
            "默认行高与 2 倍行高必须不同：{} vs {}",
            normal.metrics().line_height,
            tall.metrics().line_height
        );
        let fonts = ready_fonts(&fast);
        assert_eq!(fonts.len(), 2);
        assert!(
            !Rc::ptr_eq(&fonts[0], &fonts[1]),
            "不同 line_height 不得共享 FastFont（metrics 会串味）"
        );
        for (simple, style) in [(&normal, &base), (&tall, &doubled)] {
            let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, style);
            assert_matches_reference(simple, &reference, text);
        }
    }

    #[test]
    fn different_stretches_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let condensed = base.clone().with_stretch(75);
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let normal = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let stretched = fast.try_shape(&mut font_cx, &mut layout_cx, &family, 7, text, &condensed);
        let stats = fast.take_stats();
        assert_eq!(stats.queries, 2);
        assert_eq!(
            fast.fonts.len(),
            2,
            "两种 stretch 必须是两条验证表项（旧键会串成一条）"
        );
        let base_reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &base);
        assert_matches_reference(&normal, &base_reference, text);
        match stretched {
            Some(stretched) => {
                assert_eq!((stats.hits, stats.verified, stats.rejected), (2, 2, 0));
                let fonts = ready_fonts(&fast);
                assert_eq!(fonts.len(), 2);
                assert!(
                    !Rc::ptr_eq(&fonts[0], &fonts[1]),
                    "不同 stretch 不得共享 FastFont"
                );
                let reference =
                    parley_reference(&mut font_cx, &mut layout_cx, &family, text, &condensed);
                assert_matches_reference(&stretched, &reference, text);
            }
            None => {
                assert_eq!(
                    (stats.hits, stats.verified, stats.rejected),
                    (1, 1, 1),
                    "拒绝必须计入 rejected，且不得影响另一条样式"
                );
            }
        }
    }

    #[test]
    fn different_font_variations_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let heavy = base
            .clone()
            .with_font_variations([TextFontVariation::new(*b"wght", 700.0)]);
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let normal = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let varied = fast.try_shape(&mut font_cx, &mut layout_cx, &family, 7, text, &heavy);
        let stats = fast.take_stats();
        assert_eq!(stats.queries, 2);
        assert_eq!(
            fast.fonts.len(),
            2,
            "两种 font_variations 必须是两条验证表项（旧键会串成一条）"
        );
        let base_reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &base);
        assert_matches_reference(&normal, &base_reference, text);
        match varied {
            Some(varied) => {
                assert_eq!((stats.hits, stats.verified, stats.rejected), (2, 2, 0));
                let fonts = ready_fonts(&fast);
                assert_eq!(fonts.len(), 2);
                assert!(
                    !Rc::ptr_eq(&fonts[0], &fonts[1]),
                    "不同 font_variations 不得共享 FastFont"
                );
                let reference =
                    parley_reference(&mut font_cx, &mut layout_cx, &family, text, &heavy);
                assert_matches_reference(&varied, &reference, text);
            }
            None => {
                assert_eq!(
                    (stats.hits, stats.verified, stats.rejected),
                    (1, 1, 1),
                    "拒绝必须计入 rejected，且不得影响另一条样式"
                );
            }
        }
    }

    #[test]
    fn different_locales_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let localized = base.clone().with_locale("zh-Hans");
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let plain = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let zh = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &localized,
        );
        let stats = fast.take_stats();
        assert_eq!(
            (stats.queries, stats.hits, stats.verified, stats.rejected),
            (2, 2, 2, 0),
            "两种 locale 必须各自验证一次并各自命中"
        );
        assert_eq!(fast.fonts.len(), 2, "locale 必须参与 FastKey");
        let fonts = ready_fonts(&fast);
        assert_eq!(fonts.len(), 2);
        assert!(
            !Rc::ptr_eq(&fonts[0], &fonts[1]),
            "不同 locale 不得共享 FastFont"
        );
        for (simple, style) in [(&plain, &base), (&zh, &localized)] {
            let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, style);
            assert_matches_reference(simple, &reference, text);
        }
    }
}
