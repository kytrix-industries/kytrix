use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
};

use parley::{
    FontContext, FontStyle as ParleyFontStyle, FontWeight as ParleyFontWeight, LayoutContext,
    LineHeight as ParleyLineHeight, OverflowWrap as ParleyOverflowWrap, PositionedLayoutItem,
    StyleProperty, TextWrapMode as ParleyTextWrapMode, WordBreak as ParleyWordBreak,
    layout::{Alignment, AlignmentOptions},
};
use smol_str::{SmolStr, format_smolstr};
use taffy::AvailableSpace;

use super::{
    InlineBoxSpec, ShapedDecoration, ShapedGlyphRun, ShapedInlineBox, ShapedLine, ShapedTextItem,
    ShapedTextLine, TextAlign, TextContent, TextDecoration, TextDiagnostic, TextDirection,
    TextLineBreak, TextOverflow, TextRun, TextSlant, TextStyle, TextWrap,
    atlas::{GlyphAtlas, GlyphAtlasStats},
    fast_path::SimpleShaped,
    inline_flow,
    raster::{GlyphRasterizer, GlyphVariation, split_subpixel_x},
};
use crate::{
    error::EngineError,
    math::{Point, Rect, Size},
    paint::{
        color::{Color, ColorBrush},
        primitive::PositionedGlyph,
    },
    render::atlas::AtlasAllocator,
};

#[derive(Clone, Debug)]
pub(crate) struct EllipsisGlyphs {
    pub glyphs: Vec<(u32, f32)>,
    pub font: parley::FontData,
    pub variation_coords: Vec<i16>,
    pub synthesis: fontique::Synthesis,
    pub advance: f32,
}

impl EllipsisGlyphs {
    pub(crate) fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

pub(crate) struct RasterCtx<'a> {
    pub raster: &'a mut GlyphRasterizer,
    pub glyph_atlas: &'a mut GlyphAtlas,
    pub atlas: &'a mut AtlasAllocator,
    pub generation: u32,
    pub scale: f32,
}

pub(crate) struct ShapedText {
    text: SmolStr,
    style: TextStyle,
    runs: Box<[TextRun]>,
    normalized_runs: u16,
    font_stack_version: u32,
    kind: ShapedKind,
    inline_diagnostics: Vec<TextDiagnostic>,
    inline_boxes: Box<[InlineBoxSpec]>,
    measure_memo: RefCell<Option<(u32, Size)>>,
    measure_breaks: Cell<u32>,
}

enum ShapedKind {
    Parley(RefCell<ParleySnapshot>),
    Simple(SimpleShaped),
}

struct ParleySnapshot {
    layout: parley::Layout<ColorBrush>,
    min_size: Size,
    max_size: Size,
    broken_at: Option<u32>,
    aligned_at: Option<u8>,
}

impl ParleySnapshot {
    fn new(mut layout: parley::Layout<ColorBrush>, max_lines: Option<u16>) -> Self {
        layout.break_all_lines(None);
        let max_size = Size::new(layout.width(), visible_height(&layout, max_lines));
        let widths = layout.calculate_content_widths();
        layout.break_all_lines(Some(widths.min));
        let min_size = Size::new(layout.width(), visible_height(&layout, max_lines));
        layout.break_all_lines(None);
        Self {
            layout,
            min_size,
            max_size,
            broken_at: None,
            aligned_at: None,
        }
    }

    fn break_and_align(&mut self, width: Option<f32>, alignment: Alignment) -> f32 {
        let key = width.map(f32::to_bits);
        if self.broken_at != key {
            self.layout.break_all_lines(width);
            self.broken_at = key;
            self.aligned_at = None;
        }
        let align_key = alignment_key(alignment);
        if self.aligned_at != Some(align_key) {
            self.layout.align(alignment, AlignmentOptions::default());
            self.aligned_at = Some(align_key);
        }
        self.layout.width()
    }

    fn visible_height(&self, max_lines: Option<u16>) -> f32 {
        visible_height(&self.layout, max_lines)
    }
}

fn visible_height(layout: &parley::Layout<ColorBrush>, max_lines: Option<u16>) -> f32 {
    let Some(max_lines) = max_lines else {
        return layout.height();
    };
    let lines = layout.lines();
    if lines.len() <= usize::from(max_lines) {
        return layout.height();
    }
    lines
        .take(usize::from(max_lines))
        .map(|line| line.metrics().line_height)
        .sum()
}

impl ShapedText {
    pub(crate) fn shape(
        font_cx: &mut FontContext,
        layout_cx: &mut LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        content: &TextContent,
        font_stack_version: u32,
        run_families: &[(core::ops::Range<usize>, parley::FontFamily<'static>)],
    ) -> Self {
        let style = &content.style;
        let mut inline_diagnostics: Vec<TextDiagnostic> = Vec::new();
        if content.normalized_runs() > 0 {
            inline_diagnostics.push(TextDiagnostic::InvalidTextRun {
                runs: usize::from(content.normalized_runs()),
            });
        }
        let mut builder = layout_cx.ranged_builder(font_cx, &content.text, 1.0, true);
        builder.set_base_direction(match style.direction {
            TextDirection::Auto => parley::BaseDirection::Auto,
            TextDirection::Ltr => parley::BaseDirection::Ltr,
            TextDirection::Rtl => parley::BaseDirection::Rtl,
        });
        builder.push_default(StyleProperty::FontFamily(family.clone()));
        push_shaping_defaults(&mut builder, style);
        for run in content.runs() {
            let run_family = run_families
                .iter()
                .find(|(range, _)| *range == run.range)
                .map_or_else(|| family.clone(), |(_, family)| family.clone());
            push_run_style(&mut builder, &run.style, run_family, run.range.clone());
        }
        for spec in &content.inline_boxes {
            let (inline_box, diagnostic) = inline_flow::to_inline_box(spec, &content.text);
            if let Some(diagnostic) = diagnostic {
                inline_diagnostics.push(diagnostic);
            }
            builder.push_inline_box(inline_box);
        }
        let layout = builder.build(&content.text);
        Self {
            text: content.text.clone(),
            style: style.clone(),
            runs: content.runs().to_vec().into_boxed_slice(),
            normalized_runs: content.normalized_runs(),
            font_stack_version,
            kind: ShapedKind::Parley(RefCell::new(ParleySnapshot::new(layout, style.max_lines()))),
            inline_diagnostics,
            inline_boxes: content.inline_boxes.clone(),
            measure_memo: RefCell::new(None),
            measure_breaks: Cell::new(0),
        }
    }

    pub(crate) fn simple(
        content: &TextContent,
        simple: SimpleShaped,
        font_stack_version: u32,
    ) -> Self {
        debug_assert!(
            content.runs().is_empty(),
            "数字快速路径不接受富文本 runs（TextContext::shape_cached 已拒绝）"
        );
        Self {
            text: content.text.clone(),
            style: content.style.clone(),
            runs: Box::new([]),
            normalized_runs: 0,
            font_stack_version,
            kind: ShapedKind::Simple(simple),
            inline_diagnostics: Vec::new(),
            inline_boxes: content.inline_boxes.clone(),
            measure_memo: RefCell::new(None),
            measure_breaks: Cell::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn runs(&self) -> &[TextRun] {
        &self.runs
    }

    pub(crate) fn normalized_runs(&self) -> u16 {
        self.normalized_runs
    }

    pub(crate) fn inline_boxes(&self) -> &[InlineBoxSpec] {
        &self.inline_boxes
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    #[cfg(test)]
    pub(crate) fn style(&self) -> &TextStyle {
        &self.style
    }

    #[cfg(test)]
    pub(crate) fn blank_for_text_test(text: SmolStr, style: TextStyle) -> Self {
        Self {
            text,
            style: style.clone(),
            runs: Box::new([]),
            normalized_runs: 0,
            font_stack_version: 1,
            kind: ShapedKind::Parley(RefCell::new(ParleySnapshot::new(
                parley::Layout::new(),
                style.max_lines(),
            ))),
            inline_diagnostics: Vec::new(),
            inline_boxes: Box::new([]),
            measure_memo: RefCell::new(None),
            measure_breaks: Cell::new(0),
        }
    }

    pub(crate) fn same_shaping_inputs(&self, style: &TextStyle, runs: &[TextRun]) -> bool {
        self.style.same_shaping_inputs(style) && self.runs.as_ref() == runs
    }

    pub(crate) fn has_only_ltr_runs(&self) -> bool {
        match &self.kind {
            ShapedKind::Parley(snapshot) => snapshot
                .borrow()
                .layout
                .lines()
                .all(|line| line.runs().all(|run| !run.is_rtl())),
            ShapedKind::Simple(_) => {
                debug_assert!(
                    self.style.direction != TextDirection::Rtl,
                    "Simple 快照不应带显式 RTL 段落方向（`eligibility` 应已拒绝）"
                );
                true
            }
        }
    }

    pub(crate) fn max_content_size(&self) -> Size {
        match &self.kind {
            ShapedKind::Parley(snapshot) => snapshot.borrow().max_size,
            ShapedKind::Simple(simple) => simple.size(),
        }
    }

    pub(crate) fn measure(
        &self,
        _known: taffy::Size<Option<f32>>,
        available: taffy::Size<AvailableSpace>,
    ) -> Size {
        if self.text.is_empty() {
            return Size::ZERO;
        }
        let max_lines = self.style.max_lines();
        match &self.kind {
            ShapedKind::Simple(simple) => simple.size(),
            ShapedKind::Parley(snapshot) => match available.width {
                AvailableSpace::MinContent => snapshot.borrow().min_size,
                AvailableSpace::MaxContent => snapshot.borrow().max_size,
                AvailableSpace::Definite(width) => {
                    let width = width.max(0.0);
                    let bits = width.to_bits();
                    if let Some((cached, size)) = *self.measure_memo.borrow()
                        && cached == bits
                    {
                        return size;
                    }
                    let mut snapshot = snapshot.borrow_mut();
                    let size = if width >= snapshot.max_size.width {
                        snapshot.max_size
                    } else if width <= snapshot.min_size.width {
                        snapshot.min_size
                    } else {
                        self.measure_breaks.set(self.measure_breaks.get() + 1);
                        let measured_width =
                            snapshot.break_and_align(Some(width), Alignment::Start);
                        Size::new(measured_width, snapshot.visible_height(max_lines))
                    };
                    *self.measure_memo.borrow_mut() = Some((bits, size));
                    size
                }
            },
        }
    }

    pub(crate) fn approx_bytes(&self) -> usize {
        let (glyphs, runs) = match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let snapshot = snapshot.borrow();
                let mut glyphs = 0usize;
                let mut runs = 0usize;
                for line in snapshot.layout.lines() {
                    for item in line.items() {
                        if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                            runs += 1;
                            glyphs += run.glyphs().count();
                        }
                    }
                }
                (glyphs, runs)
            }
            ShapedKind::Simple(simple) => (simple.text_len(), 1),
        };
        let inline_boxes = core::mem::size_of_val(self.inline_boxes.as_ref());
        let inline_diagnostics = self.inline_diagnostics.capacity() * 64;
        let runs_bytes: usize = self
            .runs
            .iter()
            .map(|run| {
                core::mem::size_of::<TextRun>()
                    + run
                        .style
                        .extras
                        .as_deref()
                        .map_or(0, core::mem::size_of_val)
            })
            .sum();
        let extras = self
            .style
            .extras
            .as_deref()
            .map_or(0, core::mem::size_of_val);
        core::mem::size_of::<Self>()
            + self.text.len()
            + glyphs * 24
            + runs * 96
            + inline_boxes
            + inline_diagnostics
            + extras
            + runs_bytes
    }

    pub(crate) fn rasterize(
        &self,
        content_width: f32,
        alignment: TextAlign,
        color: Color,
        ctx: &mut RasterCtx<'_>,
        ellipsis: Option<&EllipsisGlyphs>,
    ) -> Result<(ShapedLine, Vec<TextDiagnostic>, GlyphAtlasStats), EngineError> {
        if self.text.is_empty() {
            return Ok((
                ShapedLine::empty(
                    self.text.clone(),
                    ctx.generation,
                    self.font_stack_version,
                    ctx.scale,
                ),
                Vec::new(),
                ctx.glyph_atlas.take_stats(),
            ));
        }
        let simple_offset = if let ShapedKind::Simple(simple) = &self.kind {
            super::fast_path::simple_alignment_offset(
                simple.size().width,
                content_width,
                alignment,
                self.style.direction,
            )
        } else {
            0.0
        };
        let (mut shaped, diagnostics, line_offset) = match &self.kind {
            ShapedKind::Simple(simple) => {
                let shaped =
                    simple.rasterize(self.text.clone(), &self.style, color, ctx, simple_offset)?;
                (shaped, Vec::new(), simple_offset)
            }
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                let width =
                    snapshot.break_and_align(Some(content_width.max(0.0)), align_of(alignment));
                let line_count = snapshot.layout.lines().len();
                let visible_last = max_visible_lines(&self.style, line_count);
                let line_offset = if visible_last == 0 {
                    0.0
                } else {
                    snapshot
                        .layout
                        .lines()
                        .nth(visible_last - 1)
                        .map_or(0.0, |line| line.metrics().offset)
                };
                let (shaped, diagnostics) = rasterize_parley(
                    &snapshot.layout,
                    width,
                    &self.text,
                    color,
                    &self.runs,
                    &self.style,
                    ctx,
                    &self.inline_diagnostics,
                )?;
                (shaped, diagnostics, line_offset)
            }
        };
        apply_overflow(
            &mut shaped,
            &self.style,
            content_width,
            color,
            ctx,
            ellipsis,
            line_offset,
        )?;
        Ok((shaped, diagnostics, ctx.glyph_atlas.take_stats()))
    }

    fn with_broken_layout<R>(
        &self,
        width: f32,
        alignment: TextAlign,
        body: impl FnOnce(&parley::Layout<ColorBrush>) -> R,
    ) -> Option<R> {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(0.0)), align_of(alignment));
                Some(body(&snapshot.layout))
            }
            ShapedKind::Simple(_) => None,
        }
    }

    pub(crate) fn selection_rects(
        &self,
        width: f32,
        alignment: TextAlign,
        anchor: usize,
        focus: usize,
    ) -> Vec<Rect> {
        if let ShapedKind::Simple(simple) = &self.kind {
            return simple
                .selection_rect(width, alignment, self.style.direction, anchor, focus)
                .into_iter()
                .collect();
        }
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::selection_rects(layout, anchor, focus)
        })
        .unwrap_or_default()
    }

    pub(crate) fn caret_rect(&self, width: f32, alignment: TextAlign, byte_index: usize) -> Rect {
        if let ShapedKind::Simple(simple) = &self.kind {
            let metrics = simple.metrics();
            let offset = super::fast_path::simple_alignment_offset(
                simple.size().width,
                width,
                alignment,
                self.style.direction,
            );
            return Rect::new(
                offset + simple.caret_x(byte_index),
                0.0,
                1.0,
                metrics.line_height,
            );
        }
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::caret_rect(layout, byte_index)
        })
        .unwrap_or_default()
    }

    pub(crate) fn byte_index_at(&self, width: f32, alignment: TextAlign, point: Point) -> usize {
        if let ShapedKind::Simple(simple) = &self.kind {
            if point.y < 0.0 {
                return 0;
            }
            let metrics = simple.metrics();
            if point.y > metrics.line_height {
                return simple.text_len();
            }
            let offset = super::fast_path::simple_alignment_offset(
                simple.size().width,
                width,
                alignment,
                self.style.direction,
            );
            return simple.byte_index_at(point.x - offset);
        }
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::byte_index_at(layout, self.text.as_str(), point)
        })
        .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> std::cell::Ref<'_, parley::Layout<ColorBrush>> {
        std::cell::Ref::map(
            match &self.kind {
                ShapedKind::Parley(snapshot) => snapshot.borrow(),
                ShapedKind::Simple(_) => panic!("simple fast-path snapshots have no parley layout"),
            },
            |snapshot| &snapshot.layout,
        )
    }

    #[cfg(test)]
    pub(crate) fn layout_at(&self, width: f32) -> parley::Layout<ColorBrush> {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(0.0)), Alignment::Start);
                snapshot.layout.clone()
            }
            ShapedKind::Simple(_) => panic!("simple fast-path snapshots have no parley layout"),
        }
    }

    #[cfg(test)]
    pub(crate) fn measure_breaks(&self) -> u32 {
        self.measure_breaks.get()
    }

    #[cfg(test)]
    pub(crate) fn first_glyph_x_at(&self, width: f32) -> f32 {
        let mut layout = self.layout_at(width);
        layout.align(align_of(self.style.alignment), AlignmentOptions::default());
        layout
            .lines()
            .next()
            .and_then(|line| line.items().next())
            .and_then(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    run.positioned_glyphs().next().map(|glyph| glyph.x)
                }
                PositionedLayoutItem::InlineBox(_) => None,
            })
            .unwrap_or(0.0)
    }
}

pub(super) fn push_shaping_defaults<'a>(
    builder: &mut parley::RangedBuilder<'a, ColorBrush>,
    style: &TextStyle,
) {
    builder.push_default(StyleProperty::Brush(ColorBrush { color: style.color }));
    builder.push_default(StyleProperty::FontSize(style.size));
    builder.push_default(StyleProperty::FontWeight(ParleyFontWeight::new(f32::from(
        style.weight,
    ))));
    builder.push_default(StyleProperty::FontStyle(match style.slant {
        TextSlant::Normal => ParleyFontStyle::Normal,
        TextSlant::Italic => ParleyFontStyle::Italic,
        TextSlant::Oblique => ParleyFontStyle::Oblique(None),
    }));
    builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
    builder.push_default(StyleProperty::LineHeight(match style.line_height {
        None => ParleyLineHeight::NORMAL,
        Some(factor) => ParleyLineHeight::FontSizeRelative(factor),
    }));
    if let Some(features) = style.font_features() {
        let features: Vec<parley::FontFeature> = features
            .iter()
            .map(|feature| {
                parley::FontFeature::new(
                    parley::setting::Tag::from_bytes(feature.tag),
                    feature.value,
                )
            })
            .collect();
        builder.push_default(StyleProperty::FontFeatures(parley::FontFeatures::List(
            Cow::Owned(features),
        )));
    }
    if let Some(stretch) = style.stretch() {
        builder.push_default(StyleProperty::FontWidth(
            parley::FontWidth::from_percentage(f32::from(stretch)),
        ));
    }
    if let Some(word_spacing) = style.word_spacing() {
        builder.push_default(StyleProperty::WordSpacing(word_spacing));
    }
    if let Some(locale) = style.locale()
        && let Ok(language) = parley::Language::parse(locale)
    {
        builder.push_default(StyleProperty::Locale(Some(language)));
    }
    if let Some(variations) = style.font_variations() {
        let variations: Vec<parley::FontVariation> = variations
            .iter()
            .map(|variation| parley::FontVariation {
                tag: parley::setting::Tag::from_bytes(variation.tag),
                value: variation.value,
            })
            .collect();
        builder.push_default(StyleProperty::FontVariations(parley::FontVariations::List(
            Cow::Owned(variations),
        )));
    }
    if let Some(line_break) = style.line_break() {
        builder.push_default(StyleProperty::LineBreak(parley_line_break(line_break)));
    }
    builder.push_default(StyleProperty::WhiteSpaceCollapse(
        parley::WhiteSpaceCollapse::Preserve,
    ));
    match style.wrap {
        TextWrap::Wrap => {}
        TextWrap::NoWrap => {
            builder.push_default(StyleProperty::TextWrapMode(ParleyTextWrapMode::NoWrap));
        }
        TextWrap::Anywhere => {
            builder.push_default(StyleProperty::WordBreak(ParleyWordBreak::BreakAll));
            builder.push_default(StyleProperty::OverflowWrap(ParleyOverflowWrap::Anywhere));
        }
    }
}

fn push_run_style<'a>(
    builder: &mut parley::RangedBuilder<'a, ColorBrush>,
    style: &TextStyle,
    family: parley::FontFamily<'static>,
    range: core::ops::Range<usize>,
) {
    builder.push(StyleProperty::FontFamily(family), range.clone());
    builder.push(
        StyleProperty::Brush(ColorBrush { color: style.color }),
        range.clone(),
    );
    builder.push(StyleProperty::FontSize(style.size), range.clone());
    builder.push(
        StyleProperty::FontWeight(ParleyFontWeight::new(f32::from(style.weight))),
        range.clone(),
    );
    builder.push(
        StyleProperty::FontStyle(match style.slant {
            TextSlant::Normal => ParleyFontStyle::Normal,
            TextSlant::Italic => ParleyFontStyle::Italic,
            TextSlant::Oblique => ParleyFontStyle::Oblique(None),
        }),
        range.clone(),
    );
    builder.push(
        StyleProperty::LetterSpacing(style.letter_spacing),
        range.clone(),
    );
    builder.push(
        StyleProperty::LineHeight(match style.line_height {
            None => ParleyLineHeight::NORMAL,
            Some(factor) => ParleyLineHeight::FontSizeRelative(factor),
        }),
        range.clone(),
    );
    if let Some(features) = style.font_features() {
        let features: Vec<parley::FontFeature> = features
            .iter()
            .map(|feature| {
                parley::FontFeature::new(
                    parley::setting::Tag::from_bytes(feature.tag),
                    feature.value,
                )
            })
            .collect();
        builder.push(
            StyleProperty::FontFeatures(parley::FontFeatures::List(Cow::Owned(features))),
            range.clone(),
        );
    }
    if let Some(stretch) = style.stretch() {
        builder.push(
            StyleProperty::FontWidth(parley::FontWidth::from_percentage(f32::from(stretch))),
            range.clone(),
        );
    }
    if let Some(word_spacing) = style.word_spacing() {
        builder.push(StyleProperty::WordSpacing(word_spacing), range.clone());
    }
    if let Some(locale) = style.locale()
        && let Ok(language) = parley::Language::parse(locale)
    {
        builder.push(StyleProperty::Locale(Some(language)), range.clone());
    }
    if let Some(variations) = style.font_variations() {
        let variations: Vec<parley::FontVariation> = variations
            .iter()
            .map(|variation| parley::FontVariation {
                tag: parley::setting::Tag::from_bytes(variation.tag),
                value: variation.value,
            })
            .collect();
        builder.push(
            StyleProperty::FontVariations(parley::FontVariations::List(Cow::Owned(variations))),
            range.clone(),
        );
    }
    if let Some(line_break) = style.line_break() {
        builder.push(
            StyleProperty::LineBreak(parley_line_break(line_break)),
            range.clone(),
        );
    }
    match style.wrap {
        TextWrap::Wrap => {}
        TextWrap::NoWrap => {
            builder.push(
                StyleProperty::TextWrapMode(ParleyTextWrapMode::NoWrap),
                range,
            );
        }
        TextWrap::Anywhere => {
            builder.push(
                StyleProperty::WordBreak(ParleyWordBreak::BreakAll),
                range.clone(),
            );
            builder.push(
                StyleProperty::OverflowWrap(ParleyOverflowWrap::Anywhere),
                range,
            );
        }
    }
}

pub(crate) fn measure_ellipsis(
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<ColorBrush>,
    family: &parley::FontFamily<'static>,
    style: &TextStyle,
) -> Option<EllipsisGlyphs> {
    for text in ["\u{2026}", "..."] {
        let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
        builder.set_base_direction(parley::BaseDirection::Ltr);
        builder.push_default(StyleProperty::FontFamily(family.clone()));
        push_shaping_defaults(&mut builder, style);
        let mut layout = builder.build(text);
        layout.break_all_lines(None);
        let mut glyphs: Vec<(u32, f32)> = Vec::new();
        let mut font: Option<parley::FontData> = None;
        let mut variation_coords: Vec<i16> = Vec::new();
        let mut synthesis = fontique::Synthesis::default();
        let mut advance = 0.0f32;
        let mut missing = false;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                if font.is_none() {
                    font = Some(run.run().font().clone());
                    variation_coords = run
                        .run()
                        .normalized_coords()
                        .iter()
                        .map(|coord| coord.to_bits())
                        .collect();
                    synthesis = run.run().synthesis();
                }
                for glyph in run.positioned_glyphs() {
                    if glyph.id == 0 {
                        missing = true;
                        break;
                    }
                    glyphs.push((glyph.id, glyph.advance));
                    advance += glyph.advance;
                }
            }
            if missing {
                break;
            }
        }
        if !missing && !glyphs.is_empty() {
            return Some(EllipsisGlyphs {
                glyphs,
                font: font?,
                variation_coords,
                synthesis,
                advance,
            });
        }
    }
    None
}

fn parley_line_break(kind: TextLineBreak) -> parley::LineBreak {
    match kind {
        TextLineBreak::Loose => parley::LineBreak::Loose,
        TextLineBreak::Normal => parley::LineBreak::Normal,
        TextLineBreak::Strict => parley::LineBreak::Strict,
    }
}

const UNDERLINE_OFFSET_EM: f32 = 0.10;
const STRIKETHROUGH_OFFSET_EM: f32 = 0.30;

fn line_content_end(text: &str, line_range: &core::ops::Range<usize>) -> usize {
    let bytes = text.as_bytes();
    let mut end = line_range.end.min(text.len());
    while end > line_range.start && matches!(bytes.get(end - 1), Some(b'\n' | b'\r')) {
        end -= 1;
    }
    end
}

fn decoration_segments(
    line: &parley::Line<'_, ColorBrush>,
    baseline: f32,
    text: &str,
    runs: &[TextRun],
    base_style: &TextStyle,
    base_color: Color,
) -> Vec<ShapedDecoration> {
    let line_range = line.text_range();
    let line_end = line_content_end(text, &line_range);
    let decorated: Vec<(core::ops::Range<usize>, TextDecoration, f32, Color)> = runs
        .iter()
        .filter_map(|run| {
            let decoration = run.style.decoration()?;
            if !decoration.is_visible() {
                return None;
            }
            let start = run.range.start.max(line_range.start);
            let end = run.range.end.min(line_end);
            (start < end).then_some((start..end, decoration, run.style.size, run.style.color))
        })
        .collect();

    let mut segments: Vec<(core::ops::Range<usize>, TextDecoration, f32, Color)> = Vec::new();
    if let Some(decoration) = base_style.decoration()
        && decoration.is_visible()
    {
        let mut cursor = line_range.start;
        for (range, ..) in &decorated {
            if range.start > cursor {
                segments.push((cursor..range.start, decoration, base_style.size, base_color));
            }
            cursor = cursor.max(range.end);
        }
        if cursor < line_end {
            segments.push((cursor..line_end, decoration, base_style.size, base_color));
        }
    }
    segments.extend(decorated);

    let mut out = Vec::with_capacity(segments.len());
    for (range, decoration, em, text_color) in segments {
        let Some((left, width)) = decoration_span(line, text, range) else {
            continue;
        };
        let thickness = decoration.thickness();
        let (top, color) = match decoration {
            TextDecoration::Underline { color, .. } => (
                baseline + em * UNDERLINE_OFFSET_EM,
                color.unwrap_or(text_color),
            ),
            TextDecoration::Strikethrough { color, .. } => (
                baseline - em * STRIKETHROUGH_OFFSET_EM,
                color.unwrap_or(text_color),
            ),
        };
        out.push(ShapedDecoration {
            rect: Rect::new(left, top - thickness * 0.5, width, thickness),
            color,
        });
    }
    out
}

fn decoration_span(
    line: &parley::Line<'_, ColorBrush>,
    text: &str,
    range: core::ops::Range<usize>,
) -> Option<(f32, f32)> {
    let start = range.start.min(text.len());
    let end = range.end.min(text.len());
    if start >= end {
        return None;
    }
    let mut first: Option<parley::Cluster<'_, ColorBrush>> = None;
    let mut last: Option<parley::Cluster<'_, ColorBrush>> = None;
    for run in line.runs() {
        for cluster in run.visual_clusters() {
            let cluster_range = cluster.text_range();
            if cluster_range.end <= start || cluster_range.start >= end {
                continue;
            }
            first.get_or_insert(cluster);
            last = Some(cluster);
        }
    }
    let first = first?;
    let last = last?;
    let x0 = first.visual_offset()?;
    let x1 = last.visual_offset()? + last.advance();
    let left = x0.min(x1);
    let width = (x1 - x0).abs();
    (width > 0.0 && left.is_finite() && width.is_finite()).then_some((left, width))
}

fn current_run_color(
    runs: &[TextRun],
    range: &core::ops::Range<usize>,
    shaped_color: Color,
    base_color: Color,
) -> Color {
    let mut overlaps = false;
    for run in runs {
        if run.range.start <= range.start && range.end <= run.range.end {
            return run.style.color;
        }
        overlaps |= run.range.start < range.end && range.start < run.range.end;
    }
    if overlaps { shaped_color } else { base_color }
}

fn rasterize_parley(
    layout: &parley::Layout<ColorBrush>,
    content_width: f32,
    text: &SmolStr,
    color: Color,
    runs: &[TextRun],
    base_style: &TextStyle,
    ctx: &mut RasterCtx<'_>,
    inline_diagnostics: &[TextDiagnostic],
) -> Result<(ShapedLine, Vec<TextDiagnostic>), EngineError> {
    let scale = ctx.scale;
    let mut diagnostics: Vec<TextDiagnostic> = inline_diagnostics.to_vec();
    let mut lines = Vec::with_capacity(layout.lines().len());
    for line in layout.lines() {
        let metrics = line.metrics();
        let mut items = Vec::new();
        let mut cluster_cache: Option<(
            core::ops::Range<usize>,
            Vec<(core::ops::Range<usize>, usize)>,
        )> = None;
        let mut cluster_cursor: Option<(core::ops::Range<usize>, usize)> = None;
        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    let parley_run = run.run();
                    let font = parley_run.font().clone();
                    let run_text_range = parley_run.text_range();
                    let (cached_range, clusters) = cluster_cache.get_or_insert_with(|| {
                        let clusters: Vec<(core::ops::Range<usize>, usize)> = parley_run
                            .visual_clusters()
                            .map(|cluster| (cluster.text_range(), cluster.glyphs().count()))
                            .collect();
                        (run_text_range.clone(), clusters)
                    });
                    if *cached_range != run_text_range {
                        *cached_range = run_text_range.clone();
                        *clusters = parley_run
                            .visual_clusters()
                            .map(|cluster| (cluster.text_range(), cluster.glyphs().count()))
                            .collect();
                    }
                    let mut cursor = match &cluster_cursor {
                        Some((range, cursor)) if *range == run_text_range => *cursor,
                        _ => 0,
                    };
                    let item_glyph_count = run.glyphs().count();
                    let mut item_range = run_text_range.clone();
                    let mut remaining = item_glyph_count;
                    let mut first = true;
                    while remaining > 0 {
                        let Some((cluster_range, cluster_glyphs)) = clusters.get(cursor) else {
                            break;
                        };
                        cursor += 1;
                        if first {
                            item_range = cluster_range.clone();
                            first = false;
                        } else {
                            item_range.start = item_range.start.min(cluster_range.start);
                            item_range.end = item_range.end.max(cluster_range.end);
                        }
                        remaining = remaining.saturating_sub(*cluster_glyphs);
                    }
                    cluster_cursor = Some((run_text_range, cursor));
                    let item_color =
                        current_run_color(runs, &item_range, run.style().brush.color, color);
                    let ppem = (parley_run.font_size() * scale).clamp(1.0, 512.0);
                    if parley_run.synthesis().any() {
                        diagnostics.push(TextDiagnostic::SyntheticStyle {
                            text_range: item_range.clone(),
                        });
                    }
                    let normalized_coords: Vec<i16> = parley_run
                        .normalized_coords()
                        .iter()
                        .map(|coord| coord.to_bits())
                        .collect();
                    let variation =
                        GlyphVariation::of(&normalized_coords, parley_run.synthesis(), ppem);
                    let mut glyphs = Vec::with_capacity(item_glyph_count);
                    for glyph in run.positioned_glyphs() {
                        if glyph.id == 0 {
                            diagnostics.push(TextDiagnostic::MissingGlyph {
                                font: font_label(&font),
                                glyph: glyph.id,
                                text_range: item_range.clone(),
                            });
                        }
                        let Some((positioned, color_glyph)) =
                            emit_glyph(ctx, &font, glyph.id, ppem, &variation, glyph.x, glyph.y)?
                        else {
                            continue;
                        };
                        if color_glyph {
                            diagnostics.push(TextDiagnostic::ColorGlyphMonochrome {
                                font: font_label(&font),
                                glyph: glyph.id,
                                text_range: item_range.clone(),
                            });
                        }
                        glyphs.push(positioned);
                    }
                    items.push(ShapedTextItem::GlyphRun(ShapedGlyphRun {
                        color: item_color,
                        text_range: item_range,
                        glyphs,
                    }));
                }
                PositionedLayoutItem::InlineBox(box_) => {
                    let rect = Rect::new(box_.x, box_.y, box_.width, box_.height);
                    if rect.size.width > content_width && content_width > 0.0 {
                        diagnostics.push(TextDiagnostic::InlineBoxOverflow {
                            id: box_.id,
                            width: rect.size.width,
                            available: content_width,
                        });
                    }
                    items.push(ShapedTextItem::InlineBox(ShapedInlineBox {
                        id: box_.id,
                        rect,
                        baseline: box_.baseline,
                    }));
                }
            }
        }
        let decorations =
            decoration_segments(&line, metrics.baseline, text, runs, base_style, color);
        lines.push(ShapedTextLine {
            text_range: line.text_range(),
            baseline: metrics.baseline,
            line_height: metrics.line_height,
            advance: metrics.advance,
            items,
            decorations,
        });
    }
    let shaped = ShapedLine {
        text: text.clone(),
        lines,
        width: layout.width(),
        height: layout.height(),
        first_baseline: layout.first_baseline(),
        atlas_generation: ctx.generation,
        font_stack_version: 0,
        scale,
    };
    Ok((shaped, diagnostics))
}

fn emit_glyph(
    ctx: &mut RasterCtx<'_>,
    font: &parley::FontData,
    glyph_id: u32,
    ppem: f32,
    variation: &GlyphVariation,
    logical_x: f32,
    logical_y: f32,
) -> Result<Option<(PositionedGlyph, bool)>, EngineError> {
    let scale = ctx.scale;
    let (base_x, subpixel_x) = split_subpixel_x(logical_x * scale);
    let tile = ctx.glyph_atlas.ensure(
        ctx.raster,
        ctx.atlas,
        font,
        glyph_id,
        ppem,
        subpixel_x,
        Some(variation),
    )?;
    let Some(tile) = tile else {
        return Ok(None);
    };
    let origin_x = base_x + tile.left as f32;
    let origin_y = (logical_y * scale).round() - tile.top as f32;
    Ok(Some((
        PositionedGlyph {
            tile: tile.tile,
            position: Point::new(origin_x / scale, origin_y / scale),
            size: Size::new(tile.width as f32 / scale, tile.height as f32 / scale),
        },
        tile.color,
    )))
}

fn apply_overflow(
    shaped: &mut ShapedLine,
    style: &TextStyle,
    content_width: f32,
    color: Color,
    ctx: &mut RasterCtx<'_>,
    ellipsis: Option<&EllipsisGlyphs>,
    line_offset: f32,
) -> Result<(), EngineError> {
    if shaped.lines.is_empty() {
        return Ok(());
    }
    let max_lines = max_visible_lines(style, shaped.lines.len());
    let truncated = max_lines < shaped.lines.len();
    if truncated {
        shaped.lines.truncate(max_lines);
    }
    let mut overflowing = truncated;
    if content_width > 0.0 {
        overflowing |= shaped.width > content_width + 1e-3;
    }
    if overflowing
        && style.overflow() == TextOverflow::Ellipsis
        && let Some(ellipsis) = ellipsis
        && !ellipsis.is_empty()
    {
        clip_with_ellipsis(
            shaped,
            ellipsis,
            content_width,
            color,
            style,
            ctx,
            line_offset,
        )?;
        if let Some(last) = shaped.lines.last_mut() {
            clamp_line_decorations(last, line_offset);
        }
    }
    shaped.width = shaped
        .lines
        .iter()
        .map(|line| line.advance)
        .fold(0.0f32, f32::max);
    shaped.height = shaped.lines.iter().map(|line| line.line_height).sum();
    Ok(())
}

fn clamp_line_decorations(line: &mut ShapedTextLine, line_offset: f32) {
    let min = line_offset;
    let max = line_offset + line.advance;
    for decoration in &mut line.decorations {
        let left = decoration.rect.origin.x.max(min);
        let right = (decoration.rect.origin.x + decoration.rect.size.width).min(max);
        if right > left {
            decoration.rect.origin.x = left;
            decoration.rect.size.width = right - left;
        } else {
            decoration.rect.size.width = 0.0;
        }
    }
    line.decorations
        .retain(|decoration| decoration.rect.size.width > 0.0);
}

fn max_visible_lines(style: &TextStyle, line_count: usize) -> usize {
    style
        .max_lines()
        .map_or(line_count, |max| (usize::from(max)).max(1).min(line_count))
}

fn clip_with_ellipsis(
    shaped: &mut ShapedLine,
    ellipsis: &EllipsisGlyphs,
    content_width: f32,
    color: Color,
    style: &TextStyle,
    ctx: &mut RasterCtx<'_>,
    line_offset: f32,
) -> Result<(), EngineError> {
    let Some(last) = shaped.lines.last_mut() else {
        return Ok(());
    };
    let ppem = (style.size * ctx.scale).clamp(1.0, 512.0);
    let variation = GlyphVariation::of(&ellipsis.variation_coords, ellipsis.synthesis, ppem);
    let baseline = last.baseline;

    let glyph_count = last
        .items
        .iter()
        .map(|item| match item {
            ShapedTextItem::GlyphRun(run) => run.glyphs.len(),
            ShapedTextItem::InlineBox(_) => 0,
        })
        .sum();
    let mut starts: Vec<f32> = Vec::with_capacity(glyph_count);
    for item in &last.items {
        if let ShapedTextItem::GlyphRun(run) = item {
            starts.extend(
                run.glyphs
                    .iter()
                    .map(|glyph| glyph.position.x - line_offset),
            );
        }
    }

    let mut prefix_len = 0usize;
    let mut prefix_end = 0.0f32;
    for (index, _) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(last.advance);
        if content_width > 0.0 && end + line_offset + ellipsis.advance > content_width + 1e-3 {
            break;
        }
        prefix_len = index + 1;
        prefix_end = end;
    }

    let mut remaining = prefix_len;
    for item in &mut last.items {
        let ShapedTextItem::GlyphRun(run) = item else {
            continue;
        };
        let keep = remaining.min(run.glyphs.len());
        run.glyphs.truncate(keep);
        remaining -= keep;
    }

    let mut rasterized = Vec::with_capacity(ellipsis.glyphs.len());
    let mut x = prefix_end;
    for (glyph_id, advance) in &ellipsis.glyphs {
        if let Some((positioned, _color)) = emit_glyph(
            ctx,
            &ellipsis.font,
            *glyph_id,
            ppem,
            &variation,
            x + line_offset,
            baseline,
        )? {
            rasterized.push(positioned);
        }
        x += advance;
    }
    if prefix_len == 0 {
        if !matches!(last.items.first(), Some(ShapedTextItem::GlyphRun(_))) {
            last.items.insert(
                0,
                ShapedTextItem::GlyphRun(ShapedGlyphRun {
                    color,
                    text_range: 0..0,
                    glyphs: Vec::new(),
                }),
            );
        }
        if let Some(ShapedTextItem::GlyphRun(run)) = last.items.first_mut() {
            run.glyphs.extend(rasterized);
        }
    } else if let Some(ShapedTextItem::GlyphRun(run)) = last
        .items
        .iter_mut()
        .rev()
        .find(|item| matches!(item, ShapedTextItem::GlyphRun(run) if !run.glyphs.is_empty()))
    {
        run.glyphs.extend(rasterized);
    }
    last.advance = prefix_end + ellipsis.advance;
    Ok(())
}

fn align_of(alignment: TextAlign) -> Alignment {
    match alignment {
        TextAlign::Start => Alignment::Start,
        TextAlign::End => Alignment::End,
        TextAlign::Left => Alignment::Left,
        TextAlign::Center => Alignment::Center,
        TextAlign::Right => Alignment::Right,
        TextAlign::Justify => Alignment::Justify,
    }
}

fn alignment_key(alignment: Alignment) -> u8 {
    match alignment {
        Alignment::Start => 0,
        Alignment::End => 1,
        Alignment::Left => 2,
        Alignment::Center => 3,
        Alignment::Right => 4,
        Alignment::Justify => 5,
    }
}

fn font_label(font: &parley::FontData) -> SmolStr {
    format_smolstr!("{}#{}", font.data.data().len(), font.index)
}

pub(crate) fn family_list(
    stack: &super::FontStack,
    custom: Option<&str>,
) -> parley::FontFamily<'static> {
    let extra = usize::from(custom.is_some());
    let mut list: Vec<parley::FontFamilyName<'static>> =
        Vec::with_capacity(stack.families().len() + 1 + extra);
    if let Some(custom) = custom {
        list.push(parley::FontFamilyName::Named(Cow::Owned(custom.to_owned())));
    }
    for name in stack.families() {
        list.push(parley::FontFamilyName::Named(Cow::Owned(name.to_owned())));
    }
    list.push(parley::FontFamilyName::Generic(
        parley::GenericFamily::SansSerif,
    ));
    parley::FontFamily::List(Cow::Owned(list))
}

#[cfg(test)]
mod tests {
    use taffy::AvailableSpace;

    use super::family_list;
    use crate::text::{
        FontStack, InlineAlign, InlineBoxSpec, ShapedTextItem, TextAlign, TextContent,
        TextDecoration, TextDiagnostic, TextDirection, TextRun, TextStyle, TextWrap,
    };

    struct Harness {
        font_cx: parley::FontContext,
        layout_cx: parley::LayoutContext<crate::paint::color::ColorBrush>,
        family: parley::FontFamily<'static>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                font_cx: parley::FontContext::new(),
                layout_cx: parley::LayoutContext::new(),
                family: family_list(&FontStack::system_ui(), None),
            }
        }

        fn shape(&mut self, text: &str, style: TextStyle) -> super::ShapedText {
            self.shape_with(text, style, &[])
        }

        fn shape_with(
            &mut self,
            text: &str,
            style: TextStyle,
            inline: &[InlineBoxSpec],
        ) -> super::ShapedText {
            let content = TextContent::new(text, style).with_inline_boxes(inline.to_vec());
            self.shape_content(&content)
        }

        fn shape_content(&mut self, content: &TextContent) -> super::ShapedText {
            super::ShapedText::shape(
                &mut self.font_cx,
                &mut self.layout_cx,
                &self.family,
                content,
                1,
                &[],
            )
        }
    }

    fn definite(width: f32) -> taffy::Size<AvailableSpace> {
        taffy::Size {
            width: AvailableSpace::Definite(width),
            height: AvailableSpace::Definite(f32::INFINITY),
        }
    }

    fn none_known() -> taffy::Size<Option<f32>> {
        taffy::Size {
            width: None,
            height: None,
        }
    }

    fn min_max_widths(shaped: &super::ShapedText) -> (f32, f32) {
        let layout = shaped.layout();
        let widths = layout.calculate_content_widths();
        (widths.min, widths.max)
    }

    fn first_glyph_tile(line: &crate::text::ShapedLine) -> crate::paint::atlas::AtlasTile {
        line.lines[0]
            .items
            .iter()
            .find_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.tile),
                ShapedTextItem::InlineBox(_) => None,
            })
            .expect("the first item must be a glyph with coverage")
    }

    fn all_glyphs(line: &crate::text::ShapedLine) -> Vec<crate::paint::primitive::PositionedGlyph> {
        line.glyph_runs()
            .flat_map(|run| run.glyphs.iter().copied())
            .collect()
    }

    fn rasterize_line(
        shaped: &super::ShapedText,
        width: f32,
        scale: f32,
        raster: &mut crate::text::raster::GlyphRasterizer,
        glyph_atlas: &mut crate::text::atlas::GlyphAtlas,
        atlas: &mut crate::render::atlas::AtlasAllocator,
    ) -> (
        crate::text::ShapedLine,
        Vec<crate::text::TextDiagnostic>,
        crate::text::atlas::GlyphAtlasStats,
    ) {
        rasterize_line_with_ellipsis(shaped, width, scale, raster, glyph_atlas, atlas, None)
    }

    fn rasterize_line_with_ellipsis(
        shaped: &super::ShapedText,
        width: f32,
        scale: f32,
        raster: &mut crate::text::raster::GlyphRasterizer,
        glyph_atlas: &mut crate::text::atlas::GlyphAtlas,
        atlas: &mut crate::render::atlas::AtlasAllocator,
        ellipsis: Option<&super::EllipsisGlyphs>,
    ) -> (
        crate::text::ShapedLine,
        Vec<crate::text::TextDiagnostic>,
        crate::text::atlas::GlyphAtlasStats,
    ) {
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster,
            glyph_atlas,
            atlas,
            generation,
            scale,
        };
        shaped
            .rasterize(
                width,
                shaped.style().alignment,
                crate::paint::color::Color::BLACK,
                &mut ctx,
                ellipsis,
            )
            .expect("rasterize must not fail")
    }

    fn glyph_runner() -> (
        crate::text::raster::GlyphRasterizer,
        crate::text::atlas::GlyphAtlas,
        crate::render::atlas::AtlasAllocator,
    ) {
        (
            crate::text::raster::GlyphRasterizer::new(),
            crate::text::atlas::GlyphAtlas::new(),
            crate::render::atlas::AtlasAllocator::with_page(
                crate::paint::atlas::AtlasKind::Glyph,
                (512, 512),
            ),
        )
    }

    fn line_glyphs(
        line: &crate::text::ShapedTextLine,
    ) -> Vec<crate::paint::primitive::PositionedGlyph> {
        line.items
            .iter()
            .filter_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => Some(run.glyphs.iter().copied()),
                ShapedTextItem::InlineBox(_) => None,
            })
            .flatten()
            .collect()
    }

    fn assert_ellipsis_prefix(
        rendered: &crate::text::ShapedTextLine,
        reference: &crate::text::ShapedTextLine,
        ellipsis: &super::EllipsisGlyphs,
        content_width: f32,
    ) -> usize {
        let glyphs = line_glyphs(rendered);
        let reference_glyphs = line_glyphs(reference);
        assert!(
            glyphs.len() >= ellipsis.glyphs.len(),
            "渲染序列必须包含省略号字形：{} vs {}",
            glyphs.len(),
            ellipsis.glyphs.len()
        );
        let prefix_len = glyphs.len() - ellipsis.glyphs.len();
        assert!(
            prefix_len <= reference_glyphs.len(),
            "保留前缀不得多于参考行：{prefix_len} vs {}",
            reference_glyphs.len()
        );
        assert_eq!(
            glyphs[..prefix_len],
            reference_glyphs[..prefix_len],
            "保留的必须是原行字形序列的**前缀**（逐位相同）：省略号之后残留任何字形都会失败"
        );
        let prefix_end = if prefix_len == reference_glyphs.len() {
            reference.advance
        } else {
            reference_glyphs[prefix_len].position.x
        };
        assert!(
            (rendered.advance - (prefix_end + ellipsis.advance)).abs() < 1e-2,
            "行推进必须 == 前缀终点 + 省略号推进：{} vs {} + {}",
            rendered.advance,
            prefix_end,
            ellipsis.advance
        );
        if content_width > 0.0 {
            assert!(
                rendered.advance <= content_width + 1e-2,
                "前缀 + 省略号必须放得进内容宽：{} vs {content_width}",
                rendered.advance
            );
        }
        prefix_len
    }

    #[test]
    fn shape_latin_single_line_has_positive_metrics() {
        let mut harness = Harness::new();
        let shaped = harness.shape("Hello", TextStyle::new(16.0));
        let size = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );

        assert!(size.width > 0.0, "width {}", size.width);
        assert!(size.height > 0.0, "height {}", size.height);
        assert_eq!(shaped.layout().lines().count(), 1);
        assert_eq!(shaped.layout().text_len(), 5);
    }

    #[test]
    fn shape_cjk_breaks_only_at_character_boundaries() {
        let mut harness = Harness::new();
        let shaped = harness.shape("世界世界世界世界", TextStyle::new(16.0));
        let size = shaped.measure(none_known(), definite(60.0));

        assert!(size.height > 30.0, "narrow CJK text must wrap: {size:?}");
        for line in shaped.layout().lines() {
            let range = line.text_range();
            assert!(
                "世界世界世界世界".is_char_boundary(range.start)
                    && "世界世界世界世界".is_char_boundary(range.end)
            );
        }
    }

    #[test]
    fn shape_mixed_scripts_produce_glyphs() {
        let mut harness = Harness::new();
        let shaped = harness.shape("A中🌍한", TextStyle::new(16.0));
        let glyphs: usize = shaped
            .layout()
            .lines()
            .flat_map(|line| line.items())
            .filter_map(|item| match item {
                parley::PositionedLayoutItem::GlyphRun(run) => {
                    Some(run.positioned_glyphs().count())
                }
                parley::PositionedLayoutItem::InlineBox(_) => None,
            })
            .sum();

        assert!(glyphs >= 1, "mixed text must shape at least one glyph");
    }

    #[test]
    fn rebreak_changes_line_count_without_a_new_layout() {
        let mut harness = Harness::new();
        let shaped = harness.shape("alpha beta gamma delta", TextStyle::new(16.0));
        let wide = shaped.measure(none_known(), definite(1000.0));
        let narrow = shaped.measure(none_known(), definite(60.0));

        assert!(narrow.height > wide.height, "{narrow:?} vs {wide:?}");
        assert_eq!(shaped.layout().text_len(), "alpha beta gamma delta".len());
    }

    #[test]
    fn align_center_moves_the_first_glyph_right() {
        let mut harness = Harness::new();
        let start = harness.shape("abc", TextStyle::new(16.0));
        let mut style = TextStyle::new(16.0);
        style.alignment = crate::text::TextAlign::Center;
        let centered = harness.shape("abc", style);

        assert!(
            centered.first_glyph_x_at(200.0) > start.first_glyph_x_at(200.0),
            "centering inside a 200px line box must shift the glyph right: {} vs {}",
            centered.first_glyph_x_at(200.0),
            start.first_glyph_x_at(200.0)
        );
    }

    fn rasterize_at(
        shaped: &super::ShapedText,
        width: f32,
        alignment: TextAlign,
        scale: f32,
    ) -> crate::text::ShapedLine {
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster: &mut raster,
            glyph_atlas: &mut glyphs,
            atlas: &mut atlas,
            generation,
            scale,
        };
        shaped
            .rasterize(
                width,
                alignment,
                crate::paint::color::Color::BLACK,
                &mut ctx,
                None,
            )
            .expect("rasterize")
            .0
    }

    fn rasterize_line_in_color(
        shaped: &super::ShapedText,
        width: f32,
        color: crate::paint::color::Color,
    ) -> crate::text::ShapedLine {
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster: &mut raster,
            glyph_atlas: &mut glyphs,
            atlas: &mut atlas,
            generation,
            scale: 1.0,
        };
        shaped
            .rasterize(width, shaped.style().alignment, color, &mut ctx, None)
            .expect("rasterize")
            .0
    }

    fn all_glyph_positions(line: &crate::text::ShapedLine) -> Vec<(f32, f32)> {
        line.glyph_runs()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| (glyph.position.x, glyph.position.y))
            .collect()
    }

    fn assert_positions_close(actual: &[(f32, f32)], expected: &[(f32, f32)], context: &str) {
        assert_eq!(actual.len(), expected.len(), "{context}: glyph count");
        for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert!(
                (a.0 - e.0).abs() <= 0.01 && (a.1 - e.1).abs() <= 0.01,
                "{context} glyph {index}: {a:?} vs {e:?}"
            );
        }
    }

    #[test]
    fn fast_path_raster_alignment_matches_parley_physical_positions() {
        let mut enabled = crate::text::context::TextContext::new();
        let style = TextStyle::new(16.0).with_wrap(TextWrap::NoWrap);
        let content = TextContent::new("1,234.56", style);
        let fast = enabled.shape_cached(&content).0;
        let stats = enabled.take_fast_path_stats();
        if stats.hits == 0 {
            return;
        }
        assert_eq!(stats.hits, 1, "test must exercise the fast path");

        let mut disabled = crate::text::context::TextContext::new();
        disabled.set_digit_fast_path(false);
        let parley = disabled.shape_cached(&content).0;

        for scale in [1.0f32, 1.25] {
            for alignment in [TextAlign::Right, TextAlign::Center] {
                let fast_line = rasterize_at(&fast, 200.0, alignment, scale);
                let parley_line = rasterize_at(&parley, 200.0, alignment, scale);
                assert!(
                    (fast_line.width - parley_line.width).abs() <= 0.01,
                    "{alignment:?} scale={scale}: fast width {} vs parley {}",
                    fast_line.width,
                    parley_line.width
                );
                let fast_positions = all_glyph_positions(&fast_line);
                let parley_positions = all_glyph_positions(&parley_line);
                assert_eq!(
                    fast_positions.len(),
                    parley_positions.len(),
                    "{alignment:?} scale={scale}: glyph count"
                );
                for (index, (fast_pos, parley_pos)) in
                    fast_positions.iter().zip(&parley_positions).enumerate()
                {
                    assert!(
                        (fast_pos.0 - parley_pos.0).abs() <= 0.01,
                        "{alignment:?} scale={scale} glyph {index}: fast x {} vs parley {}",
                        fast_pos.0,
                        parley_pos.0
                    );
                    assert!(
                        (fast_pos.0 * scale - (fast_pos.0 * scale).round()).abs() <= 1e-4,
                        "{alignment:?} scale={scale} glyph {index}: physical x {} is not an integer",
                        fast_pos.0 * scale
                    );
                }
            }
        }
    }

    #[test]
    fn fast_path_alignment_after_ellipsis_uses_the_full_advance() {
        let mut text_cx = crate::text::context::TextContext::new();
        let style = TextStyle::new(16.0)
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(crate::text::TextOverflow::Ellipsis, 1);
        let content = TextContent::new("1234567890", style.clone());
        let shaped = text_cx.shape_cached(&content).0;
        if text_cx.take_fast_path_stats().hits == 0 {
            return;
        }
        let ellipsis = text_cx
            .ellipsis_for(&style)
            .expect("host font must provide an ellipsis glyph");
        let width = 41.84375f32;
        let left = {
            let (mut raster, mut glyphs, mut atlas) = glyph_runner();
            let generation = atlas.generation();
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: 1.0,
            };
            shaped
                .rasterize(
                    width,
                    TextAlign::Left,
                    crate::paint::color::Color::BLACK,
                    &mut ctx,
                    Some(ellipsis.as_ref()),
                )
                .expect("left raster")
                .0
        };
        let right = {
            let (mut raster, mut glyphs, mut atlas) = glyph_runner();
            let generation = atlas.generation();
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: 1.0,
            };
            shaped
                .rasterize(
                    width,
                    TextAlign::Right,
                    crate::paint::color::Color::BLACK,
                    &mut ctx,
                    Some(ellipsis.as_ref()),
                )
                .expect("right raster")
                .0
        };
        let mut parley_cx = crate::text::context::TextContext::new();
        parley_cx.set_digit_fast_path(false);
        let parley_shaped = parley_cx.shape_cached(&content).0;
        let parley_ellipsis = parley_cx
            .ellipsis_for(&style)
            .expect("host font must provide an ellipsis glyph");
        let parley_right = {
            let (mut raster, mut glyphs, mut atlas) = glyph_runner();
            let generation = atlas.generation();
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: 1.0,
            };
            parley_shaped
                .rasterize(
                    width,
                    TextAlign::Right,
                    crate::paint::color::Color::BLACK,
                    &mut ctx,
                    Some(parley_ellipsis.as_ref()),
                )
                .expect("parley right raster")
                .0
        };
        let full_width = shaped.max_content_size().width;
        assert!(
            left.width < full_width,
            "ellipsis 必须截断：{} vs full {full_width}",
            left.width
        );
        assert!(
            (left.width - parley_right.width).abs() <= 0.01,
            "fast/parley ellipsis 宽度必须一致：{} vs {}",
            left.width,
            parley_right.width
        );
        let left_positions = all_glyph_positions(&left);
        let right_positions = all_glyph_positions(&right);
        let parley_positions = all_glyph_positions(&parley_right);
        assert_positions_close(
            &right_positions,
            &left_positions,
            "ellipsis 溢出时 Right 与 Left",
        );
        assert_positions_close(
            &left_positions,
            &parley_positions,
            "fast/parley ellipsis raster",
        );
    }

    #[test]
    fn one_snapshot_serves_multiple_alignments() {
        let mut harness = Harness::new();
        let shaped = harness.shape("abc", TextStyle::new(16.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut render = |alignment: crate::text::TextAlign| {
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: 1.0,
            };
            shaped
                .rasterize(
                    100.0,
                    alignment,
                    crate::paint::color::Color::BLACK,
                    &mut ctx,
                    None,
                )
                .expect("rasterize")
                .0
        };
        let left = render(crate::text::TextAlign::Left);
        let center = render(crate::text::TextAlign::Center);
        let left_x = all_glyphs(&left)[0].position.x;
        let center_x = all_glyphs(&center)[0].position.x;

        assert!(
            center_x > left_x,
            "同一快照在不同对齐下必须得到不同落点：{left_x} → {center_x}"
        );
    }

    #[test]
    fn content_widths_match_forced_breaks() {
        let mut harness = Harness::new();
        let shaped = harness.shape("The quick brown fox", TextStyle::new(16.0));
        let widths = shaped.layout().calculate_content_widths();
        let broken = shaped.measure(none_known(), definite(1.0));
        let single = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );

        assert!(
            (broken.width - widths.min).abs() < 0.01,
            "{broken:?} vs {widths:?}"
        );
        assert!(
            (single.width - widths.max).abs() < 0.01,
            "{single:?} vs {widths:?}"
        );
    }

    #[test]
    fn empty_text_measures_zero() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(16.0));

        assert_eq!(
            shaped.measure(none_known(), definite(100.0)),
            crate::math::Size::ZERO
        );
        assert_eq!(
            shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                }
            ),
            crate::math::Size::ZERO
        );
    }

    #[test]
    fn measure_definite_width_wraps_and_grows_height() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three four five", TextStyle::new(16.0));
        let wide = shaped.measure(none_known(), definite(400.0));
        let narrow = shaped.measure(none_known(), definite(40.0));

        assert!(narrow.height > wide.height);
        assert!(
            narrow.width <= 40.5,
            "wrapped width {} exceeds the constraint",
            narrow.width
        );
    }

    #[test]
    fn measure_min_content_is_the_longest_word() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three", TextStyle::new(16.0));
        let min = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );

        assert!((min.width - shaped.layout().calculate_content_widths().min).abs() < 0.01);
    }

    #[test]
    fn measure_max_content_is_one_line() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three", TextStyle::new(16.0));
        let single = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        let line_height = shaped
            .layout()
            .lines()
            .next()
            .map(|line| line.metrics().line_height)
            .unwrap_or(0.0);

        assert!(
            (single.height - line_height).abs() < 0.5,
            "{single:?} vs {line_height}"
        );
    }

    #[test]
    fn measure_is_deterministic() {
        let mut harness = Harness::new();
        let shaped = harness.shape("determinism 一致性", TextStyle::new(15.0));
        let a = shaped.measure(none_known(), definite(120.0));
        let b = shaped.measure(none_known(), definite(120.0));

        assert_eq!(a.width.to_bits(), b.width.to_bits());
        assert_eq!(a.height.to_bits(), b.height.to_bits());
    }

    #[test]
    fn wrap_change_keeps_max_width_but_changes_height() {
        let mut harness = Harness::new();
        let wrap = harness.shape("aaaa aaaa aaaa", TextStyle::new(16.0));
        let anywhere = harness.shape(
            "aaaa aaaa aaaa",
            TextStyle::new(16.0).with_wrap(TextWrap::Anywhere),
        );
        let max = |shaped: &super::ShapedText| shaped.layout().calculate_content_widths().max;

        assert!(
            (max(&wrap) - max(&anywhere)).abs() < 0.5,
            "precondition: max-content widths must match ({} vs {})",
            max(&wrap),
            max(&anywhere)
        );
        assert!(
            anywhere.measure(none_known(), definite(30.0)).height
                > wrap.measure(none_known(), definite(30.0)).height,
            "BreakAll must produce more lines at the same width"
        );
    }

    #[test]
    fn approx_bytes_counts_inline_boxes() {
        let mut harness = Harness::new();
        let plain = harness.shape("abcd", TextStyle::new(16.0));
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::Size::new(20.0, 18.0),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let boxes: Vec<InlineBoxSpec> = (0..8)
            .map(|index| InlineBoxSpec { index, ..spec })
            .collect();
        let with_boxes = harness.shape_with("abcd", TextStyle::new(16.0), &boxes);
        let extra = with_boxes.approx_bytes() - plain.approx_bytes();
        assert!(
            extra >= boxes.len() * core::mem::size_of::<InlineBoxSpec>(),
            "行内盒必须计入预算（差 {extra}，至少 {}）",
            boxes.len() * core::mem::size_of::<InlineBoxSpec>()
        );
    }

    #[test]
    fn inline_box_inserts_an_item_between_runs() {
        let mut harness = Harness::new();
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::Size::new(20.0, 18.0),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let shaped = harness.shape_with("abcd", TextStyle::new(16.0), &[spec]);

        let boxes = shaped
            .layout()
            .lines()
            .flat_map(|line| line.items())
            .filter(|item| matches!(item, parley::PositionedLayoutItem::InlineBox(_)))
            .count();
        assert_eq!(boxes, 1, "one inline box must be placed");
    }

    #[test]
    fn runs_split_glyph_runs_and_keep_per_run_colors() {
        use crate::paint::color::Color;

        let red = Color::Srgb {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        };
        let blue = Color::Srgb {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            alpha: 1.0,
        };
        let mut harness = Harness::new();
        let content = TextContent::new("abcdef", TextStyle::new(16.0)).with_runs([
            TextRun::new(0..3, TextStyle::new(16.0).with_color(red)),
            TextRun::new(3..6, TextStyle::new(16.0).with_color(blue)),
        ]);
        let shaped = harness.shape_content(&content);
        assert_eq!(shaped.runs().len(), 2, "快照必须保存 runs（缓存命中判据）");

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let runs: Vec<_> = line.glyph_runs().collect();
        assert_eq!(runs.len(), 2, "两个 TextRun 必须切成两个字形 run");
        assert_eq!(runs[0].text_range, 0..3);
        assert_eq!(runs[1].text_range, 3..6);
        assert_eq!(
            runs[0].color, red,
            "第一个 run 的文本色来自 run.style.color"
        );
        assert_eq!(runs[1].color, blue);
        assert!(!runs[0].glyphs.is_empty() && !runs[1].glyphs.is_empty());
    }

    #[test]
    fn base_color_recolor_keeps_explicit_run_colors() {
        use crate::paint::color::Color;

        let red = Color::Srgb {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        };
        let green = Color::Srgb {
            r: 0.0,
            g: 1.0,
            b: 0.0,
            alpha: 1.0,
        };
        let blue = Color::Srgb {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            alpha: 1.0,
        };
        let mut harness = Harness::new();
        let content = TextContent::new("abcdef", TextStyle::new(16.0).with_color(green))
            .with_runs([TextRun::new(0..3, TextStyle::new(16.0).with_color(red))]);
        let shaped = harness.shape_content(&content);
        let width = shaped.max_content_size().width;

        let blue_line = rasterize_line_in_color(&shaped, width, blue);
        let blue_runs: Vec<_> = blue_line.glyph_runs().collect();
        assert_eq!(blue_runs.len(), 2, "两个 style run：{:?}", blue_line);
        assert_eq!(blue_runs[0].text_range, 0..3);
        assert_eq!(blue_runs[1].text_range, 3..6);
        assert_eq!(
            blue_runs[0].color, red,
            "TextRun 的颜色是具体值，换基础色不得覆盖它"
        );
        assert_eq!(
            blue_runs[1].color, blue,
            "未被 run 覆盖的区间必须用本次基础色（缓存命中仍画首次颜色的回归点）"
        );

        let green_line = rasterize_line_in_color(&shaped, width, green);
        let green_runs: Vec<_> = green_line.glyph_runs().collect();
        assert_eq!(green_runs[0].color, red);
        assert_eq!(green_runs[1].color, green);
        assert_eq!(
            all_glyph_positions(&blue_line),
            all_glyph_positions(&green_line),
            "同一份布局快照只换颜色，字形落点必须逐位相同"
        );
    }

    #[test]
    fn decoration_geometry_follows_baseline_and_run_span() {
        use crate::paint::color::Color;

        let ink = Color::Srgb {
            r: 0.1,
            g: 0.2,
            b: 0.3,
            alpha: 1.0,
        };
        let explicit = Color::Srgb {
            r: 0.9,
            g: 0.9,
            b: 0.0,
            alpha: 1.0,
        };
        let mut harness = Harness::new();
        let content = TextContent::new("abcdef", TextStyle::new(16.0).with_color(ink)).with_runs([
            TextRun::new(
                0..3,
                TextStyle::new(16.0)
                    .with_color(ink)
                    .with_decoration(TextDecoration::underline(1.0)),
            ),
            TextRun::new(
                3..6,
                TextStyle::new(16.0)
                    .with_color(ink)
                    .with_decoration(TextDecoration::strikethrough(2.0).with_color(Some(explicit))),
            ),
        ]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.decorations.len(),
            2,
            "每个有装饰的 run 一条装饰几何：{:?}",
            shaped_line.decorations
        );
        let underline = shaped_line.decorations[0];
        let strike = shaped_line.decorations[1];
        assert!(
            underline.rect.origin.y > shaped_line.baseline,
            "下划线在基线下方：{} vs baseline {}",
            underline.rect.origin.y,
            shaped_line.baseline
        );
        assert!(
            strike.rect.origin.y < shaped_line.baseline,
            "删除线在基线上方：{} vs baseline {}",
            strike.rect.origin.y,
            shaped_line.baseline
        );
        assert!((underline.rect.size.height - 1.0).abs() < 1e-4);
        assert!((strike.rect.size.height - 2.0).abs() < 1e-4);
        assert_eq!(underline.color, ink, "默认线色跟随 run 文本色");
        assert_eq!(strike.color, explicit, "显式线色优先");
        assert!(underline.rect.size.width > 0.0 && strike.rect.size.width > 0.0);
        assert!(
            underline.rect.origin.x < strike.rect.origin.x,
            "LTR 下前一个 run 的装饰在左：{:?}",
            shaped_line.decorations
        );
        let plain = harness.shape("abcdef", TextStyle::new(16.0));
        let (plain_line, _, _) = rasterize_line(
            &plain,
            plain.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(plain_line.lines[0].decorations.is_empty());
    }

    #[test]
    fn base_style_decoration_covers_uncovered_text_only() {
        let underline = TextDecoration::underline(1.0);
        let strike = TextDecoration::strikethrough(2.0);
        let mut harness = Harness::new();
        let content = TextContent::new("abcdef", TextStyle::new(16.0).with_decoration(underline))
            .with_runs([TextRun::new(
                0..3,
                TextStyle::new(16.0).with_decoration(strike),
            )]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let decorations = &line.lines[0].decorations;
        assert_eq!(
            decorations.len(),
            2,
            "删除线 run + 兜底下划线区间：{decorations:?}"
        );
        let base = decorations[0];
        let run = decorations[1];
        assert!(
            base.rect.origin.y > line.lines[0].baseline,
            "兜底装饰 = 下划线（基线下方）：{base:?}"
        );
        assert!(
            run.rect.origin.y < line.lines[0].baseline,
            "run 装饰 = 删除线（基线上方）：{run:?}"
        );
        assert!(
            base.rect.origin.x >= run.rect.origin.x + run.rect.size.width - 1e-3,
            "兜底下划线不得覆盖已由 run 装饰的区间：{decorations:?}"
        );
    }

    #[test]
    fn cjk_and_emoji_mixed_runs_keep_boundaries() {
        let mut harness = Harness::new();
        let content = TextContent::new("价格🌍ab", TextStyle::new(16.0)).with_runs([
            TextRun::new(
                0..6,
                TextStyle::new(16.0).with_decoration(TextDecoration::underline(1.0)),
            ),
            TextRun::new(6..10, TextStyle::new(18.0)),
            TextRun::new(10..12, TextStyle::new(16.0).with_weight(700)),
        ]);
        let shaped = harness.shape_content(&content);
        assert_eq!(shaped.runs().len(), 3);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(line.lines.len(), 1, "不限宽 ⇒ 单行");
        assert!(
            line.glyph_runs().count() >= 2,
            "CJK/emoji/ASCII 至少两个 run"
        );
        assert_eq!(line.lines[0].decorations.len(), 1, "只有第一个 run 有装饰");
        assert!(line.lines[0].decorations[0].rect.size.width > 0.0);
        for run in line.glyph_runs() {
            assert!(
                "价格🌍ab".is_char_boundary(run.text_range.start)
                    && "价格🌍ab".is_char_boundary(run.text_range.end),
                "字形 run 边界必须落在 char boundary：{:?}",
                run.text_range
            );
        }
    }

    #[test]
    fn decoration_across_wrap_creates_one_segment_per_line() {
        let mut harness = Harness::new();
        let content = TextContent::new(
            "alpha beta gamma",
            TextStyle::new(16.0).with_decoration(TextDecoration::underline(1.0)),
        );
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let width = 40.0;
        let (line, _diagnostics, _stats) =
            rasterize_line(&shaped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        assert!(
            line.lines.len() > 1,
            "40 px 内容宽必须换行：{:?}",
            line.lines
        );
        assert_eq!(
            line.lines
                .iter()
                .map(|line| line.decorations.len())
                .sum::<usize>(),
            line.lines.len(),
            "每行一条装饰（兜底样式覆盖整段）：{:?}",
            line.lines
        );
        for shaped_line in &line.lines {
            for decoration in &shaped_line.decorations {
                assert!(
                    decoration.rect.origin.x >= -1e-3
                        && decoration.rect.origin.x + decoration.rect.size.width
                            <= shaped_line.advance + 1e-3,
                    "装饰不得超出该行推进：{decoration:?} / advance {}",
                    shaped_line.advance
                );
            }
        }
    }

    #[test]
    fn decoration_survives_hard_line_breaks() {
        let underline = TextDecoration::underline(1.0);
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        for (text, expected) in [
            ("ab\ncd", vec![1usize, 1]),
            ("ab\r\ncd", vec![1, 1]),
            ("ab\n\ncd", vec![1, 0, 1]),
            ("ab\n", vec![1, 0]),
        ] {
            let content = TextContent::new(text, TextStyle::new(16.0).with_decoration(underline));
            let shaped = harness.shape_content(&content);
            let (line, _diagnostics, _stats) = rasterize_line(
                &shaped,
                shaped.max_content_size().width,
                1.0,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            let counts: Vec<usize> = line
                .lines
                .iter()
                .map(|shaped_line| shaped_line.decorations.len())
                .collect();
            assert_eq!(
                counts, expected,
                "硬换行行尾不属于装饰范围（也不得整行丢弃）：{text:?} -> {line:?}"
            );
            for shaped_line in &line.lines {
                for decoration in &shaped_line.decorations {
                    assert!(
                        decoration.rect.size.width > 0.0,
                        "装饰必须有正宽度：{decoration:?}"
                    );
                    assert!(
                        decoration.rect.origin.x >= -1e-3
                            && decoration.rect.origin.x + decoration.rect.size.width
                                <= shaped_line.advance + 1e-3,
                        "装饰不得超出该行内容推进：{decoration:?} / advance {}",
                        shaped_line.advance
                    );
                }
            }
        }
    }

    #[test]
    fn decoration_range_excludes_the_hard_break_tail() {
        assert_eq!(super::line_content_end("ab\ncd", &(0..3)), 2);
        assert_eq!(super::line_content_end("ab\r\ncd", &(0..4)), 2);
        assert_eq!(
            super::line_content_end("ab\n\ncd", &(3..4)),
            3,
            "空行没有内容"
        );
        assert_eq!(super::line_content_end("\n", &(0..1)), 0);
        assert_eq!(
            super::line_content_end("ab", &(0..2)),
            2,
            "末行无换行时原样保留"
        );
        assert_eq!(
            super::line_content_end("ab\t", &(0..3)),
            3,
            "非换行字符不裁剪"
        );
    }

    #[test]
    fn run_decoration_clipped_at_hard_line_break() {
        let underline = TextDecoration::underline(1.0);
        let mut harness = Harness::new();
        let content = TextContent::new("ab\ncd", TextStyle::new(16.0)).with_runs([TextRun::new(
            0..5,
            TextStyle::new(16.0).with_decoration(underline),
        )]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let counts: Vec<usize> = line
            .lines
            .iter()
            .map(|shaped_line| shaped_line.decorations.len())
            .collect();
        assert_eq!(
            counts,
            vec![1, 1],
            "跨硬换行的 run 装饰必须逐行生成：{line:?}"
        );
        let second = &line.lines[1];
        let decoration = second.decorations[0];
        assert!(
            decoration.rect.origin.x.abs() < 1e-2
                && (decoration.rect.size.width - second.advance).abs() < 1e-2,
            "第二行装饰跨度 = 内容区间（不含行尾换行）：{decoration:?} / advance {}",
            second.advance
        );
    }

    #[test]
    fn rtl_decoration_covers_the_full_visual_span() {
        let underline = TextDecoration::underline(1.0);
        let rtl = TextStyle::new(16.0).with_direction(TextDirection::Rtl);
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let content = TextContent::new("שלום עולם", rtl.clone().with_decoration(underline));
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(line.lines.len(), 1, "不限宽 ⇒ 单行");
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.decorations.len(),
            1,
            "整段兜底装饰一条：{:?}",
            shaped_line.decorations
        );
        let decoration = shaped_line.decorations[0];
        let right = decoration.rect.origin.x + decoration.rect.size.width;
        assert!(
            decoration.rect.origin.x <= 1.0,
            "RTL 装饰必须从行左边缘开始（旧实现从中间开始）：{decoration:?}"
        );
        assert!(
            (right - shaped_line.advance).abs() <= 1.0,
            "RTL 装饰必须覆盖到行推进末端：{decoration:?} / advance {}",
            shaped_line.advance
        );

        let content = TextContent::new("שלום עולם", rtl.clone()).with_runs([
            TextRun::new(0..4, rtl.clone().with_decoration(underline)),
            TextRun::new(4..8, rtl.with_decoration(underline)),
        ]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.decorations.len(),
            2,
            "两个 RTL run 各一条装饰：{:?}",
            shaped_line.decorations
        );
        let left = shaped_line
            .decorations
            .iter()
            .map(|decoration| decoration.rect.origin.x)
            .fold(f32::INFINITY, f32::min);
        let right = shaped_line
            .decorations
            .iter()
            .map(|decoration| decoration.rect.origin.x + decoration.rect.size.width)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            shaped_line
                .decorations
                .iter()
                .all(|decoration| decoration.rect.size.width > 10.0),
            "每个 RTL run 的装饰必须覆盖整段视觉宽度（旧实现只剩中间）：{:?}",
            shaped_line.decorations
        );
        assert!(
            left <= 40.0,
            "跨 run 的 RTL 装饰必须覆盖到词左端：{shaped_line:?}"
        );
        assert!(
            (right - shaped_line.advance).abs() <= 1.0,
            "跨 run 的 RTL 装饰必须覆盖到行推进末端：{shaped_line:?}"
        );

        let content =
            TextContent::new("abc שלום def", TextStyle::new(16.0)).with_runs([TextRun::new(
                4..12,
                TextStyle::new(16.0).with_decoration(underline),
            )]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(shaped_line.decorations.len(), 1, "只有 RTL run 有装饰");
        let decoration = shaped_line.decorations[0];
        let (run_left, run_right) = shaped_line
            .items
            .iter()
            .find_map(|item| match item {
                ShapedTextItem::GlyphRun(run) if run.text_range == (4..12) => {
                    let left = run
                        .glyphs
                        .iter()
                        .map(|glyph| glyph.position.x)
                        .fold(f32::INFINITY, f32::min);
                    let right = run
                        .glyphs
                        .iter()
                        .map(|glyph| glyph.position.x + glyph.size.width)
                        .fold(f32::NEG_INFINITY, f32::max);
                    Some((left, right))
                }
                ShapedTextItem::GlyphRun(_) | ShapedTextItem::InlineBox(_) => None,
            })
            .expect("RTL run 必须存在");
        assert!(
            decoration.rect.origin.x <= run_left + 1.5,
            "LTR 段落里的 RTL run：装饰左端不得从 run 内部开始：{decoration:?} vs run {run_left}..{run_right}"
        );
        assert!(
            decoration.rect.origin.x + decoration.rect.size.width >= run_right - 1.5,
            "LTR 段落里的 RTL run：装饰右端不得止于 run 内部：{decoration:?} vs run {run_left}..{run_right}"
        );
    }

    #[test]
    fn rasterize_of_empty_text_yields_empty_snapshot() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(16.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let (line, diagnostics, _stats) =
            rasterize_line(&shaped, 0.0, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert!(line.lines.is_empty());
        assert_eq!(line.width, 0.0);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn rasterize_positions_glyphs_and_sets_tiles() {
        let mut harness = Harness::new();
        let shaped = harness.shape("Ab", TextStyle::new(16.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let (line, _diagnostics, stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert_eq!(line.lines.len(), 1);
        assert_eq!(line.atlas_generation, atlas.generation());
        let run = match &line.lines[0].items[0] {
            ShapedTextItem::GlyphRun(run) => run,
            ShapedTextItem::InlineBox(_) => panic!("first item must be a glyph run"),
        };
        assert!(!run.glyphs.is_empty(), "both glyphs have coverage");
        for glyph in &run.glyphs {
            assert!(!glyph.tile.is_stale(atlas.generation()));
            assert!(glyph.size.width > 0.0 && glyph.size.height > 0.0);
            let physical = glyph.size.width * line.scale;
            assert!(
                (physical - physical.round()).abs() < 1e-3,
                "scale * size must be integral, got {physical}"
            );
            let physical_x = glyph.position.x * line.scale;
            assert!(
                (physical_x - physical_x.round()).abs() < 1e-3,
                "亚像素相位必须烘进位图，落点仍是整数物理像素（否则会被 to_physical 二次取整）：{physical_x}"
            );
        }
        assert_eq!(stats.rasters, run.glyphs.len());
    }

    #[test]
    fn subpixel_phase_reaches_the_glyph_atlas() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let centered = TextStyle::new(16.0).with_alignment(crate::text::TextAlign::Center);
        let shaped = harness.shape("R", centered);

        let (left_width, right_width) = (200.0f32, 200.8f32);
        let left_x = shaped.first_glyph_x_at(left_width);
        let right_x = shaped.first_glyph_x_at(right_width);
        assert!(
            (right_x - left_x - 0.4).abs() < 0.01,
            "precondition: 居中偏移必须线性移动 0.4 px（{left_x} → {right_x}）"
        );
        assert_ne!(
            crate::text::raster::split_subpixel_x(left_x).1,
            crate::text::raster::split_subpixel_x(right_x).1,
            "precondition: 0.4 px 的位移必须换到不同相位桶"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (left, _, _) = rasterize_line(
            &shaped,
            left_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (right, _, _) = rasterize_line(
            &shaped,
            right_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert_ne!(
            first_glyph_tile(&left).uv(),
            first_glyph_tile(&right).uv(),
            "0.4 px 的亚像素位移必须产生不同 tile（相位必须进入 GlyphKey）"
        );
    }

    #[test]
    fn subpixel_phase_at_fractional_scale_follows_physical_positions() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let shaped = harness.shape(
            "R",
            TextStyle::new(16.0).with_alignment(crate::text::TextAlign::Center),
        );

        let (left_width, right_width) = (200.0f32, 200.64f32);
        let left_logical = shaped.first_glyph_x_at(left_width);
        let right_logical = shaped.first_glyph_x_at(right_width);
        assert!(
            (right_logical - left_logical - 0.32).abs() < 0.01,
            "precondition: 逻辑落点移动 0.32 px（{left_logical} → {right_logical}）"
        );
        assert!(
            (right_logical - left_logical) * 1.25 - 0.4 < 0.01,
            "precondition: 物理落点移动 0.4 px"
        );
        assert_ne!(
            crate::text::raster::split_subpixel_x(left_logical * 1.25).1,
            crate::text::raster::split_subpixel_x(right_logical * 1.25).1,
            "precondition: 0.4 物理像素必须换到不同相位桶"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (left, _, _) = rasterize_line(
            &shaped,
            left_width,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (right, _, _) = rasterize_line(
            &shaped,
            right_width,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert_ne!(
            first_glyph_tile(&left).uv(),
            first_glyph_tile(&right).uv(),
            "物理相位不同 ⇒ 1.25 scale 下必须产生不同 tile"
        );
        for line in [&left, &right] {
            let glyphs = all_glyphs(line);
            assert_eq!(glyphs.len(), 1, "'R' must have coverage");
            for glyph in glyphs {
                let physical_x = glyph.position.x * line.scale;
                assert!(
                    (physical_x - physical_x.round()).abs() < 1e-3,
                    "落点必须是整数物理像素（相位已烘进位图）：{physical_x}"
                );
                let physical_width = glyph.size.width * line.scale;
                assert!(
                    (physical_width - physical_width.round()).abs() < 1e-3,
                    "位图物理宽度必须是整数：{physical_width}"
                );
            }
        }
    }

    #[test]
    fn subpixel_phase_is_derived_from_physical_coordinates() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let centered =
            |size: f32| TextStyle::new(size).with_alignment(crate::text::TextAlign::Center);
        let fractional = harness.shape("R", centered(16.0));
        let integer = harness.shape("R", centered(20.0));
        let fractional_logical = fractional.first_glyph_x_at(200.0);
        let integer_logical = integer.first_glyph_x_at(250.0);
        assert!(
            (fractional_logical * 1.25 - integer_logical).abs() < 1e-2,
            "precondition: 物理落点必须一致（{} → {integer_logical}）",
            fractional_logical * 1.25
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (fractional_line, _, fractional_stats) = rasterize_line(
            &fractional,
            200.0,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (integer_line, _, integer_stats) =
            rasterize_line(&integer, 250.0, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert_eq!(
            fractional_stats.rasters, 1,
            "the single 'R' must raster once"
        );
        assert_eq!(
            (
                integer_stats.misses,
                integer_stats.rasters,
                integer_stats.hits
            ),
            (0, 0, 1),
            "物理布局相同 ⇒ 第二次必须命中第一次的 tile（逻辑坐标/逻辑字号实现会 miss）"
        );
        assert_eq!(
            first_glyph_tile(&fractional_line),
            first_glyph_tile(&integer_line),
            "同一物理字形必须复用同一 tile"
        );
    }

    #[test]
    fn fractional_ppem_keeps_parley_advances() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let shaped = harness.shape("0123456789", TextStyle::new(14.0));
        let layout_width = {
            let layout = shaped.layout();
            layout.width()
        };
        let layout_advance = shaped
            .layout()
            .lines()
            .next()
            .expect("single line")
            .metrics()
            .advance;

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, diagnostics, _) = rasterize_line(
            &shaped,
            layout_width,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert!(
            diagnostics.is_empty(),
            "digits must not produce diagnostics"
        );
        assert!(!all_glyphs(&line).is_empty(), "digits must rasterize");
        assert_eq!(
            line.width, layout_width,
            "内容宽 = 逻辑布局宽（不再除 scale）"
        );
        assert_eq!(
            line.lines[0].advance, layout_advance,
            "行 advance 必须来自 parley，不受小数 ppem 影响"
        );
        let total_advance: f32 = line.lines.iter().map(|line| line.advance).sum();
        assert!(
            (total_advance - line.width).abs() < 1e-3,
            "advance 总和必须等于内容宽：{total_advance} vs {}",
            line.width
        );
    }

    #[test]
    fn fractional_scale_rasterizes_larger_bitmaps_with_distinct_tiles() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let shaped = harness.shape("HHHH", TextStyle::new(16.0));
        let width = shaped.max_content_size().width;

        let mut raster = crate::text::raster::GlyphRasterizer::new();
        let mut glyphs = crate::text::atlas::GlyphAtlas::new();
        let mut atlas = crate::render::atlas::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (2048, 2048),
        );
        let (line_1x, _, _) =
            rasterize_line(&shaped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (line_125, _, _) =
            rasterize_line(&shaped, width, 1.25, &mut raster, &mut glyphs, &mut atlas);

        let glyphs_1x = all_glyphs(&line_1x);
        let glyphs_125 = all_glyphs(&line_125);
        assert_eq!(glyphs_1x.len(), 4, "HHHH must produce four covered glyphs");
        assert_eq!(glyphs_125.len(), 4);
        assert_ne!(
            glyphs_1x[0].tile.uv(),
            glyphs_125[0].tile.uv(),
            "不同 ppem（16 vs 20）必须占不同 tile"
        );

        let physical_width_sum =
            |glyphs: &[crate::paint::primitive::PositionedGlyph], scale: f32| -> f32 {
                glyphs.iter().map(|glyph| glyph.size.width * scale).sum()
            };
        let width_1x = physical_width_sum(&glyphs_1x, 1.0);
        let width_125 = physical_width_sum(&glyphs_125, 1.25);
        let ratio = width_125 / width_1x;
        assert!(
            (ratio - 1.25).abs() < 0.12,
            "125% 位图宽度应接近 1.25×：{width_1x} → {width_125}（ratio {ratio}）"
        );
    }

    #[test]
    fn subpixel_positioning_does_not_change_advances() {
        let mut harness = Harness::new();
        let shaped = harness.shape("0123456789", TextStyle::new(16.0));
        let (layout_width, layout_advance) = {
            let layout = shaped.layout();
            let advance = layout
                .lines()
                .next()
                .expect("single line")
                .metrics()
                .advance;
            (layout.width(), advance)
        };

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, diagnostics, _) = rasterize_line(
            &shaped,
            layout_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(
            diagnostics.is_empty(),
            "digits must not produce diagnostics"
        );

        assert_eq!(line.width, layout_width, "内容宽必须等于 parley 布局宽");
        assert_eq!(line.lines.len(), 1);
        assert_eq!(line.lines[0].advance, layout_advance);
        let total_advance: f32 = line.lines.iter().map(|line| line.advance).sum();
        assert!(
            (total_advance - layout_width).abs() < 1e-3,
            "亚像素定位不得改变 advance 总和：{total_advance} vs {layout_width}"
        );
        assert!(
            (shaped.max_content_size().width - layout_width).abs() < 1e-3,
            "形状快照的内容宽也必须与布局宽一致"
        );
    }

    #[test]
    fn measure_is_o1_for_min_max_content() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three four five", TextStyle::new(16.0));
        for _ in 0..32 {
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MinContent,
                    height: AvailableSpace::MinContent,
                },
            );
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                },
            );
        }
        assert_eq!(
            shaped.measure_breaks(),
            0,
            "Min/MaxContent 不得触发任何断行（O(1) 路径）"
        );

        let width = shaped.max_content_size().width * 0.5;
        for _ in 0..16 {
            let _ = shaped.measure(none_known(), definite(width));
        }
        assert_eq!(
            shaped.measure_breaks(),
            1,
            "同一宽度的重复探测必须命中 memo"
        );
        let _ = shaped.measure(none_known(), definite(width * 0.5));
        assert_eq!(shaped.measure_breaks(), 2, "换宽度才允许再次断行");
    }

    #[test]
    fn missing_glyph_draws_notdef_instead_of_vanishing() {
        const RARE: &str = "\u{10FFFD}";
        let mut harness = Harness::new();
        let shaped = harness.shape(RARE, TextStyle::new(16.0));
        let width = shaped.max_content_size().width.max(32.0);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, diagnostics, stats) =
            rasterize_line(&shaped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);

        let missing = diagnostics
            .iter()
            .filter(|diagnostic| matches!(diagnostic, TextDiagnostic::MissingGlyph { .. }))
            .count();
        if missing == 0 {
            eprintln!("skip: host font stack covers U+10FFFD");
            return;
        }
        assert!(missing >= 1, "缺字形必须有 MissingGlyph 诊断");
        assert!(
            stats.misses > 0,
            "缺字形必须走图集（.notdef 光栅）路径，实际统计 {stats:?}"
        );

        let rendered = all_glyphs(&line);
        if rendered.is_empty() {
            eprintln!("skip: host font has no .notdef outline");
            return;
        }
        assert_eq!(
            rendered.len(),
            1,
            "单字符文本必须产出 1 个 .notdef 占位字形（旧实现为 0）：{rendered:?}"
        );
        let placeholder = rendered[0];
        assert!(
            placeholder.size.width > 0.0 && placeholder.size.height > 0.0,
            "`.notdef` 占位必须有正尺寸：{:?}",
            placeholder.size
        );
    }

    #[test]
    fn ellipsis_clips_the_line_and_appends_glyphs() {
        let mut harness = Harness::new();
        let style = TextStyle::new(16.0)
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(crate::text::TextOverflow::Ellipsis, 0);
        let shaped = harness.shape("abcdefghijklmnopqrstuvwxyz", style);
        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");
        let full_width = shaped.max_content_size().width;
        let width = full_width * 0.5;

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster: &mut raster,
            glyph_atlas: &mut glyphs,
            atlas: &mut atlas,
            generation,
            scale: 1.0,
        };
        let (line, _diagnostics, _stats) = shaped
            .rasterize(
                width,
                crate::text::TextAlign::Start,
                crate::paint::color::Color::BLACK,
                &mut ctx,
                Some(&ellipsis),
            )
            .expect("ellipsis clipping must not fail");

        assert_eq!(line.lines.len(), 1, "NoWrap 恒为一行");
        assert!(
            line.width <= width + 0.01,
            "截断后宽度 {} 必须在 {} 之内",
            line.width,
            width
        );
        let rendered = all_glyphs(&line);
        assert!(!rendered.is_empty(), "截断后仍有字形");
        let tail_glyph = rendered.last().expect("tail glyph").tile;
        let expected = ellipsis
            .glyphs
            .last()
            .map(|(glyph, _)| *glyph)
            .expect("ellipsis glyph id");
        let _ = (tail_glyph, expected);
        assert!(
            rendered.len() < 26,
            "截断后字形数必须少于全文：{}",
            rendered.len()
        );
    }

    #[test]
    fn max_lines_truncates_lines() {
        const TEXT: &str = "one two three four five six seven eight nine ten eleven twelve";
        let mut harness = Harness::new();
        let shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(16.0));
        let narrow = min_max_widths(&full).0 + 1.0;
        assert!(
            narrow < full.max_content_size().width,
            "precondition: 必须落在真正的折行区间"
        );

        let size = shaped.measure(none_known(), definite(narrow));
        let full_size = full.measure(none_known(), definite(narrow));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) =
            rasterize_line(&shaped, narrow, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (full_line, _, _) =
            rasterize_line(&full, narrow, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (size.height - line.height).abs() < 1e-3,
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        assert!(
            (full_size.height - full_line.height).abs() < 1e-3,
            "对照：无 max_lines 时两者也必须一致：{} vs {}",
            full_size.height,
            full_line.height
        );
        let removed = full_size.height - size.height;
        assert!(
            removed > line.lines[0].line_height - 1e-3,
            "被截掉的行高必须从 measure 里消失（去掉 {removed}，单行高 {}）",
            line.lines[0].line_height
        );
    }

    #[test]
    fn measure_height_matches_rasterized_lines_for_definite_width() {
        const TEXT: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let mut harness = Harness::new();
        let clamped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(16.0));
        let (min_width, max_width) = min_max_widths(&full);
        let width = min_width + (max_width - min_width) * 0.25;

        let size = clamped.measure(none_known(), definite(width));
        let full_size = full.measure(none_known(), definite(width));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) =
            rasterize_line(&clamped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (full_line, _, _) =
            rasterize_line(&full, width, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (size.height - line.height).abs() < 1e-3,
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        let expected = full_line.lines[0].line_height + full_line.lines[1].line_height;
        assert!(
            (size.height - expected).abs() < 1e-3,
            "钳制高度必须是前两行的真实行高之和：{} vs {expected}",
            size.height
        );
        assert!(
            size.height < full_size.height - 1.0,
            "钳制后必须严格矮于全文：{} vs {}",
            size.height,
            full_size.height
        );
        assert!(
            (full_size.height - full_line.height).abs() < 1e-3,
            "max_lines = None 时 measure 必须等于 rasterize：{} vs {}",
            full_size.height,
            full_line.height
        );
    }

    #[test]
    fn max_lines_clamps_min_and_max_content_height_without_breaking() {
        const TEXT: &str = "one\ntwo\nthree\nfour";
        let mut harness = Harness::new();
        let shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(16.0));

        let max = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        let min = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );
        for _ in 0..32 {
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MinContent,
                    height: AvailableSpace::MinContent,
                },
            );
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                },
            );
        }
        assert_eq!(
            shaped.measure_breaks(),
            0,
            "Min/MaxContent 的 clamp 不得触发任何断行（O(1) 路径）"
        );

        let (expected, full_height) = {
            let layout = shaped.layout();
            let mut lines = layout.lines();
            let first = lines.next().expect("至少一行").metrics().line_height;
            let second = lines
                .next()
                .expect("硬换行 ⇒ 至少两行")
                .metrics()
                .line_height;
            assert_eq!(lines.count() + 2, 4, "硬换行 ⇒ max-content 恰好四行");
            (first + second, layout.height())
        };
        assert!(
            (max.height - expected).abs() < 1e-3,
            "MaxContent 高度必须钳到前两行：{} vs {expected}",
            max.height
        );
        assert!(
            (min.height - max.height).abs() < 1e-3,
            "MinContent 高度同样钳制（纯文本行行高相同）：{} vs {}",
            min.height,
            max.height
        );
        assert!(
            max.height < full_height - 1.0,
            "钳制后必须严格矮于全文：{} vs {full_height}",
            max.height
        );
        assert_eq!(
            shaped.max_content_size().height,
            max.height,
            "max_content_size 与 measure(MaxContent) 必须同源（都取钳制后的 max_size）"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (line.height - max.height).abs() < 1e-3,
            "MaxContent 的 measure 高度必须等于 rasterize 高度：{} vs {}",
            max.height,
            line.height
        );

        let full_size = full.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        assert!(
            (full_size.height - full_height).abs() < 1e-3,
            "全文高度回归：{} vs {full_height}",
            full_size.height
        );
        assert!(max.height < full_size.height - 1.0);
    }

    #[test]
    fn ellipsis_does_not_change_measure_width_and_matches_rasterized_height() {
        const TEXT: &str = "The quick brown fox jumps over the lazy dog";
        let mut harness = Harness::new();
        let ellipsis_shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0)
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(crate::text::TextOverflow::Ellipsis, 0),
        );
        let clip_shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0)
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(crate::text::TextOverflow::Clip, 0),
        );
        let full_width = ellipsis_shaped.max_content_size().width;
        let content_width = full_width * 0.5;
        assert!(
            content_width > 0.0 && content_width < full_width,
            "precondition: 内容宽必须真的裁剪"
        );

        let ellipsis_size = ellipsis_shaped.measure(none_known(), definite(content_width));
        let clip_size = clip_shaped.measure(none_known(), definite(content_width));
        assert_eq!(
            ellipsis_size.width.to_bits(),
            clip_size.width.to_bits(),
            "省略号不得改变宽度上界：{} vs {}",
            ellipsis_size.width,
            clip_size.width
        );
        assert_eq!(
            ellipsis_size.height.to_bits(),
            clip_size.height.to_bits(),
            "省略号不得改变高度"
        );
        assert!(
            (ellipsis_size.width - full_width).abs() < 1e-3,
            "NoWrap 的宽度上界是全文宽（内容宽只做绘制期裁剪）：{} vs {full_width}",
            ellipsis_size.width
        );

        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            ellipsis_shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line_with_ellipsis(
            &ellipsis_shaped,
            content_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );
        assert!(
            line.width <= content_width + 0.01,
            "省略号裁剪后宽度必须在内容宽内：{} vs {content_width}",
            line.width
        );
        assert!(
            line.width < full_width - 1.0,
            "省略号必须真的缩短内容宽：{} vs {full_width}",
            line.width
        );
        assert!(
            (line.height - ellipsis_size.height).abs() < 1e-3,
            "ellipsis 单行高度必须等于 measure：{} vs {}",
            line.height,
            ellipsis_size.height
        );
        assert!(
            all_glyphs(&line).len() < TEXT.chars().count(),
            "必须真的丢掉尾部字形（否则省略号没生效）"
        );

        let multiline_ellipsis = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Ellipsis, 2),
        );
        let multiline_full = harness.shape(TEXT, TextStyle::new(16.0));
        let (min_width, max_width) = min_max_widths(&multiline_full);
        let wrap_width = min_width + (max_width - min_width) * 0.25;
        let multiline_size = multiline_ellipsis.measure(none_known(), definite(wrap_width));
        let (full_line, _, _) = rasterize_line(
            &multiline_full,
            wrap_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &multiline_ellipsis,
            wrap_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );
        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (line.height - multiline_size.height).abs() < 1e-3,
            "ellipsis + max_lines 的高度必须等于 measure：{} vs {}",
            line.height,
            multiline_size.height
        );
        assert!(
            multiline_size.height < full_line.height - 1.0,
            "钳制后必须严格矮于全文：{} vs {}",
            multiline_size.height,
            full_line.height
        );
    }

    #[test]
    fn ellipsis_max_lines_keeps_short_last_line_prefix() {
        const TEXT: &str = "first\nshort\nthird";
        const CONTENT_WIDTH: f32 = 240.0;
        let mut harness = Harness::new();
        let ellipsis_shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Ellipsis, 2),
        );
        let clip_shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let (_, max_width) = min_max_widths(&ellipsis_shaped);
        assert!(
            max_width < CONTENT_WIDTH,
            "precondition: 宽度必须充足（末行未溢出），实得 {max_width} vs {CONTENT_WIDTH}"
        );
        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            ellipsis_shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (clip_line, _, _) = rasterize_line(
            &clip_shaped,
            CONTENT_WIDTH,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &ellipsis_shaped,
            CONTENT_WIDTH,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );

        assert_eq!(clip_line.lines.len(), 2, "Clip + max_lines = 2");
        assert_eq!(line.lines.len(), 2, "Ellipsis + max_lines = 2");
        let clip_last = clip_line.lines.last().expect("clip last line");
        let last = line.lines.last().expect("ellipsis last line");
        let prefix_len = assert_ellipsis_prefix(last, clip_last, &ellipsis, CONTENT_WIDTH);

        let clip_glyphs = line_glyphs(clip_last);
        assert_eq!(
            prefix_len,
            clip_glyphs.len(),
            "宽度充足的末行必须保留全部原行字形（旧实现只剩省略号）：{:?}",
            line_glyphs(last)
        );
        let last_glyphs = line_glyphs(last);
        assert_eq!(
            last_glyphs.len(),
            clip_glyphs.len() + ellipsis.glyphs.len(),
            "末行 = 原行字形 + 省略号字形：{:?}",
            last_glyphs
        );

        let expected_ellipsis = {
            let generation = atlas.generation();
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: 1.0,
            };
            let ppem = (16.0f32 * 1.0).clamp(1.0, 512.0);
            let variation =
                super::GlyphVariation::of(&ellipsis.variation_coords, ellipsis.synthesis, ppem);
            let mut positioned = Vec::with_capacity(ellipsis.glyphs.len());
            let mut x = clip_last.advance;
            for (glyph_id, advance) in &ellipsis.glyphs {
                if let Some((glyph, _)) = super::emit_glyph(
                    &mut ctx,
                    &ellipsis.font,
                    *glyph_id,
                    ppem,
                    &variation,
                    x,
                    clip_last.baseline,
                )
                .expect("ellipsis must not fail")
                {
                    positioned.push(glyph);
                }
                x += advance;
            }
            positioned
        };
        assert_eq!(
            &last_glyphs[prefix_len..],
            expected_ellipsis.as_slice(),
            "省略号必须紧跟前缀终点（位置逐位一致）"
        );
    }

    #[test]
    fn ellipsis_clears_trailing_runs() {
        const TEXT: &str = "abcdef中文中文";
        let mut harness = Harness::new();
        let style = |overflow| {
            TextStyle::new(16.0)
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(overflow, 1)
        };
        let ellipsis_shaped = harness.shape(TEXT, style(crate::text::TextOverflow::Ellipsis));
        let clip_shaped = harness.shape(TEXT, style(crate::text::TextOverflow::Clip));
        let latin_width = harness
            .shape("abcdef", TextStyle::new(16.0).with_wrap(TextWrap::NoWrap))
            .max_content_size()
            .width;
        let width = latin_width - 2.0;
        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            ellipsis_shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (clip_line, _, _) = rasterize_line(
            &clip_shaped,
            width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &ellipsis_shaped,
            width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );
        assert_eq!(line.lines.len(), 1, "NoWrap 恒为一行");

        let clip_last = clip_line.lines.first().expect("clip line");
        let runs = clip_last
            .items
            .iter()
            .filter(|item| matches!(item, ShapedTextItem::GlyphRun(run) if !run.glyphs.is_empty()))
            .count();
        if runs < 2 {
            eprintln!("skip: host has no mixed-script fallback font (single run)");
            return;
        }
        let last = line.lines.first().expect("ellipsis line");
        let reference_glyphs = line_glyphs(clip_last);
        let prefix_len = assert_ellipsis_prefix(last, clip_last, &ellipsis, width);
        assert!(prefix_len > 0, "窄宽仍应放得下至少一个字");
        assert!(
            prefix_len < reference_glyphs.len(),
            "必须真的截断：{prefix_len} vs {}",
            reference_glyphs.len()
        );
        for glyph in line_glyphs(last) {
            assert!(
                glyph.position.x <= last.advance + 0.5,
                "省略号之后不得残留字形：x = {} > advance {}",
                glyph.position.x,
                last.advance
            );
        }
    }

    #[test]
    fn ellipsis_prefix_advance_fits_content_width() {
        const TEXT: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
        let mut harness = Harness::new();
        let style = |overflow| {
            TextStyle::new(16.0)
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(overflow, 0)
        };
        let ellipsis_shaped = harness.shape(TEXT, style(crate::text::TextOverflow::Ellipsis));
        let clip_shaped = harness.shape(TEXT, style(crate::text::TextOverflow::Clip));
        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            ellipsis_shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");
        let full_width = ellipsis_shaped.max_content_size().width;
        let min_width = ellipsis.advance + 1.0;
        assert!(
            full_width > min_width + 2.0,
            "precondition: 文本必须明显宽于省略号（{full_width} vs {min_width}）"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let steps = 32usize;
        let mut previous_prefix = 0usize;
        for step in 0..steps {
            let ratio = step as f32 / (steps - 1) as f32;
            let width = min_width + (full_width - min_width - 1.0) * ratio;
            let (clip_line, _, _) = rasterize_line(
                &clip_shaped,
                width,
                1.0,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            let (line, _, _) = rasterize_line_with_ellipsis(
                &ellipsis_shaped,
                width,
                1.0,
                &mut raster,
                &mut glyphs,
                &mut atlas,
                Some(&ellipsis),
            );
            let reference = clip_line.lines.first().expect("clip line");
            let last = line.lines.first().expect("ellipsis line");
            let prefix_len = assert_ellipsis_prefix(last, reference, &ellipsis, width);
            assert!(
                prefix_len >= previous_prefix,
                "宽度 {width}: 前缀长度必须随宽度单调不减（{prefix_len} < {previous_prefix}）"
            );
            previous_prefix = prefix_len;
        }
        assert!(
            previous_prefix > 0,
            "至少最宽的一档必须保留前缀（否则扫描没覆盖到有效区间）"
        );
    }

    #[test]
    fn max_lines_height_uses_real_per_line_heights() {
        const TEXT: &str = "aa\nbb\ncc\ndd";
        let mut harness = Harness::new();
        let boxes = [
            InlineBoxSpec {
                id: 1,
                index: 1,
                size: crate::math::Size::new(30.0, 40.0),
                baseline: None,
                align: InlineAlign::Baseline,
                paint: None,
            },
            InlineBoxSpec {
                id: 2,
                index: 4,
                size: crate::math::Size::new(30.0, 60.0),
                baseline: None,
                align: InlineAlign::Baseline,
                paint: None,
            },
        ];
        let shaped = harness.shape_with(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
            &boxes,
        );
        let full = harness.shape_with(TEXT, TextStyle::new(16.0), &boxes);
        let width = shaped.max_content_size().width;

        let size = shaped.measure(none_known(), definite(width));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) =
            rasterize_line(&shaped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (full_line, _, _) =
            rasterize_line(&full, width, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert_eq!(full_line.lines.len(), 4, "硬换行 ⇒ 全文四行");
        let plain_line_height = full_line.lines[2].line_height;
        assert!(
            line.lines[0].line_height > plain_line_height + 1.0
                && line.lines[1].line_height > plain_line_height + 1.0,
            "precondition: 行内盒必须把前两行撑高（{} / {} vs 纯文本行 {plain_line_height}）",
            line.lines[0].line_height,
            line.lines[1].line_height
        );

        let expected = line.lines[0].line_height + line.lines[1].line_height;
        assert!(
            (size.height - expected).abs() < 1e-3,
            "measure 必须是前两行真实行高之和：{} vs {expected}",
            size.height
        );
        assert!(
            (size.height - line.height).abs() < 1e-3,
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        let uniform = 2.0 * plain_line_height;
        assert!(
            (size.height - uniform).abs() > 1.0,
            "不得用「可见行数 × 单一 line_height」：{} vs {uniform}",
            size.height
        );
        assert!(
            size.height < full_line.height - 1.0,
            "钳制后必须严格矮于全文：{} vs {}",
            size.height,
            full_line.height
        );
    }

    #[test]
    fn single_line_with_max_lines_one_keeps_its_height() {
        let mut harness = Harness::new();
        let clamped = harness.shape(
            "Hello",
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 1),
        );
        let plain = harness.shape("Hello", TextStyle::new(16.0));
        let size = clamped.measure(none_known(), definite(100.0));
        let plain_size = plain.measure(none_known(), definite(100.0));
        let line_height = {
            let layout = clamped.layout();
            layout
                .lines()
                .next()
                .expect("Hello 必须有一行")
                .metrics()
                .line_height
        };

        assert!(size.height > 0.0, "单行不得被钳成 0：{:?}", size);
        assert!(
            (size.height - line_height).abs() < 1e-3,
            "单行高度必须等于真实行高：{} vs {line_height}",
            size.height
        );
        assert_eq!(
            size.height.to_bits(),
            plain_size.height.to_bits(),
            "max_lines = 1 与不设上限在单行文本上必须逐位相同"
        );
    }

    #[test]
    fn raster_ppem_is_physical_and_grows_with_scale() {
        let mut harness = Harness::new();
        if crate::text::raster::probe_font().is_none() {
            return;
        }
        let shaped = harness.shape("H", TextStyle::new(16.0));
        let logical_width = shaped.max_content_size().width;
        let mut physical = Vec::new();
        for scale in [1.0f32, 1.25, 2.0] {
            let (mut raster, mut glyphs, mut atlas) = glyph_runner();
            let (line, _, stats) = rasterize_line(
                &shaped,
                logical_width,
                scale,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            assert_eq!(stats.rasters, 1, "unique ppem ⇒ one raster");
            assert_eq!(line.width, logical_width, "逻辑宽度与 scale 无关");
            let glyph = all_glyphs(&line).pop().expect("glyph");
            physical.push(glyph.size.height * scale);
        }
        assert!(
            physical[1] > physical[0],
            "physical height grows with the scale: {physical:?}"
        );
        assert!(
            physical[2] > physical[1],
            "physical height grows with the scale: {physical:?}"
        );
    }

    #[test]
    fn explicit_family_changes_the_shape() {
        let stack = FontStack::new(["Consolas"]);
        let with_custom = family_list(&stack, Some("Times New Roman"));
        let without = family_list(&stack, None);

        let names = |family: &parley::FontFamily<'static>| -> Vec<String> {
            match family {
                parley::FontFamily::List(list) => list
                    .iter()
                    .map(|name| match name {
                        parley::FontFamilyName::Named(name) => name.to_string(),
                        parley::FontFamilyName::Generic(generic) => format!("{generic:?}"),
                    })
                    .collect(),
                parley::FontFamily::Single(name) => vec![format!("{name:?}")],
                parley::FontFamily::Source(_) => vec!["<source>".to_owned()],
            }
        };

        let with_custom = names(&with_custom);
        let without = names(&without);
        assert_eq!(
            with_custom.first().map(String::as_str),
            Some("Times New Roman"),
            "显式 family 必须是列表首项（优先级最高），实际：{with_custom:?}"
        );
        assert_eq!(
            with_custom.get(1).map(String::as_str),
            Some("Consolas"),
            "字体栈必须排在显式 family 之后，实际：{with_custom:?}"
        );
        assert_eq!(
            without.first().map(String::as_str),
            Some("Consolas"),
            "没有显式 family 时栈首项就是最高优先级，实际：{without:?}"
        );
        assert_eq!(
            with_custom.len(),
            without.len() + 1,
            "显式 family 只多一项，不改变 fallback 结构：{with_custom:?} vs {without:?}"
        );
    }

    #[test]
    fn explicit_family_changes_advances_when_the_host_has_both_fonts() {
        let mut harness = Harness::new();
        let candidates = [
            "Consolas",
            "Times New Roman",
            "Segoe UI",
            "DejaVu Sans",
            "Liberation Serif",
        ];
        let mut widths = Vec::new();
        for family in candidates {
            harness.family = family_list(&FontStack::new([family]), None);
            let shaped = harness.shape("Hamburgefonstiv", TextStyle::new(16.0));
            widths.push((family, shaped.max_content_size().width));
        }
        let distinct = widths
            .iter()
            .map(|(_, width)| width.to_bits())
            .collect::<std::collections::HashSet<_>>();
        if distinct.len() < 2 {
            eprintln!("skip: host resolves every candidate family to the same font: {widths:?}");
            return;
        }
        assert!(
            distinct.len() >= 2,
            "different families must produce different advances: {widths:?}"
        );
    }

    #[test]
    fn has_only_ltr_runs_follows_the_resolved_bidi_levels() {
        let mut harness = Harness::new();
        for (text, style, expected) in [
            ("Hello", TextStyle::new(16.0), true),
            ("价🌍ab", TextStyle::new(16.0), true),
            ("abc אבג", TextStyle::new(16.0), false),
            (
                "abc",
                TextStyle::new(16.0).with_direction(TextDirection::Ltr),
                true,
            ),
            (
                "אבג",
                TextStyle::new(16.0).with_direction(TextDirection::Ltr),
                false,
            ),
            (
                "abc",
                TextStyle::new(16.0).with_direction(TextDirection::Rtl),
                true,
            ),
        ] {
            let direction = style.direction;
            let shaped = harness.shape(text, style);
            assert_eq!(
                shaped.has_only_ltr_runs(),
                expected,
                "{text:?} direction={direction:?}"
            );
        }
    }

    #[test]
    fn numeric_text_never_has_rtl_runs() {
        let mut text_cx = crate::text::context::TextContext::new();
        for text in ["42150", "-1.50%", "1,234.56", "12/34:56"] {
            let content = TextContent::new(text, TextStyle::new(16.0).with_wrap(TextWrap::NoWrap));
            let shaped = text_cx.shape_cached(&content).0;
            assert!(
                shaped.has_only_ltr_runs(),
                "{text:?} 是纯 ASCII 数字文本：无论走快速路径还是 parley，都不该有 RTL run"
            );
        }
    }
}
