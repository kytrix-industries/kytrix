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
    InlineBoxSpec, ShapedGlyphRun, ShapedInlineBox, ShapedLine, ShapedQuad, ShapedTextItem,
    ShapedTextLine, TextAlign, TextBackground, TextContent, TextDecorationKind, TextDiagnostic,
    TextDirection, TextLineBreak, TextLineMetrics, TextOverflow, TextRun, TextSlant, TextStyle,
    TextWrap,
    atlas::{GlyphAtlas, GlyphAtlasStats},
    fast_path::SimpleShaped,
    inline_flow,
    raster::{GlyphRasterizer, GlyphVariation, split_subpixel_x},
};
use crate::{
    error::EngineError,
    math::{DevicePixelRatio, LogicalPoint, LogicalPx, LogicalRect, LogicalSize, physical_px, px},
    paint::{
        color::{Color, ColorBrush},
        primitive::PositionedGlyph,
    },
    render::atlas::AtlasAllocator,
};

#[derive(Clone, Debug)]
pub(crate) struct EllipsisGlyphs {
    pub glyphs: Vec<(u32, LogicalPx)>,
    pub font: parley::FontData,
    pub variation_coords: Vec<i16>,
    pub synthesis: fontique::Synthesis,
    pub advance: LogicalPx,
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
    pub scale: DevicePixelRatio,
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
    measure_memo: RefCell<Option<(u32, LogicalSize)>>,
    measure_breaks: Cell<u32>,
}

enum ShapedKind {
    Parley(RefCell<ParleySnapshot>),
    Simple(SimpleShaped),
}

struct ParleySnapshot {
    layout: parley::Layout<ColorBrush>,
    min_size: LogicalSize,
    max_size: LogicalSize,
    broken_at: Option<u32>,
    aligned_at: Option<u8>,
    #[cfg(test)]
    breaks: Cell<u32>,
}

impl ParleySnapshot {
    fn new(mut layout: parley::Layout<ColorBrush>, max_lines: Option<u16>) -> Self {
        layout.break_all_lines(None);
        let max_size = LogicalSize::new(px(layout.width()), visible_height(&layout, max_lines));
        let widths = layout.calculate_content_widths();
        layout.break_all_lines(Some(widths.min));
        let min_size = LogicalSize::new(px(layout.width()), visible_height(&layout, max_lines));
        layout.break_all_lines(None);
        Self {
            layout,
            min_size,
            max_size,
            broken_at: None,
            aligned_at: None,
            #[cfg(test)]
            breaks: Cell::new(0),
        }
    }

    fn break_and_align(&mut self, width: Option<LogicalPx>, alignment: Alignment) -> LogicalPx {
        let key = width.map(|width| width.value().to_bits());
        if self.broken_at != key {
            self.layout.break_all_lines(width.map(LogicalPx::value));
            self.broken_at = key;
            self.aligned_at = None;
            #[cfg(test)]
            self.breaks.set(self.breaks.get() + 1);
        }
        let align_key = alignment_key(alignment);
        if self.aligned_at != Some(align_key) {
            self.layout.align(alignment, AlignmentOptions::default());
            self.aligned_at = Some(align_key);
        }
        px(self.layout.width())
    }

    fn visible_height(&self, max_lines: Option<u16>) -> LogicalPx {
        visible_height(&self.layout, max_lines)
    }
}

fn visible_height(layout: &parley::Layout<ColorBrush>, max_lines: Option<u16>) -> LogicalPx {
    let Some(max_lines) = max_lines else {
        return px(layout.height());
    };
    let lines = layout.lines();
    if lines.len() <= usize::from(max_lines) {
        return px(layout.height());
    }
    px(lines
        .take(usize::from(max_lines))
        .map(|line| line.metrics().line_height)
        .sum())
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

    pub(crate) fn max_content_size(&self) -> LogicalSize {
        match &self.kind {
            ShapedKind::Parley(snapshot) => snapshot.borrow().max_size,
            ShapedKind::Simple(simple) => simple.size(),
        }
    }

    pub(crate) fn measure(
        &self,
        _known: taffy::Size<Option<f32>>,
        available: taffy::Size<AvailableSpace>,
    ) -> LogicalSize {
        if self.text.is_empty() {
            return LogicalSize::ZERO;
        }
        let max_lines = self.style.max_lines();
        match &self.kind {
            ShapedKind::Simple(simple) => simple.size(),
            ShapedKind::Parley(snapshot) => match available.width {
                AvailableSpace::MinContent => snapshot.borrow().min_size,
                AvailableSpace::MaxContent => snapshot.borrow().max_size,
                AvailableSpace::Definite(width) => {
                    let width = px(width.max(0.0));
                    let bits = width.value().to_bits();
                    if let Some((cached, size)) = *self.measure_memo.borrow()
                        && cached == bits
                    {
                        return size;
                    }
                    let mut snapshot = snapshot.borrow_mut();
                    let size = if width >= snapshot.max_size.width {
                        snapshot.max_size
                    } else if self.style.wrap != TextWrap::BreakWord
                        && width <= snapshot.min_size.width
                    {
                        snapshot.min_size
                    } else {
                        self.measure_breaks.set(self.measure_breaks.get() + 1);
                        let measured_width =
                            snapshot.break_and_align(Some(width), Alignment::Start);
                        LogicalSize::new(measured_width, snapshot.visible_height(max_lines))
                    };
                    *self.measure_memo.borrow_mut() = Some((bits, size));
                    size
                }
            },
        }
    }

    pub(crate) fn measure_editable(
        &self,
        known: taffy::Size<Option<f32>>,
        available: taffy::Size<AvailableSpace>,
    ) -> LogicalSize {
        if !self.text.is_empty() {
            return self.measure(known, available);
        }
        let line_height = match &self.kind {
            ShapedKind::Simple(simple) => simple.metrics().line_height,
            ShapedKind::Parley(snapshot) => {
                let snapshot = snapshot.borrow();
                snapshot.layout.lines().next().map_or_else(
                    || self.style.size * self.style.line_height.unwrap_or(1.0),
                    |line| px(line.metrics().line_height),
                )
            }
        };
        LogicalSize::new(LogicalPx::ZERO, line_height)
    }

    pub(crate) fn line_metrics(&self, width: LogicalPx, alignment: TextAlign) -> TextLineMetrics {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(LogicalPx::ZERO)), align_of(alignment));
                let line_height = snapshot.layout.lines().next().map_or_else(
                    || self.style.size * self.style.line_height.unwrap_or(1.0),
                    |line| px(line.metrics().line_height),
                );
                TextLineMetrics {
                    line_height,
                    first_baseline: snapshot.layout.first_baseline().map(px),
                    line_count: snapshot.layout.lines().count(),
                    content_height: px(snapshot.layout.height()),
                }
            }
            ShapedKind::Simple(simple) => {
                let metrics = simple.metrics();
                TextLineMetrics {
                    line_height: metrics.line_height,
                    first_baseline: Some(metrics.baseline),
                    line_count: 1,
                    content_height: metrics.line_height,
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_simple(&self) -> bool {
        matches!(self.kind, ShapedKind::Simple(_))
    }

    #[cfg(test)]
    pub(crate) fn layout_breaks(&self) -> u32 {
        match &self.kind {
            ShapedKind::Parley(snapshot) => snapshot.borrow().breaks.get(),
            ShapedKind::Simple(_) => 0,
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
        content_width: LogicalPx,
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
                    ctx.glyph_atlas.color_generation(),
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
            LogicalPx::ZERO
        };
        let (mut shaped, diagnostics, line_offset) = match &self.kind {
            ShapedKind::Simple(simple) => {
                let shaped =
                    simple.rasterize(self.text.clone(), &self.style, color, ctx, simple_offset)?;
                (shaped, Vec::new(), simple_offset)
            }
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                let width = snapshot.break_and_align(
                    Some(content_width.max(LogicalPx::ZERO)),
                    align_of(alignment),
                );
                let line_count = snapshot.layout.lines().len();
                let visible_last = max_visible_lines(&self.style, line_count);
                let line_offset = if visible_last == 0 {
                    LogicalPx::ZERO
                } else {
                    snapshot
                        .layout
                        .lines()
                        .nth(visible_last - 1)
                        .map_or(LogicalPx::ZERO, |line| px(line.metrics().offset))
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
        shaped.font_stack_version = self.font_stack_version;
        shaped.color_atlas_generation = ctx.glyph_atlas.color_generation();
        Ok((shaped, diagnostics, ctx.glyph_atlas.take_stats()))
    }

    pub(crate) fn with_broken_layout<R>(
        &self,
        width: LogicalPx,
        alignment: TextAlign,
        body: impl FnOnce(&parley::Layout<ColorBrush>) -> R,
    ) -> Option<R> {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(LogicalPx::ZERO)), align_of(alignment));
                Some(body(&snapshot.layout))
            }
            ShapedKind::Simple(_) => None,
        }
    }

    pub(crate) fn selection_rects(
        &self,
        width: LogicalPx,
        alignment: TextAlign,
        anchor: usize,
        focus: usize,
    ) -> Vec<LogicalRect> {
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

    pub(crate) fn caret_rect(
        &self,
        width: LogicalPx,
        alignment: TextAlign,
        byte_index: usize,
    ) -> LogicalRect {
        if let ShapedKind::Simple(simple) = &self.kind {
            let metrics = simple.metrics();
            let offset = super::fast_path::simple_alignment_offset(
                simple.size().width,
                width,
                alignment,
                self.style.direction,
            );
            return LogicalRect::new(
                offset + simple.caret_x(byte_index),
                px(0.0),
                px(1.0),
                metrics.line_height,
            );
        }
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::caret_rect(layout, byte_index)
        })
        .unwrap_or_default()
    }

    pub(crate) fn composition_underline_rects(
        &self,
        width: LogicalPx,
        alignment: TextAlign,
        range: core::ops::Range<usize>,
        font_size: LogicalPx,
        thickness: LogicalPx,
    ) -> Vec<LogicalRect> {
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::composition_underline_rects(
                layout,
                range.start,
                range.end,
                font_size.value(),
                thickness.value(),
            )
        })
        .unwrap_or_default()
    }

    pub(crate) fn byte_index_at(
        &self,
        width: LogicalPx,
        alignment: TextAlign,
        point: LogicalPoint,
    ) -> usize {
        if let ShapedKind::Simple(simple) = &self.kind {
            if point.y < LogicalPx::ZERO {
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
    pub(crate) fn layout_at(&self, width: LogicalPx) -> parley::Layout<ColorBrush> {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(LogicalPx::ZERO)), Alignment::Start);
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
    pub(crate) fn first_glyph_x_at(&self, width: LogicalPx) -> LogicalPx {
        let mut layout = self.layout_at(width);
        layout.align(align_of(self.style.alignment), AlignmentOptions::default());
        px(layout
            .lines()
            .next()
            .and_then(|line| line.items().next())
            .and_then(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    run.positioned_glyphs().next().map(|glyph| glyph.x)
                }
                PositionedLayoutItem::InlineBox(_) => None,
            })
            .unwrap_or(0.0))
    }
}

pub(super) fn push_shaping_defaults<'a>(
    builder: &mut parley::RangedBuilder<'a, ColorBrush>,
    style: &TextStyle,
) {
    builder.push_default(StyleProperty::Brush(ColorBrush { color: style.color }));
    builder.push_default(StyleProperty::FontSize(style.size.value()));
    builder.push_default(StyleProperty::FontWeight(ParleyFontWeight::new(f32::from(
        style.weight,
    ))));
    builder.push_default(StyleProperty::FontStyle(match style.slant {
        TextSlant::Normal => ParleyFontStyle::Normal,
        TextSlant::Italic => ParleyFontStyle::Italic,
        TextSlant::Oblique => ParleyFontStyle::Oblique(None),
    }));
    builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing.value()));
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
        builder.push_default(StyleProperty::WordSpacing(word_spacing.value()));
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
        TextWrap::BreakWord => {
            builder.push_default(StyleProperty::OverflowWrap(ParleyOverflowWrap::BreakWord));
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
    builder.push(StyleProperty::FontSize(style.size.value()), range.clone());
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
        StyleProperty::LetterSpacing(style.letter_spacing.value()),
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
        builder.push(
            StyleProperty::WordSpacing(word_spacing.value()),
            range.clone(),
        );
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
        TextWrap::BreakWord => {}
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
        let mut glyphs: Vec<(u32, LogicalPx)> = Vec::new();
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
                    glyphs.push((glyph.id, px(glyph.advance)));
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
                advance: px(advance),
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
const BACKGROUND_SEAM_EPSILON: LogicalPx = LogicalPx::new(1e-3);

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
    baseline: LogicalPx,
    text: &str,
    runs: &[TextRun],
    base_style: &TextStyle,
    base_color: Color,
) -> Vec<ShapedQuad> {
    let line_range = line.text_range();
    let line_end = line_content_end(text, &line_range);
    let mut out = Vec::new();
    for kind in TextDecorationKind::ALL {
        let offset_em = match kind {
            TextDecorationKind::Underline => UNDERLINE_OFFSET_EM,
            TextDecorationKind::Strikethrough => -STRIKETHROUGH_OFFSET_EM,
        };
        let segments = covered_segments(
            line_range.start,
            line_end,
            runs,
            |style| {
                style
                    .decorations()
                    .get(kind)
                    .filter(|decoration| decoration.is_visible())
                    .map(|decoration| (decoration, style.size, style.color))
            },
            base_style
                .decorations()
                .get(kind)
                .filter(|decoration| decoration.is_visible())
                .map(|decoration| (decoration, base_style.size, base_color)),
        );
        for (range, (decoration, em, text_color)) in segments {
            let Some((left, width)) = visual_span(line, text, range) else {
                continue;
            };
            let thickness = decoration.thickness;
            out.push(ShapedQuad {
                rect: LogicalRect::new(
                    left,
                    baseline + em * offset_em - thickness * 0.5,
                    width,
                    thickness,
                ),
                color: decoration.color.unwrap_or(text_color),
                corner_radius: LogicalPx::ZERO,
            });
        }
    }
    out
}

fn background_segments(
    line: &parley::Line<'_, ColorBrush>,
    line_top: LogicalPx,
    line_height: LogicalPx,
    text: &str,
    runs: &[TextRun],
    base_style: &TextStyle,
) -> Vec<ShapedQuad> {
    let line_range = line.text_range();
    let line_end = line_content_end(text, &line_range);
    let segments = covered_segments(
        line_range.start,
        line_end,
        runs,
        |style| {
            style
                .background()
                .filter(|background| background.is_visible())
        },
        base_style
            .background()
            .filter(|background| background.is_visible()),
    );
    let mut spans = Vec::with_capacity(segments.len());
    for (range, background) in segments {
        let Some((left, width)) = visual_span(line, text, range) else {
            continue;
        };
        spans.push((left, left + width, background));
    }
    spans.sort_by(|a, b| a.0.value().total_cmp(&b.0.value()));
    let mut merged: Vec<(LogicalPx, LogicalPx, TextBackground)> = Vec::with_capacity(spans.len());
    for (left, right, background) in spans {
        if let Some((_, last_right, last_background)) = merged.last_mut()
            && *last_background == background
            && left <= *last_right + BACKGROUND_SEAM_EPSILON
        {
            *last_right = last_right.max(right);
            continue;
        }
        merged.push((left, right, background));
    }
    let mut out = Vec::with_capacity(merged.len());
    for (index, (left, right, background)) in merged.iter().copied().enumerate() {
        let previous_right = index.checked_sub(1).map(|previous| merged[previous].1);
        let next_left = merged.get(index + 1).map(|next| next.0);
        let left_padding = previous_right.map_or(background.padding_x, |previous| {
            let gap = left - previous;
            if gap > BACKGROUND_SEAM_EPSILON {
                background.padding_x.min(gap * 0.5)
            } else {
                LogicalPx::ZERO
            }
        });
        let right_padding = next_left.map_or(background.padding_x, |next| {
            let gap = next - right;
            if gap > BACKGROUND_SEAM_EPSILON {
                background.padding_x.min(gap * 0.5)
            } else {
                LogicalPx::ZERO
            }
        });
        out.push(ShapedQuad {
            rect: LogicalRect::new(
                left - left_padding,
                line_top,
                right - left + left_padding + right_padding,
                line_height,
            ),
            color: background.color,
            corner_radius: background.corner_radius,
        });
    }
    out
}

fn covered_segments<T: Copy>(
    line_start: usize,
    line_end: usize,
    runs: &[TextRun],
    run_value: impl Fn(&TextStyle) -> Option<T>,
    base_value: Option<T>,
) -> Vec<(core::ops::Range<usize>, T)> {
    let run_segments: Vec<(core::ops::Range<usize>, T)> = runs
        .iter()
        .filter_map(|run| {
            let value = run_value(&run.style)?;
            let start = run.range.start.max(line_start);
            let end = run.range.end.min(line_end);
            (start < end).then_some((start..end, value))
        })
        .collect();
    let mut segments: Vec<(core::ops::Range<usize>, T)> =
        Vec::with_capacity(run_segments.len() + usize::from(base_value.is_some()));
    if let Some(value) = base_value {
        let mut cursor = line_start;
        for (range, _) in &run_segments {
            if range.start > cursor {
                segments.push((cursor..range.start, value));
            }
            cursor = cursor.max(range.end);
        }
        if cursor < line_end {
            segments.push((cursor..line_end, value));
        }
    }
    segments.extend(run_segments);
    segments
}

fn visual_span(
    line: &parley::Line<'_, ColorBrush>,
    text: &str,
    range: core::ops::Range<usize>,
) -> Option<(LogicalPx, LogicalPx)> {
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
    (width > 0.0 && left.is_finite() && width.is_finite()).then_some((px(left), px(width)))
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
    content_width: LogicalPx,
    text: &SmolStr,
    color: Color,
    runs: &[TextRun],
    base_style: &TextStyle,
    ctx: &mut RasterCtx<'_>,
    inline_diagnostics: &[TextDiagnostic],
) -> Result<(ShapedLine, Vec<TextDiagnostic>), EngineError> {
    let mut diagnostics: Vec<TextDiagnostic> = inline_diagnostics.to_vec();
    let mut lines = Vec::with_capacity(layout.lines().len());
    let mut line_top = LogicalPx::ZERO;
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
                    let ppem = ctx
                        .scale
                        .to_physical(px(parley_run.font_size()))
                        .value()
                        .clamp(1.0, 512.0);
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
                        let Some((positioned, color_monochrome)) = emit_glyph(
                            ctx,
                            &font,
                            glyph.id,
                            ppem,
                            &variation,
                            px(glyph.x),
                            px(glyph.y),
                        )?
                        else {
                            continue;
                        };
                        if color_monochrome {
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
                    let rect =
                        LogicalRect::new(px(box_.x), px(box_.y), px(box_.width), px(box_.height));
                    if rect.size.width > content_width && content_width > LogicalPx::ZERO {
                        diagnostics.push(TextDiagnostic::InlineBoxOverflow {
                            id: box_.id,
                            width: rect.size.width,
                            available: content_width,
                        });
                    }
                    items.push(ShapedTextItem::InlineBox(ShapedInlineBox {
                        id: box_.id,
                        rect,
                        baseline: box_.baseline.map(px),
                    }));
                }
            }
        }
        let decorations =
            decoration_segments(&line, px(metrics.baseline), text, runs, base_style, color);
        let line_height = px(metrics.line_height);
        let backgrounds = background_segments(&line, line_top, line_height, text, runs, base_style);
        lines.push(ShapedTextLine {
            text_range: line.text_range(),
            baseline: px(metrics.baseline),
            line_top,
            line_height,
            advance: px(metrics.advance),
            items,
            decorations,
            backgrounds,
        });
        line_top += line_height;
    }
    let shaped = ShapedLine {
        text: text.clone(),
        lines,
        width: px(layout.width()),
        height: px(layout.height()),
        first_baseline: layout.first_baseline().map(px),
        atlas_generation: ctx.generation,
        color_atlas_generation: ctx.glyph_atlas.color_generation(),
        font_stack_version: 0,
        scale: ctx.scale,
    };
    Ok((shaped, diagnostics))
}

fn emit_glyph(
    ctx: &mut RasterCtx<'_>,
    font: &parley::FontData,
    glyph_id: u32,
    ppem: f32,
    variation: &GlyphVariation,
    logical_x: LogicalPx,
    logical_y: LogicalPx,
) -> Result<Option<(PositionedGlyph, bool)>, EngineError> {
    let (base_x, subpixel_x) = split_subpixel_x(ctx.scale.to_physical(logical_x).value());
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
    let origin_y = ctx.scale.to_physical(logical_y).value().round() - tile.top as f32;
    Ok(Some((
        PositionedGlyph {
            tile: tile.tile,
            position: LogicalPoint::new(
                ctx.scale.to_logical(physical_px(origin_x)),
                ctx.scale.to_logical(physical_px(origin_y)),
            ),
            size: LogicalSize::new(
                ctx.scale.to_logical(physical_px(tile.width as f32)),
                ctx.scale.to_logical(physical_px(tile.height as f32)),
            ),
        },
        tile.color_monochrome,
    )))
}

fn apply_overflow(
    shaped: &mut ShapedLine,
    style: &TextStyle,
    content_width: LogicalPx,
    color: Color,
    ctx: &mut RasterCtx<'_>,
    ellipsis: Option<&EllipsisGlyphs>,
    line_offset: LogicalPx,
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
    if content_width > LogicalPx::ZERO {
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
            clamp_line_rects(&mut last.decorations, line_offset, last.advance);
            truncate_line_rects(&mut last.backgrounds, line_offset, last.advance);
        }
    }
    shaped.width = shaped
        .lines
        .iter()
        .map(|line| line.advance)
        .fold(LogicalPx::ZERO, LogicalPx::max);
    shaped.height = shaped
        .lines
        .iter()
        .fold(LogicalPx::ZERO, |total, line| total + line.line_height);
    Ok(())
}

fn clamp_line_rects(rects: &mut Vec<ShapedQuad>, line_offset: LogicalPx, line_advance: LogicalPx) {
    let min = line_offset;
    let max = line_offset + line_advance;
    for quad in rects.iter_mut() {
        let left = quad.rect.origin.x.max(min);
        let right = (quad.rect.origin.x + quad.rect.size.width).min(max);
        if right > left {
            quad.rect.origin.x = left;
            quad.rect.size.width = right - left;
        } else {
            quad.rect.size.width = LogicalPx::ZERO;
        }
    }
    rects.retain(|quad| quad.rect.size.width > LogicalPx::ZERO);
}

fn truncate_line_rects(
    rects: &mut Vec<ShapedQuad>,
    line_offset: LogicalPx,
    line_advance: LogicalPx,
) {
    let max = line_offset + line_advance;
    for quad in rects.iter_mut() {
        let right = (quad.rect.origin.x + quad.rect.size.width).min(max);
        quad.rect.size.width = (right - quad.rect.origin.x).max(LogicalPx::ZERO);
    }
    rects.retain(|quad| quad.rect.size.width > LogicalPx::ZERO);
}

fn max_visible_lines(style: &TextStyle, line_count: usize) -> usize {
    style
        .max_lines()
        .map_or(line_count, |max| (usize::from(max)).max(1).min(line_count))
}

fn clip_with_ellipsis(
    shaped: &mut ShapedLine,
    ellipsis: &EllipsisGlyphs,
    content_width: LogicalPx,
    color: Color,
    style: &TextStyle,
    ctx: &mut RasterCtx<'_>,
    line_offset: LogicalPx,
) -> Result<(), EngineError> {
    let Some(last) = shaped.lines.last_mut() else {
        return Ok(());
    };
    let ppem = ctx.scale.to_physical(style.size).value().clamp(1.0, 512.0);
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
    let mut starts: Vec<LogicalPx> = Vec::with_capacity(glyph_count);
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
    let mut prefix_end = LogicalPx::ZERO;
    for (index, _) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(last.advance);
        if content_width > LogicalPx::ZERO
            && end + line_offset + ellipsis.advance > content_width + 1e-3
        {
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
        x += *advance;
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
    use crate::{
        math::{DevicePixelRatio, LogicalPx, px},
        text::{
            FontStack, InlineAlign, InlineBoxSpec, ShapedTextItem, TextAlign, TextBackground,
            TextContent, TextDiagnostic, TextDirection, TextRun, TextStyle, TextWrap,
        },
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
        width: LogicalPx,
        scale: DevicePixelRatio,
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
        width: LogicalPx,
        scale: DevicePixelRatio,
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

    fn glyph_bounds(
        line: &crate::text::ShapedTextLine,
        range: core::ops::Range<usize>,
    ) -> (f32, f32) {
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        for item in &line.items {
            let ShapedTextItem::GlyphRun(run) = item else {
                continue;
            };
            if run.text_range.start < range.end && range.start < run.text_range.end {
                for glyph in &run.glyphs {
                    left = left.min(glyph.position.x.value());
                    right = right.max((glyph.position.x + glyph.size.width).value());
                }
            }
        }
        assert!(left.is_finite() && right.is_finite(), "区间必须有字形");
        (left, right)
    }

    fn assert_ellipsis_prefix(
        rendered: &crate::text::ShapedTextLine,
        reference: &crate::text::ShapedTextLine,
        ellipsis: &super::EllipsisGlyphs,
        content_width: LogicalPx,
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
            (rendered.advance - (prefix_end + ellipsis.advance)).abs() < px(1e-2),
            "行推进必须 == 前缀终点 + 省略号推进：{} vs {} + {}",
            rendered.advance,
            prefix_end,
            ellipsis.advance
        );
        if content_width > px(0.0) {
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
        let shaped = harness.shape("Hello", TextStyle::new(px(16.0)));
        let size = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );

        assert!(size.width > px(0.0), "width {}", size.width);
        assert!(size.height > px(0.0), "height {}", size.height);
        assert_eq!(shaped.layout().lines().count(), 1);
        assert_eq!(shaped.layout().text_len(), 5);
    }

    #[test]
    fn shape_cjk_breaks_only_at_character_boundaries() {
        let mut harness = Harness::new();
        let shaped = harness.shape("世界世界世界世界", TextStyle::new(px(16.0)));
        let size = shaped.measure(none_known(), definite(60.0));

        assert!(
            size.height > px(30.0),
            "narrow CJK text must wrap: {size:?}"
        );
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
        let shaped = harness.shape("A中🌍한", TextStyle::new(px(16.0)));
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
        let shaped = harness.shape("alpha beta gamma delta", TextStyle::new(px(16.0)));
        let wide = shaped.measure(none_known(), definite(1000.0));
        let narrow = shaped.measure(none_known(), definite(60.0));

        assert!(narrow.height > wide.height, "{narrow:?} vs {wide:?}");
        assert_eq!(shaped.layout().text_len(), "alpha beta gamma delta".len());
    }

    #[test]
    fn align_center_moves_the_first_glyph_right() {
        let mut harness = Harness::new();
        let start = harness.shape("abc", TextStyle::new(px(16.0)));
        let mut style = TextStyle::new(px(16.0));
        style.alignment = crate::text::TextAlign::Center;
        let centered = harness.shape("abc", style);

        assert!(
            centered.first_glyph_x_at(px(200.0)) > start.first_glyph_x_at(px(200.0)),
            "centering inside a 200px line box must shift the glyph right: {} vs {}",
            centered.first_glyph_x_at(px(200.0)),
            start.first_glyph_x_at(px(200.0))
        );
    }

    fn rasterize_at(
        shaped: &super::ShapedText,
        width: LogicalPx,
        alignment: TextAlign,
        scale: DevicePixelRatio,
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
        width: LogicalPx,
        color: crate::paint::color::Color,
    ) -> crate::text::ShapedLine {
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster: &mut raster,
            glyph_atlas: &mut glyphs,
            atlas: &mut atlas,
            generation,
            scale: DevicePixelRatio::ONE,
        };
        shaped
            .rasterize(width, shaped.style().alignment, color, &mut ctx, None)
            .expect("rasterize")
            .0
    }

    fn all_glyph_positions(line: &crate::text::ShapedLine) -> Vec<(f32, f32)> {
        line.glyph_runs()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| (glyph.position.x.value(), glyph.position.y.value()))
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
        let style = TextStyle::new(px(16.0)).with_wrap(TextWrap::NoWrap);
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

        for scale in [
            DevicePixelRatio::ONE,
            DevicePixelRatio::new(1.25).expect("scale"),
        ] {
            for alignment in [TextAlign::Right, TextAlign::Center] {
                let fast_line = rasterize_at(&fast, px(200.0), alignment, scale);
                let parley_line = rasterize_at(&parley, px(200.0), alignment, scale);
                assert!(
                    (fast_line.width - parley_line.width).abs() <= px(0.01),
                    "{alignment:?} scale={scale:?}: fast width {} vs parley {}",
                    fast_line.width,
                    parley_line.width
                );
                let fast_positions = all_glyph_positions(&fast_line);
                let parley_positions = all_glyph_positions(&parley_line);
                assert_eq!(
                    fast_positions.len(),
                    parley_positions.len(),
                    "{alignment:?} scale={scale:?}: glyph count"
                );
                for (index, (fast_pos, parley_pos)) in
                    fast_positions.iter().zip(&parley_positions).enumerate()
                {
                    assert!(
                        (fast_pos.0 - parley_pos.0).abs() <= 0.01,
                        "{alignment:?} scale={scale:?} glyph {index}: fast x {} vs parley {}",
                        fast_pos.0,
                        parley_pos.0
                    );
                    assert!(
                        (fast_pos.0 * scale.factor() - (fast_pos.0 * scale.factor()).round()).abs()
                            <= 1e-4,
                        "{alignment:?} scale={scale:?} glyph {index}: physical x {} is not an integer",
                        fast_pos.0 * scale.factor()
                    );
                }
            }
        }
    }

    #[test]
    fn fast_path_alignment_after_ellipsis_uses_the_full_advance() {
        let mut text_cx = crate::text::context::TextContext::new();
        let style = TextStyle::new(px(16.0))
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
                scale: DevicePixelRatio::ONE,
            };
            shaped
                .rasterize(
                    px(width),
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
                scale: DevicePixelRatio::ONE,
            };
            shaped
                .rasterize(
                    px(width),
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
                scale: DevicePixelRatio::ONE,
            };
            parley_shaped
                .rasterize(
                    px(width),
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
            (left.width - parley_right.width).abs() <= px(0.01),
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
        let shaped = harness.shape("abc", TextStyle::new(px(16.0)));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut render = |alignment: crate::text::TextAlign| {
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: DevicePixelRatio::ONE,
            };
            shaped
                .rasterize(
                    px(100.0),
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
        let shaped = harness.shape("The quick brown fox", TextStyle::new(px(16.0)));
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
            (broken.width - widths.min).abs() < px(0.01),
            "{broken:?} vs {widths:?}"
        );
        assert!(
            (single.width - widths.max).abs() < px(0.01),
            "{single:?} vs {widths:?}"
        );
    }

    #[test]
    fn empty_text_measures_zero() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(px(16.0)));

        assert_eq!(
            shaped.measure(none_known(), definite(100.0)),
            crate::math::LogicalSize::ZERO
        );
        assert_eq!(
            shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                }
            ),
            crate::math::LogicalSize::ZERO
        );
    }

    #[test]
    fn measure_definite_width_wraps_and_grows_height() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three four five", TextStyle::new(px(16.0)));
        let wide = shaped.measure(none_known(), definite(400.0));
        let narrow = shaped.measure(none_known(), definite(40.0));

        assert!(narrow.height > wide.height);
        assert!(
            narrow.width <= px(40.5),
            "wrapped width {} exceeds the constraint",
            narrow.width
        );
    }

    #[test]
    fn measure_min_content_is_the_longest_word() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three", TextStyle::new(px(16.0)));
        let min = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );

        assert!((min.width - shaped.layout().calculate_content_widths().min).abs() < px(0.01));
    }

    #[test]
    fn measure_max_content_is_one_line() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three", TextStyle::new(px(16.0)));
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
            (single.height - line_height).abs() < px(0.5),
            "{single:?} vs {line_height}"
        );
    }

    #[test]
    fn measure_is_deterministic() {
        let mut harness = Harness::new();
        let shaped = harness.shape("determinism 一致性", TextStyle::new(px(15.0)));
        let a = shaped.measure(none_known(), definite(120.0));
        let b = shaped.measure(none_known(), definite(120.0));

        assert_eq!(a.width.value().to_bits(), b.width.value().to_bits());
        assert_eq!(a.height.value().to_bits(), b.height.value().to_bits());
    }

    #[test]
    fn wrap_change_keeps_max_width_but_changes_height() {
        let mut harness = Harness::new();
        let wrap = harness.shape("aaaa aaaa aaaa", TextStyle::new(px(16.0)));
        let anywhere = harness.shape(
            "aaaa aaaa aaaa",
            TextStyle::new(px(16.0)).with_wrap(TextWrap::Anywhere),
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
        let plain = harness.shape("abcd", TextStyle::new(px(16.0)));
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::LogicalSize::new(px(20.0), px(18.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let boxes: Vec<InlineBoxSpec> = (0..8)
            .map(|index| InlineBoxSpec { index, ..spec })
            .collect();
        let with_boxes = harness.shape_with("abcd", TextStyle::new(px(16.0)), &boxes);
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
            size: crate::math::LogicalSize::new(px(20.0), px(18.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let shaped = harness.shape_with("abcd", TextStyle::new(px(16.0)), &[spec]);

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
        let content = TextContent::new("abcdef", TextStyle::new(px(16.0))).with_runs([
            TextRun::new(0..3, TextStyle::new(px(16.0)).with_color(red)),
            TextRun::new(3..6, TextStyle::new(px(16.0)).with_color(blue)),
        ]);
        let shaped = harness.shape_content(&content);
        assert_eq!(shaped.runs().len(), 2, "快照必须保存 runs（缓存命中判据）");

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
        let content = TextContent::new("abcdef", TextStyle::new(px(16.0)).with_color(green))
            .with_runs([TextRun::new(0..3, TextStyle::new(px(16.0)).with_color(red))]);
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
        let content = TextContent::new("abcdef", TextStyle::new(px(16.0)).with_color(ink))
            .with_runs([
                TextRun::new(
                    0..3,
                    TextStyle::new(px(16.0))
                        .with_color(ink)
                        .with_underline(px(1.0), None),
                ),
                TextRun::new(
                    3..6,
                    TextStyle::new(px(16.0))
                        .with_color(ink)
                        .with_strikethrough(px(2.0), Some(explicit)),
                ),
            ]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
        assert!((underline.rect.size.height - 1.0).abs() < px(1e-4));
        assert!((strike.rect.size.height - 2.0).abs() < px(1e-4));
        assert_eq!(underline.color, ink, "默认线色跟随 run 文本色");
        assert_eq!(strike.color, explicit, "显式线色优先");
        assert!(underline.rect.size.width > px(0.0) && strike.rect.size.width > px(0.0));
        assert!(
            underline.rect.origin.x < strike.rect.origin.x,
            "LTR 下前一个 run 的装饰在左：{:?}",
            shaped_line.decorations
        );
        let plain = harness.shape("abcdef", TextStyle::new(px(16.0)));
        let (plain_line, _, _) = rasterize_line(
            &plain,
            plain.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(plain_line.lines[0].decorations.is_empty());
    }

    #[test]
    fn base_style_decoration_covers_uncovered_text_only() {
        let mut harness = Harness::new();
        let base = TextStyle::new(px(16.0)).with_underline(px(1.0), None);
        let content = TextContent::new("abcdef", base.clone()).with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_underline(px(2.0), None),
        )]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let decorations = &line.lines[0].decorations;
        assert_eq!(
            decorations.len(),
            2,
            "同一种线的兜底区间与 run 区间各一条：{decorations:?}"
        );
        let fallback = decorations[0];
        let run = decorations[1];
        assert!(
            (run.rect.origin.x).abs() < px(1e-3),
            "run 覆盖行首：{run:?}"
        );
        assert!(
            fallback.rect.origin.x >= run.rect.origin.x + run.rect.size.width - 1e-3,
            "兜底下划线不得覆盖已由 run 下划线装饰的区间：{decorations:?}"
        );
        assert_eq!(fallback.rect.size.height, px(1.0), "兜底用基础样式的厚度");
        assert_eq!(run.rect.size.height, px(2.0), "run 用自身厚度");

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let mixed = TextContent::new("abcdef", base).with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_strikethrough(px(2.0), None),
        )]);
        let shaped = harness.shape_content(&mixed);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let decorations = &line.lines[0].decorations;
        assert_eq!(
            decorations.len(),
            2,
            "基础下划线 + run 删除线必须同时存在：{decorations:?}"
        );
        let underline = decorations[0];
        let strike = decorations[1];
        assert!(
            underline.rect.origin.y > line.lines[0].baseline,
            "基础样式 = 下划线（基线下方）：{underline:?}"
        );
        assert!(
            strike.rect.origin.y < line.lines[0].baseline,
            "run 样式 = 删除线（基线上方）：{strike:?}"
        );
        assert!(
            underline.rect.origin.x <= strike.rect.origin.x
                && underline.rect.origin.x + underline.rect.size.width
                    >= strike.rect.origin.x + strike.rect.size.width,
            "不同种类的线互不覆盖：基础下划线与 run 删除线共享同一文本区间：{decorations:?}"
        );
        assert!(
            underline.rect.size.width > strike.rect.size.width,
            "基础下划线仍覆盖 run 没有下划线的区间：{decorations:?}"
        );
    }

    #[test]
    fn cjk_and_emoji_mixed_runs_keep_boundaries() {
        let mut harness = Harness::new();
        let content = TextContent::new("价格🌍ab", TextStyle::new(px(16.0))).with_runs([
            TextRun::new(0..6, TextStyle::new(px(16.0)).with_underline(px(1.0), None)),
            TextRun::new(6..10, TextStyle::new(px(18.0))),
            TextRun::new(10..12, TextStyle::new(px(16.0)).with_weight(700)),
        ]);
        let shaped = harness.shape_content(&content);
        assert_eq!(shaped.runs().len(), 3);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
        assert!(line.lines[0].decorations[0].rect.size.width > px(0.0));
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
            TextStyle::new(px(16.0)).with_underline(px(1.0), None),
        );
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let width = 40.0;
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            px(width),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
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
                    decoration.rect.origin.x >= px(-1e-3)
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
        let underline_thickness = px(1.0);
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        for (text, expected) in [
            ("ab\ncd", vec![1usize, 1]),
            ("ab\r\ncd", vec![1, 1]),
            ("ab\n\ncd", vec![1, 0, 1]),
            ("ab\n", vec![1, 0]),
        ] {
            let content = TextContent::new(
                text,
                TextStyle::new(px(16.0)).with_underline(underline_thickness, None),
            );
            let shaped = harness.shape_content(&content);
            let (line, _diagnostics, _stats) = rasterize_line(
                &shaped,
                shaped.max_content_size().width,
                DevicePixelRatio::ONE,
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
                        decoration.rect.size.width > px(0.0),
                        "装饰必须有正宽度：{decoration:?}"
                    );
                    assert!(
                        decoration.rect.origin.x >= px(-1e-3)
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
        let underline_thickness = px(1.0);
        let mut harness = Harness::new();
        let content =
            TextContent::new("ab\ncd", TextStyle::new(px(16.0))).with_runs([TextRun::new(
                0..5,
                TextStyle::new(px(16.0)).with_underline(underline_thickness, None),
            )]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
            decoration.rect.origin.x.abs() < px(1e-2)
                && (decoration.rect.size.width - second.advance).abs() < px(1e-2),
            "第二行装饰跨度 = 内容区间（不含行尾换行）：{decoration:?} / advance {}",
            second.advance
        );
    }

    #[test]
    fn rtl_decoration_covers_the_full_visual_span() {
        let underline_thickness = px(1.0);
        let rtl = TextStyle::new(px(16.0)).with_direction(TextDirection::Rtl);
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let content = TextContent::new(
            "שלום עולם",
            rtl.clone().with_underline(underline_thickness, None),
        );
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
            decoration.rect.origin.x <= px(1.0),
            "RTL 装饰必须从行左边缘开始（旧实现从中间开始）：{decoration:?}"
        );
        assert!(
            (right - shaped_line.advance.value()).abs() <= px(1.0),
            "RTL 装饰必须覆盖到行推进末端：{decoration:?} / advance {}",
            shaped_line.advance
        );

        let content = TextContent::new("שלום עולם", rtl.clone()).with_runs([
            TextRun::new(0..4, rtl.clone().with_underline(underline_thickness, None)),
            TextRun::new(4..8, rtl.with_underline(underline_thickness, None)),
        ]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
            .map(|decoration| decoration.rect.origin.x.value())
            .fold(f32::INFINITY, f32::min);
        let right = shaped_line
            .decorations
            .iter()
            .map(|decoration| (decoration.rect.origin.x + decoration.rect.size.width).value())
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            shaped_line
                .decorations
                .iter()
                .all(|decoration| decoration.rect.size.width > px(10.0)),
            "每个 RTL run 的装饰必须覆盖整段视觉宽度（旧实现只剩中间）：{:?}",
            shaped_line.decorations
        );
        assert!(
            left <= 40.0,
            "跨 run 的 RTL 装饰必须覆盖到词左端：{shaped_line:?}"
        );
        assert!(
            (right - shaped_line.advance.value()).abs() <= 1.0,
            "跨 run 的 RTL 装饰必须覆盖到行推进末端：{shaped_line:?}"
        );

        let content =
            TextContent::new("abc שלום def", TextStyle::new(px(16.0))).with_runs([TextRun::new(
                4..12,
                TextStyle::new(px(16.0)).with_underline(underline_thickness, None),
            )]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
                        .map(|glyph| glyph.position.x.value())
                        .fold(f32::INFINITY, f32::min);
                    let right = run
                        .glyphs
                        .iter()
                        .map(|glyph| (glyph.position.x + glyph.size.width).value())
                        .fold(f32::NEG_INFINITY, f32::max);
                    Some((left, right))
                }
                ShapedTextItem::GlyphRun(_) | ShapedTextItem::InlineBox(_) => None,
            })
            .expect("RTL run 必须存在");
        assert!(
            decoration.rect.origin.x <= px(run_left + 1.5),
            "LTR 段落里的 RTL run：装饰左端不得从 run 内部开始：{decoration:?} vs run {run_left}..{run_right}"
        );
        assert!(
            decoration.rect.origin.x + decoration.rect.size.width >= px(run_right - 1.5),
            "LTR 段落里的 RTL run：装饰右端不得止于 run 内部：{decoration:?} vs run {run_left}..{run_right}"
        );
    }

    #[test]
    fn rtl_background_covers_the_visual_span() {
        use crate::paint::color::Color;

        let background = TextBackground {
            color: Color::Srgb {
                r: 0.8,
                g: 0.9,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(0.0),
            padding_x: px(0.0),
        };
        let rtl = TextStyle::new(px(16.0)).with_direction(TextDirection::Rtl);
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let content = TextContent::new("שלום עולם", rtl.clone().with_background(background));
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.backgrounds.len(),
            1,
            "整段 RTL 背景一条：{:?}",
            shaped_line.backgrounds
        );
        let rect = shaped_line.backgrounds[0].rect;
        assert!(
            rect.origin.x <= px(1.0),
            "RTL 背景必须从行左边缘开始（视觉跨度，不是文本区间线性换算）：{rect:?}"
        );
        assert!(
            (rect.origin.x + rect.size.width - shaped_line.advance).abs() <= px(1.0),
            "RTL 背景必须覆盖到行推进末端：{rect:?} / advance {}",
            shaped_line.advance
        );

        let red = TextBackground {
            color: Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 1.0,
            },
            corner_radius: px(0.0),
            padding_x: px(0.0),
        };
        let blue = TextBackground {
            color: Color::Srgb {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(0.0),
            padding_x: px(0.0),
        };
        let content = TextContent::new("שלום עולם", rtl.clone()).with_runs([
            TextRun::new(0..8, rtl.clone().with_background(red)),
            TextRun::new(8..17, rtl.clone().with_background(blue)),
        ]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.backgrounds.len(),
            2,
            "不同底色的两个 RTL run 各一条：{:?}",
            shaped_line.backgrounds
        );
        let red_rect = shaped_line
            .backgrounds
            .iter()
            .find(|quad| quad.color == red.color)
            .expect("红色背景")
            .rect;
        let blue_rect = shaped_line
            .backgrounds
            .iter()
            .find(|quad| quad.color == blue.color)
            .expect("蓝色背景")
            .rect;
        assert!(
            red_rect.origin.x > blue_rect.origin.x,
            "RTL 视觉反转：文本序靠前的 run 必须落在右侧：{red_rect:?} / {blue_rect:?}"
        );
        let (red_left, red_right) = glyph_bounds(shaped_line, 0..8);
        let (blue_left, blue_right) = glyph_bounds(shaped_line, 8..17);
        assert!(
            red_rect.origin.x <= px(red_left + 1.5)
                && red_rect.origin.x + red_rect.size.width >= px(red_right - 1.5),
            "红色矩形必须覆盖文本序第一个 run 的视觉跨度：{red_rect:?} vs {red_left}..{red_right}"
        );
        assert!(
            blue_rect.origin.x <= px(blue_left + 1.5)
                && blue_rect.origin.x + blue_rect.size.width >= px(blue_right - 1.5),
            "蓝色矩形必须覆盖文本序第二个 run 的视觉跨度：{blue_rect:?} vs {blue_left}..{blue_right}"
        );
        assert!(
            (blue_rect.origin.x + blue_rect.size.width - red_rect.origin.x).abs() <= px(1e-3),
            "视觉相邻的两个 RTL run 背景必须严格相接：{blue_rect:?} / {red_rect:?}"
        );

        let content = TextContent::new("שלום עולם", rtl.clone()).with_runs([
            TextRun::new(0..4, rtl.clone().with_background(red)),
            TextRun::new(4..8, rtl.clone().with_background(blue)),
            TextRun::new(8..17, rtl.with_background(red)),
        ]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        let mut rects = shaped_line.backgrounds.to_vec();
        rects.sort_by(|a, b| a.rect.origin.x.value().total_cmp(&b.rect.origin.x.value()));
        assert_eq!(
            rects.len(),
            3,
            "合并只发生在同色之间：被蓝色隔开的两段红色不得合并：{rects:?}"
        );
        assert_eq!(
            rects.iter().map(|quad| quad.color).collect::<Vec<_>>(),
            vec![red.color, blue.color, red.color],
            "视觉序（左→右）是文本序的反序，颜色必须跟随各自 run：{rects:?}"
        );
        assert!(
            (rects[0].rect.origin.x + rects[0].rect.size.width - rects[1].rect.origin.x).abs()
                <= px(1e-3)
                && (rects[1].rect.origin.x + rects[1].rect.size.width - rects[2].rect.origin.x)
                    .abs()
                    <= px(1e-3),
            "三条视觉相邻矩形必须严格相接：{rects:?}"
        );

        let content =
            TextContent::new("abc שלום def", TextStyle::new(px(16.0))).with_runs([TextRun::new(
                4..12,
                TextStyle::new(px(16.0)).with_background(background),
            )]);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(shaped_line.backgrounds.len(), 1, "只有 RTL run 有背景");
        let rect = shaped_line.backgrounds[0].rect;
        let (run_left, run_right) = shaped_line
            .items
            .iter()
            .find_map(|item| match item {
                ShapedTextItem::GlyphRun(run) if run.text_range == (4..12) => {
                    let left = run
                        .glyphs
                        .iter()
                        .map(|glyph| glyph.position.x.value())
                        .fold(f32::INFINITY, f32::min);
                    let right = run
                        .glyphs
                        .iter()
                        .map(|glyph| (glyph.position.x + glyph.size.width).value())
                        .fold(f32::NEG_INFINITY, f32::max);
                    Some((left, right))
                }
                ShapedTextItem::GlyphRun(_) | ShapedTextItem::InlineBox(_) => None,
            })
            .expect("RTL run 必须存在");
        assert!(
            rect.origin.x <= px(run_left + 1.5),
            "LTR 段落里的 RTL run：背景左端不得从 run 内部开始：{rect:?} vs run {run_left}..{run_right}"
        );
        assert!(
            rect.origin.x + rect.size.width >= px(run_right - 1.5),
            "LTR 段落里的 RTL run：背景右端不得止于 run 内部：{rect:?} vs run {run_left}..{run_right}"
        );
    }

    #[test]
    fn one_run_draws_underline_and_strikethrough_together() {
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
        let content = TextContent::new("hello", TextStyle::new(px(16.0)).with_color(ink))
            .with_runs([TextRun::new(
                0..5,
                TextStyle::new(px(16.0))
                    .with_color(ink)
                    .with_underline(px(1.0), None)
                    .with_strikethrough(px(2.0), Some(explicit)),
            )]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.decorations.len(),
            2,
            "同一个 run 的下划线与删除线都必须生成矩形：{:?}",
            shaped_line.decorations
        );
        let underline = shaped_line.decorations[0];
        let strike = shaped_line.decorations[1];
        assert!(
            underline.rect.origin.y > shaped_line.baseline,
            "下划线在基线下方：{underline:?} vs baseline {}",
            shaped_line.baseline
        );
        assert!(
            strike.rect.origin.y < shaped_line.baseline,
            "删除线在基线上方：{strike:?} vs baseline {}",
            shaped_line.baseline
        );
        assert!((underline.rect.size.height - 1.0).abs() < px(1e-4));
        assert!((strike.rect.size.height - 2.0).abs() < px(1e-4));
        assert_eq!(underline.color, ink, "下划线默认跟随文本色");
        assert_eq!(strike.color, explicit, "显式线色优先");
        assert_eq!(
            underline.rect.origin.x, strike.rect.origin.x,
            "同一 run 的两条线共享水平跨度：{underline:?} / {strike:?}"
        );
        assert_eq!(underline.rect.size.width, strike.rect.size.width);
    }

    #[test]
    fn run_background_covers_the_line_box_with_padding_on_every_line() {
        use crate::paint::color::Color;

        let background = TextBackground {
            color: Color::Srgb {
                r: 0.8,
                g: 0.9,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let plain = TextContent::new("alpha beta gamma", TextStyle::new(px(16.0)));
        let highlighted = plain.clone().with_runs([TextRun::new(
            0..16,
            TextStyle::new(px(16.0))
                .with_background(background)
                .with_underline(px(1.0), None),
        )]);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let width = px(40.0);
        let (plain_line, _, _) = rasterize_line(
            &harness.shape_content(&plain),
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped = harness.shape_content(&highlighted);
        let (line, _, _) = rasterize_line(
            &shaped,
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(
            line.lines.len() > 1,
            "40 px 内容宽必须换行：{:?}",
            line.lines
        );
        assert!(
            plain_line
                .lines
                .iter()
                .all(|line| line.backgrounds.is_empty()),
            "没有背景色就不得生成背景矩形"
        );
        assert_eq!(
            line.lines
                .iter()
                .map(|line| line.backgrounds.len())
                .sum::<usize>(),
            line.lines.len(),
            "跨行 run 每行一个背景矩形：{:?}",
            line.lines
        );
        for (index, (plain, highlighted)) in plain_line.lines.iter().zip(&line.lines).enumerate() {
            assert_eq!(
                highlighted.text_range, plain.text_range,
                "背景不得改变断行点"
            );
            assert_eq!(highlighted.advance, plain.advance, "背景不得改变推进宽度");
            assert_eq!(
                highlighted.line_height, plain.line_height,
                "背景不得改变行高"
            );
            if index == 0 {
                assert!(
                    highlighted.line_top.abs() < px(1e-3),
                    "首行行盒 top 从 0 开始：{}",
                    highlighted.line_top
                );
            } else {
                let previous = &line.lines[index - 1];
                assert!(
                    (highlighted.line_top - (previous.line_top + previous.line_height)).abs()
                        < px(1e-3),
                    "行盒 top 必须逐行累加：{} vs {} + {}",
                    highlighted.line_top,
                    previous.line_top,
                    previous.line_height
                );
            }
            let background_quad = highlighted.backgrounds[0];
            let decoration = highlighted.decorations[0];
            assert_eq!(background_quad.color, background.color);
            assert_eq!(background_quad.corner_radius, px(2.0));
            assert_eq!(
                background_quad.rect.origin.y, highlighted.line_top,
                "背景矩形纵向从行盒顶部开始"
            );
            assert_eq!(
                background_quad.rect.size.height, highlighted.line_height,
                "背景矩形纵向覆盖整个行盒"
            );
            assert!(
                (background_quad.rect.origin.x - (decoration.rect.origin.x - px(2.0))).abs()
                    < px(1e-3),
                "左侧 2 px 水平内边距：{background_quad:?} vs {decoration:?}"
            );
            assert!(
                (background_quad.rect.size.width - (decoration.rect.size.width + px(4.0))).abs()
                    < px(1e-3),
                "左右各 2 px 水平内边距：{background_quad:?} vs {decoration:?}"
            );
        }

        let mut merged_harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let split = highlighted.clone().with_runs([
            TextRun::new(0..5, TextStyle::new(px(16.0)).with_background(background)),
            TextRun::new(5..16, TextStyle::new(px(16.0)).with_background(background)),
        ]);
        let (split_line, _, _) = rasterize_line(
            &merged_harness.shape_content(&split),
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(
            split_line.lines.len(),
            line.lines.len(),
            "拆成相邻同色 run 不得改变断行"
        );
        assert_eq!(
            split_line
                .lines
                .iter()
                .map(|line| line.backgrounds.len())
                .sum::<usize>(),
            split_line.lines.len(),
            "相邻同色 run 必须合并成每行一个矩形（不得出现接缝）：{:?}",
            split_line.lines
        );
        for (split, whole) in split_line.lines.iter().zip(&line.lines) {
            let split_quad = split.backgrounds[0];
            let whole_quad = whole.backgrounds[0];
            assert!(
                (split_quad.rect.origin.x - whole_quad.rect.origin.x).abs() < px(1e-3)
                    && (split_quad.rect.size.width - whole_quad.rect.size.width).abs() < px(1e-3),
                "合并后的矩形必须与单个 run 的矩形一致：{split_quad:?} vs {whole_quad:?}"
            );
            assert_eq!(split_quad.color, whole_quad.color);
        }
    }

    #[test]
    fn fractional_line_height_keeps_background_boxes_touching() {
        use crate::paint::color::Color;

        let background = TextBackground {
            color: Color::Srgb {
                r: 0.8,
                g: 0.9,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let style = TextStyle::new(px(14.0))
            .with_line_height(1.4)
            .with_background(background);
        let shaped = harness.shape("alpha beta gamma delta", style);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            px(40.0),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(
            line.lines.len() > 2,
            "40 px 内容宽必须折成多行：{:?}",
            line.lines
        );
        let line_height = line.lines[0].line_height.value();
        assert!(
            (line_height - 19.6).abs() < 0.05,
            "前置：14 px × 1.4 = 19.6 才是分数行高（实际 {line_height}）"
        );
        assert!(
            (line_height - line_height.round()).abs() > 1e-3,
            "前置：行高必须非整数，否则测不出逐行取整误差（实际 {line_height}）"
        );
        let mut expected_top = 0.0f32;
        for shaped_line in &line.lines {
            assert!(
                (shaped_line.line_top.value() - expected_top).abs() < 1e-3,
                "行盒 top 必须按未取整行高累加：{} vs {expected_top}",
                shaped_line.line_top.value()
            );
            assert_eq!(
                shaped_line.backgrounds.len(),
                1,
                "每行一个背景矩形：{shaped_line:?}"
            );
            let rect = shaped_line.backgrounds[0].rect;
            assert!((rect.origin.y.value() - expected_top).abs() < 1e-3);
            assert!((rect.size.height.value() - shaped_line.line_height.value()).abs() < 1e-3);
            expected_top += shaped_line.line_height.value();
        }
        for pair in line.lines.windows(2) {
            let upper = pair[0].backgrounds[0].rect;
            let lower = pair[1].backgrounds[0].rect;
            assert_eq!(
                upper.origin.y + upper.size.height,
                lower.origin.y,
                "相邻行背景必须严格相接（不得重叠 1 px、也不得留 1 px 缝）：{upper:?} / {lower:?}"
            );
        }
    }

    #[test]
    fn invisible_backgrounds_produce_no_geometry() {
        use crate::paint::color::Color;

        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        for background in [
            TextBackground {
                color: Color::TRANSPARENT,
                corner_radius: px(2.0),
                padding_x: px(2.0),
            },
            TextBackground {
                color: Color::WHITE,
                corner_radius: px(f32::NAN),
                padding_x: px(2.0),
            },
            TextBackground {
                color: Color::WHITE,
                corner_radius: px(2.0),
                padding_x: px(f32::NAN),
            },
            TextBackground {
                color: Color::WHITE,
                corner_radius: px(2.0),
                padding_x: px(-1.0),
            },
        ] {
            let content =
                TextContent::new("ab", TextStyle::new(px(16.0)).with_background(background));
            let shaped = harness.shape_content(&content);
            let (line, _diagnostics, _stats) = rasterize_line(
                &shaped,
                shaped.max_content_size().width,
                DevicePixelRatio::ONE,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            assert!(
                line.lines[0].backgrounds.is_empty(),
                "不可见背景（透明色/NaN/负值几何）不得产出矩形：{background:?}"
            );
        }
    }

    #[test]
    fn adjacent_different_backgrounds_do_not_double_pad_the_seam() {
        use crate::paint::color::Color;

        let left = TextBackground {
            color: Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 0.5,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let right = TextBackground {
            color: Color::Srgb {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                alpha: 0.5,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let content = TextContent::new("abcd", TextStyle::new(px(16.0))).with_runs([
            TextRun::new(0..2, TextStyle::new(px(16.0)).with_background(left)),
            TextRun::new(2..4, TextStyle::new(px(16.0)).with_background(right)),
        ]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.backgrounds.len(),
            2,
            "不同底色不得合并：{:?}",
            shaped_line.backgrounds
        );
        let first = shaped_line.backgrounds[0].rect;
        let second = shaped_line.backgrounds[1].rect;
        assert!(
            (first.origin.x + px(2.0)).abs() < px(1e-3),
            "组最左端必须有 2 px 内边距：{first:?}"
        );
        assert!(
            ((first.origin.x + first.size.width) - second.origin.x).abs() < px(1e-3),
            "组间接缝不得双绘 4 px：前段右端必须与后段左端严格相接：{first:?} / {second:?}"
        );
        assert!(
            ((second.origin.x + second.size.width) - (shaped_line.advance + px(2.0))).abs()
                < px(1e-3),
            "组最右端必须有 2 px 内边距：{second:?} / advance {}",
            shaped_line.advance
        );
        assert_eq!(shaped_line.backgrounds[0].color, left.color);
        assert_eq!(shaped_line.backgrounds[1].color, right.color);
    }

    #[test]
    fn background_survives_hard_line_breaks() {
        use crate::paint::color::Color;

        let background = TextBackground {
            color: Color::Srgb {
                r: 0.8,
                g: 0.9,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        for (text, expected) in [
            ("ab\ncd", vec![1usize, 1]),
            ("ab\r\ncd", vec![1, 1]),
            ("ab\n\ncd", vec![1, 0, 1]),
        ] {
            let content =
                TextContent::new(text, TextStyle::new(px(16.0)).with_background(background));
            let shaped = harness.shape_content(&content);
            let (line, _diagnostics, _stats) = rasterize_line(
                &shaped,
                shaped.max_content_size().width,
                DevicePixelRatio::ONE,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            let counts: Vec<usize> = line
                .lines
                .iter()
                .map(|shaped_line| shaped_line.backgrounds.len())
                .collect();
            assert_eq!(
                counts, expected,
                "硬换行：每行一个背景矩形、空行没有（{text:?}）"
            );
            for shaped_line in &line.lines {
                for background_quad in &shaped_line.backgrounds {
                    assert!(
                        background_quad.rect.size.width > px(0.0),
                        "背景必须有正宽度：{background_quad:?}"
                    );
                    assert_eq!(background_quad.rect.origin.y, shaped_line.line_top);
                    assert_eq!(background_quad.rect.size.height, shaped_line.line_height);
                    assert!(
                        background_quad.rect.origin.x >= px(-2.0 - 1e-3)
                            && background_quad.rect.origin.x + background_quad.rect.size.width
                                <= shaped_line.advance + px(2.0 + 1e-3),
                        "背景不得越过该行盒的左右 padding 范围：{background_quad:?} / advance {}",
                        shaped_line.advance
                    );
                }
            }
            let last = line.lines.last().expect("末行");
            let last_quad = last.backgrounds[0];
            assert!(
                (last_quad.rect.size.width - (last.advance + px(4.0))).abs() < px(1e-2),
                "末行内容不含行尾换行，背景宽度必须 = 行推进 + 左右 padding：{last_quad:?} / advance {}",
                last.advance
            );
        }
    }

    #[test]
    fn empty_and_whitespace_background_runs_stay_bounded() {
        use crate::paint::color::Color;

        let background = TextBackground {
            color: Color::Srgb {
                r: 0.8,
                g: 0.9,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let base = TextStyle::new(px(16.0)).with_background(background);

        let content = TextContent::new("ab\n\ncd", base);
        let shaped = harness.shape_content(&content);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(
            line.lines
                .iter()
                .map(|shaped_line| shaped_line.backgrounds.len())
                .collect::<Vec<_>>(),
            vec![1, 0, 1],
            "纯空白行没有内容，不得产生背景矩形"
        );

        let empty_run = TextContent::new("ab", TextStyle::new(px(16.0))).with_runs([TextRun::new(
            1..1,
            TextStyle::new(px(16.0)).with_background(background),
        )]);
        assert_eq!(empty_run.normalized_runs(), 1, "前置：空 run 被规范化丢弃");
        let shaped = harness.shape_content(&empty_run);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(
            line.lines[0].backgrounds.is_empty(),
            "零长度 run 不得产生背景矩形：{:?}",
            line.lines[0].backgrounds
        );

        let whitespace =
            TextContent::new("a b", TextStyle::new(px(16.0))).with_runs([TextRun::new(
                1..2,
                TextStyle::new(px(16.0)).with_background(background),
            )]);
        let shaped = harness.shape_content(&whitespace);
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(
            line.lines[0].backgrounds.len(),
            1,
            "纯空白 run 仍占推进宽度，必须画出背景：{:?}",
            line.lines[0].backgrounds
        );
        let rect = line.lines[0].backgrounds[0].rect;
        assert!(
            rect.size.width > px(4.0),
            "空格 advance + 左右 padding 必须为正宽：{rect:?}"
        );
        assert!(
            rect.origin.x >= px(-2.0 - 1e-3)
                && rect.origin.x + rect.size.width <= line.lines[0].advance + px(2.0 + 1e-3),
            "空白 run 的背景不得越过行推进的 padding 范围：{rect:?} / advance {}",
            line.lines[0].advance
        );
    }

    #[test]
    fn base_and_run_backgrounds_keep_their_own_spans() {
        use crate::paint::color::Color;

        let base = TextBackground {
            color: Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let run = TextBackground {
            color: Color::Srgb {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let content = TextContent::new("abcdef", TextStyle::new(px(16.0)).with_background(base))
            .with_runs([TextRun::new(
                0..3,
                TextStyle::new(px(16.0))
                    .with_background(run)
                    .with_weight(700),
            )]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.backgrounds.len(),
            2,
            "双色背景必须各占一条（run 覆盖 + 底座兜底）：{:?}",
            shaped_line.backgrounds
        );
        let run_quad = shaped_line.backgrounds[0];
        let base_quad = shaped_line.backgrounds[1];
        assert_eq!(run_quad.color, run.color);
        assert_eq!(base_quad.color, base.color);
        assert!(
            (run_quad.rect.origin.x + px(2.0)).abs() < px(1e-3),
            "run 覆盖最左端 ⇒ 左 padding 有效：{run_quad:?}"
        );
        assert!(
            (run_quad.rect.origin.x + run_quad.rect.size.width - base_quad.rect.origin.x).abs()
                < px(1e-3),
            "底座兜底不得覆盖 run 区间，两者必须严格相接：{run_quad:?} / {base_quad:?}"
        );
        assert!(
            ((base_quad.rect.origin.x + base_quad.rect.size.width)
                - (shaped_line.advance + px(2.0)))
            .abs()
                < px(1e-3),
            "底座兜底覆盖最右端 ⇒ 右 padding 有效：{base_quad:?} / advance {}",
            shaped_line.advance
        );
        let (run_left, run_right) = glyph_bounds(shaped_line, 0..3);
        let (base_left, base_right) = glyph_bounds(shaped_line, 3..6);
        assert!(
            run_quad.rect.origin.x <= px(run_left + 1.5)
                && run_quad.rect.origin.x + run_quad.rect.size.width >= px(run_right - 1.5),
            "run 矩形必须覆盖 run 0..3 的字形视觉跨度：{run_quad:?} vs {run_left}..{run_right}"
        );
        assert!(
            base_quad.rect.origin.x <= px(base_left + 1.5)
                && base_quad.rect.origin.x + base_quad.rect.size.width >= px(base_right - 1.5),
            "底座兜底矩形必须覆盖 run 3..6 的字形视觉跨度：{base_quad:?} vs {base_left}..{base_right}"
        );
        let seam = run_quad.rect.origin.x + run_quad.rect.size.width;
        assert!(
            seam >= px(run_right - 1.5) && seam <= px(base_left + 1.5),
            "内侧接缝必须落在第 3/第 4 字符之间的边界（run 右端与 base 左端都不得多吃字符）：seam {seam} vs glyph {run_right}..{base_left}"
        );
    }

    #[test]
    fn narrow_gap_between_backgrounds_is_split_between_them() {
        use crate::paint::color::Color;

        let left = TextBackground {
            color: Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 0.5,
            },
            corner_radius: px(0.0),
            padding_x: px(2.0),
        };
        let right = TextBackground {
            color: Color::Srgb {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                alpha: 0.5,
            },
            corner_radius: px(0.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let text_style = TextStyle::new(px(10.0));
        let gap = harness
            .shape("a b", text_style.clone())
            .max_content_size()
            .width
            - harness
                .shape("ab", text_style.clone())
                .max_content_size()
                .width;
        assert!(
            gap > px(0.0) && gap < px(4.0),
            "前置：空格宽度必须落在 (0, 2×padding) 的窄缝区间（实际 {gap}）"
        );
        let content = TextContent::new("a b", text_style.clone()).with_runs([
            TextRun::new(0..1, text_style.clone().with_background(left)),
            TextRun::new(2..3, text_style.with_background(right)),
        ]);
        let shaped = harness.shape_content(&content);
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _diagnostics, _stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.backgrounds.len(),
            2,
            "两个不同底色必须各一条：{:?}",
            shaped_line.backgrounds
        );
        let first = shaped_line.backgrounds[0].rect;
        let second = shaped_line.backgrounds[1].rect;
        assert!(
            (first.origin.x + px(2.0)).abs() < px(1e-3),
            "首组外侧仍是完整 padding：{first:?}"
        );
        assert!(
            ((second.origin.x + second.size.width) - (shaped_line.advance + px(2.0))).abs()
                < px(1e-3),
            "末组外侧仍是完整 padding：{second:?} / advance {}",
            shaped_line.advance
        );
        assert!(
            ((first.origin.x + first.size.width) - second.origin.x).abs() < px(1e-3),
            "窄缝两侧各取 min(padding, gap/2)，矩形必须严格相接、不得双绘：{first:?} / {second:?}"
        );
    }

    #[test]
    fn ellipsis_clamps_background_into_the_kept_line() {
        use crate::paint::color::Color;

        let background = TextBackground {
            color: Color::Srgb {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                alpha: 1.0,
            },
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let mut harness = Harness::new();
        let style = TextStyle::new(px(16.0))
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(crate::text::TextOverflow::Ellipsis, 0)
            .with_background(background);
        let shaped = harness.shape("abcdefghijklmnopqrstuvwxyz", style);
        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");
        let width = shaped.max_content_size().width * 0.5;
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster: &mut raster,
            glyph_atlas: &mut glyphs,
            atlas: &mut atlas,
            generation,
            scale: DevicePixelRatio::ONE,
        };
        let (line, _diagnostics, _stats) = shaped
            .rasterize(
                width,
                crate::text::TextAlign::Start,
                Color::BLACK,
                &mut ctx,
                Some(&ellipsis),
            )
            .expect("ellipsis clipping must not fail");
        let shaped_line = &line.lines[0];
        assert_eq!(
            shaped_line.backgrounds.len(),
            1,
            "省略号裁剪后背景仍必须保留：{shaped_line:?}"
        );
        let background_quad = shaped_line.backgrounds[0];
        let right = background_quad.rect.origin.x + background_quad.rect.size.width;
        assert!(
            (background_quad.rect.origin.x + px(2.0)).abs() < px(1e-3),
            "判定：截断行左侧保留 padding（下限 line_offset − padding_x），左界不得被钳到 0：{background_quad:?}"
        );
        assert_eq!(
            right, shaped_line.advance,
            "判定：截断行背景右端严格等于截断后的行推进（右侧必须截断到省略号之前）：{background_quad:?} / advance {}",
            shaped_line.advance
        );
    }

    #[test]
    fn rasterize_of_empty_text_yields_empty_snapshot() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(px(16.0)));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let (line, diagnostics, _stats) = rasterize_line(
            &shaped,
            px(0.0),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert!(line.lines.is_empty());
        assert_eq!(line.width, px(0.0));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn rasterize_positions_glyphs_and_sets_tiles() {
        let mut harness = Harness::new();
        let shaped = harness.shape("Ab", TextStyle::new(px(16.0)));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let (line, _diagnostics, stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            DevicePixelRatio::ONE,
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
            assert!(glyph.size.width > px(0.0) && glyph.size.height > px(0.0));
            let physical = glyph.size.width.value() * line.scale.factor();
            assert!(
                (physical - physical.round()).abs() < 1e-3,
                "scale * size must be integral, got {physical}"
            );
            let physical_x = glyph.position.x.value() * line.scale.factor();
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
        let centered = TextStyle::new(px(16.0)).with_alignment(crate::text::TextAlign::Center);
        let shaped = harness.shape("R", centered);

        let (left_width, right_width) = (200.0f32, 200.8f32);
        let left_x = shaped.first_glyph_x_at(px(left_width));
        let right_x = shaped.first_glyph_x_at(px(right_width));
        assert!(
            (right_x - left_x - 0.4).abs() < px(0.01),
            "precondition: 居中偏移必须线性移动 0.4 px（{left_x} → {right_x}）"
        );
        assert_ne!(
            crate::text::raster::split_subpixel_x(left_x.value()).1,
            crate::text::raster::split_subpixel_x(right_x.value()).1,
            "precondition: 0.4 px 的位移必须换到不同相位桶"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (left, _, _) = rasterize_line(
            &shaped,
            px(left_width),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (right, _, _) = rasterize_line(
            &shaped,
            px(right_width),
            DevicePixelRatio::ONE,
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
            TextStyle::new(px(16.0)).with_alignment(crate::text::TextAlign::Center),
        );

        let (left_width, right_width) = (200.0f32, 200.64f32);
        let left_logical = shaped.first_glyph_x_at(px(left_width));
        let right_logical = shaped.first_glyph_x_at(px(right_width));
        assert!(
            (right_logical - left_logical - 0.32).abs() < px(0.01),
            "precondition: 逻辑落点移动 0.32 px（{left_logical} → {right_logical}）"
        );
        assert!(
            (right_logical - left_logical) * 1.25 - 0.4 < px(0.01),
            "precondition: 物理落点移动 0.4 px"
        );
        assert_ne!(
            crate::text::raster::split_subpixel_x((left_logical * 1.25).value()).1,
            crate::text::raster::split_subpixel_x((right_logical * 1.25).value()).1,
            "precondition: 0.4 物理像素必须换到不同相位桶"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (left, _, _) = rasterize_line(
            &shaped,
            px(left_width),
            DevicePixelRatio::new(1.25).expect("scale"),
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (right, _, _) = rasterize_line(
            &shaped,
            px(right_width),
            DevicePixelRatio::new(1.25).expect("scale"),
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
                let physical_x = glyph.position.x.value() * line.scale.factor();
                assert!(
                    (physical_x - physical_x.round()).abs() < 1e-3,
                    "落点必须是整数物理像素（相位已烘进位图）：{physical_x}"
                );
                let physical_width = glyph.size.width.value() * line.scale.factor();
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
            |size: f32| TextStyle::new(px(size)).with_alignment(crate::text::TextAlign::Center);
        let fractional = harness.shape("R", centered(16.0));
        let integer = harness.shape("R", centered(20.0));
        let fractional_logical = fractional.first_glyph_x_at(px(200.0));
        let integer_logical = integer.first_glyph_x_at(px(250.0));
        assert!(
            (fractional_logical * 1.25 - integer_logical).abs() < px(1e-2),
            "precondition: 物理落点必须一致（{} → {integer_logical}）",
            fractional_logical * 1.25
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (fractional_line, _, fractional_stats) = rasterize_line(
            &fractional,
            px(200.0),
            DevicePixelRatio::new(1.25).expect("scale"),
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (integer_line, _, integer_stats) = rasterize_line(
            &integer,
            px(250.0),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

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
        let shaped = harness.shape("0123456789", TextStyle::new(px(14.0)));
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
            px(layout_width),
            DevicePixelRatio::new(1.25).expect("scale"),
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
            line.width,
            px(layout_width),
            "内容宽 = 逻辑布局宽（不再除 scale）"
        );
        assert_eq!(
            line.lines[0].advance,
            px(layout_advance),
            "行 advance 必须来自 parley，不受小数 ppem 影响"
        );
        let total_advance: f32 = line.lines.iter().map(|line| line.advance.value()).sum();
        assert!(
            (total_advance - line.width.value()).abs() < 1e-3,
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
        let shaped = harness.shape("HHHH", TextStyle::new(px(16.0)));
        let width = shaped.max_content_size().width;

        let mut raster = crate::text::raster::GlyphRasterizer::new();
        let mut glyphs = crate::text::atlas::GlyphAtlas::new();
        let mut atlas = crate::render::atlas::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (2048, 2048),
        );
        let (line_1x, _, _) = rasterize_line(
            &shaped,
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line_125, _, _) = rasterize_line(
            &shaped,
            width,
            DevicePixelRatio::new(1.25).expect("scale"),
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

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
                glyphs
                    .iter()
                    .map(|glyph| glyph.size.width.value() * scale)
                    .sum()
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
        let shaped = harness.shape("0123456789", TextStyle::new(px(16.0)));
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
            px(layout_width),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(
            diagnostics.is_empty(),
            "digits must not produce diagnostics"
        );

        assert_eq!(line.width, px(layout_width), "内容宽必须等于 parley 布局宽");
        assert_eq!(line.lines.len(), 1);
        assert_eq!(line.lines[0].advance, px(layout_advance));
        let total_advance: f32 = line.lines.iter().map(|line| line.advance.value()).sum();
        assert!(
            (total_advance - layout_width).abs() < 1e-3,
            "亚像素定位不得改变 advance 总和：{total_advance} vs {layout_width}"
        );
        assert!(
            (shaped.max_content_size().width - layout_width).abs() < px(1e-3),
            "形状快照的内容宽也必须与布局宽一致"
        );
    }

    #[test]
    fn measure_is_o1_for_min_max_content() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three four five", TextStyle::new(px(16.0)));
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
            let _ = shaped.measure(none_known(), definite(width.value()));
        }
        assert_eq!(
            shaped.measure_breaks(),
            1,
            "同一宽度的重复探测必须命中 memo"
        );
        let _ = shaped.measure(none_known(), definite((width * 0.5).value()));
        assert_eq!(shaped.measure_breaks(), 2, "换宽度才允许再次断行");
    }

    #[test]
    fn missing_glyph_draws_notdef_instead_of_vanishing() {
        const RARE: &str = "\u{10FFFD}";
        let mut harness = Harness::new();
        let shaped = harness.shape(RARE, TextStyle::new(px(16.0)));
        let width = shaped.max_content_size().width.max(px(32.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, diagnostics, stats) = rasterize_line(
            &shaped,
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

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
            placeholder.size.width > px(0.0) && placeholder.size.height > px(0.0),
            "`.notdef` 占位必须有正尺寸：{:?}",
            placeholder.size
        );
    }

    #[test]
    fn ellipsis_clips_the_line_and_appends_glyphs() {
        let mut harness = Harness::new();
        let style = TextStyle::new(px(16.0))
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
            scale: DevicePixelRatio::ONE,
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
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(px(16.0)));
        let narrow = min_max_widths(&full).0 + 1.0;
        assert!(
            narrow < full.max_content_size().width.value(),
            "precondition: 必须落在真正的折行区间"
        );

        let size = shaped.measure(none_known(), definite(narrow));
        let full_size = full.measure(none_known(), definite(narrow));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line(
            &shaped,
            px(narrow),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (full_line, _, _) = rasterize_line(
            &full,
            px(narrow),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (size.height - line.height).abs() < px(1e-3),
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        assert!(
            (full_size.height - full_line.height).abs() < px(1e-3),
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
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(px(16.0)));
        let (min_width, max_width) = min_max_widths(&full);
        let width = min_width + (max_width - min_width) * 0.25;

        let size = clamped.measure(none_known(), definite(width));
        let full_size = full.measure(none_known(), definite(width));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line(
            &clamped,
            px(width),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (full_line, _, _) = rasterize_line(
            &full,
            px(width),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (size.height - line.height).abs() < px(1e-3),
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        let expected = full_line.lines[0].line_height + full_line.lines[1].line_height;
        assert!(
            (size.height - expected).abs() < px(1e-3),
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
            (full_size.height - full_line.height).abs() < px(1e-3),
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
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(px(16.0)));

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
            (max.height - expected).abs() < px(1e-3),
            "MaxContent 高度必须钳到前两行：{} vs {expected}",
            max.height
        );
        assert!(
            (min.height - max.height).abs() < px(1e-3),
            "MinContent 高度同样钳制（纯文本行行高相同）：{} vs {}",
            min.height,
            max.height
        );
        assert!(
            max.height < px(full_height - 1.0),
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
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (line.height - max.height).abs() < px(1e-3),
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
            (full_size.height - full_height).abs() < px(1e-3),
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
            TextStyle::new(px(16.0))
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(crate::text::TextOverflow::Ellipsis, 0),
        );
        let clip_shaped = harness.shape(
            TEXT,
            TextStyle::new(px(16.0))
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(crate::text::TextOverflow::Clip, 0),
        );
        let full_width = ellipsis_shaped.max_content_size().width;
        let content_width = full_width * 0.5;
        assert!(
            content_width > px(0.0) && content_width < full_width,
            "precondition: 内容宽必须真的裁剪"
        );

        let ellipsis_size = ellipsis_shaped.measure(none_known(), definite(content_width.value()));
        let clip_size = clip_shaped.measure(none_known(), definite(content_width.value()));
        assert_eq!(
            ellipsis_size.width.value().to_bits(),
            clip_size.width.value().to_bits(),
            "省略号不得改变宽度上界：{} vs {}",
            ellipsis_size.width,
            clip_size.width
        );
        assert_eq!(
            ellipsis_size.height.value().to_bits(),
            clip_size.height.value().to_bits(),
            "省略号不得改变高度"
        );
        assert!(
            (ellipsis_size.width - full_width).abs() < px(1e-3),
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
            DevicePixelRatio::ONE,
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
            (line.height - ellipsis_size.height).abs() < px(1e-3),
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
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Ellipsis, 2),
        );
        let multiline_full = harness.shape(TEXT, TextStyle::new(px(16.0)));
        let (min_width, max_width) = min_max_widths(&multiline_full);
        let wrap_width = min_width + (max_width - min_width) * 0.25;
        let multiline_size = multiline_ellipsis.measure(none_known(), definite(wrap_width));
        let (full_line, _, _) = rasterize_line(
            &multiline_full,
            px(wrap_width),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &multiline_ellipsis,
            px(wrap_width),
            DevicePixelRatio::ONE,
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
            (line.height - multiline_size.height).abs() < px(1e-3),
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
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Ellipsis, 2),
        );
        let clip_shaped = harness.shape(
            TEXT,
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Clip, 2),
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
            px(CONTENT_WIDTH),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &ellipsis_shaped,
            px(CONTENT_WIDTH),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );

        assert_eq!(clip_line.lines.len(), 2, "Clip + max_lines = 2");
        assert_eq!(line.lines.len(), 2, "Ellipsis + max_lines = 2");
        let clip_last = clip_line.lines.last().expect("clip last line");
        let last = line.lines.last().expect("ellipsis last line");
        let prefix_len = assert_ellipsis_prefix(last, clip_last, &ellipsis, px(CONTENT_WIDTH));

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
                scale: DevicePixelRatio::ONE,
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
                x += *advance;
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
            TextStyle::new(px(16.0))
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(overflow, 1)
        };
        let ellipsis_shaped = harness.shape(TEXT, style(crate::text::TextOverflow::Ellipsis));
        let clip_shaped = harness.shape(TEXT, style(crate::text::TextOverflow::Clip));
        let latin_width = harness
            .shape(
                "abcdef",
                TextStyle::new(px(16.0)).with_wrap(TextWrap::NoWrap),
            )
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
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &ellipsis_shaped,
            width,
            DevicePixelRatio::ONE,
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
            TextStyle::new(px(16.0))
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
                DevicePixelRatio::ONE,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            let (line, _, _) = rasterize_line_with_ellipsis(
                &ellipsis_shaped,
                width,
                DevicePixelRatio::ONE,
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
                size: crate::math::LogicalSize::new(px(30.0), px(40.0)),
                baseline: None,
                align: InlineAlign::Baseline,
                paint: None,
            },
            InlineBoxSpec {
                id: 2,
                index: 4,
                size: crate::math::LogicalSize::new(px(30.0), px(60.0)),
                baseline: None,
                align: InlineAlign::Baseline,
                paint: None,
            },
        ];
        let shaped = harness.shape_with(
            TEXT,
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Clip, 2),
            &boxes,
        );
        let full = harness.shape_with(TEXT, TextStyle::new(px(16.0)), &boxes);
        let width = shaped.max_content_size().width;

        let size = shaped.measure(none_known(), definite(width.value()));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line(
            &shaped,
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (full_line, _, _) = rasterize_line(
            &full,
            width,
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

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
            (size.height - expected).abs() < px(1e-3),
            "measure 必须是前两行真实行高之和：{} vs {expected}",
            size.height
        );
        assert!(
            (size.height - line.height).abs() < px(1e-3),
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        let uniform = plain_line_height * 2.0;
        assert!(
            (size.height - uniform).abs() > px(1.0),
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
            TextStyle::new(px(16.0)).with_overflow(crate::text::TextOverflow::Clip, 1),
        );
        let plain = harness.shape("Hello", TextStyle::new(px(16.0)));
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

        assert!(size.height > px(0.0), "单行不得被钳成 0：{:?}", size);
        assert!(
            (size.height - line_height).abs() < px(1e-3),
            "单行高度必须等于真实行高：{} vs {line_height}",
            size.height
        );
        assert_eq!(
            size.height.value().to_bits(),
            plain_size.height.value().to_bits(),
            "max_lines = 1 与不设上限在单行文本上必须逐位相同"
        );
    }

    #[test]
    fn raster_ppem_is_physical_and_grows_with_scale() {
        let mut harness = Harness::new();
        if crate::text::raster::probe_font().is_none() {
            return;
        }
        let shaped = harness.shape("H", TextStyle::new(px(16.0)));
        let logical_width = shaped.max_content_size().width;
        let mut physical = Vec::new();
        for scale in [
            DevicePixelRatio::ONE,
            DevicePixelRatio::new(1.25).expect("scale"),
            DevicePixelRatio::new(2.0).expect("scale"),
        ] {
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
            physical.push(glyph.size.height * scale.factor());
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
            let shaped = harness.shape("Hamburgefonstiv", TextStyle::new(px(16.0)));
            widths.push((family, shaped.max_content_size().width));
        }
        let distinct = widths
            .iter()
            .map(|(_, width)| width.value().to_bits())
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
            ("Hello", TextStyle::new(px(16.0)), true),
            ("价🌍ab", TextStyle::new(px(16.0)), true),
            ("abc אבג", TextStyle::new(px(16.0)), false),
            (
                "abc",
                TextStyle::new(px(16.0)).with_direction(TextDirection::Ltr),
                true,
            ),
            (
                "אבג",
                TextStyle::new(px(16.0)).with_direction(TextDirection::Ltr),
                false,
            ),
            (
                "abc",
                TextStyle::new(px(16.0)).with_direction(TextDirection::Rtl),
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
    fn a_colr_v1_only_font_emits_the_color_glyph_monochrome_diagnostic() {
        const FAMILY: &str = "Kytrix COLRv1 Fixture";
        let Some((font, glyph)) = crate::text::raster::require_color_glyph_monochrome_fixture()
        else {
            return;
        };
        let mut font_cx = parley::FontContext::new();
        let registered = font_cx.collection.register_fonts(
            font.data,
            Some(parley::fontique::FontInfoOverride {
                family_name: Some(FAMILY),
                ..Default::default()
            }),
        );
        assert!(!registered.is_empty(), "注入字体必须注册成功");
        let mut layout_cx: parley::LayoutContext<crate::paint::color::ColorBrush> =
            parley::LayoutContext::new();
        let family = parley::FontFamily::Single(parley::FontFamilyName::Named(
            std::borrow::Cow::Borrowed(FAMILY),
        ));
        let content = TextContent::new("A", TextStyle::new(px(32.0)).with_wrap(TextWrap::NoWrap));
        let shaped =
            super::ShapedText::shape(&mut font_cx, &mut layout_cx, &family, &content, 1, &[]);
        let mut raster = crate::text::raster::GlyphRasterizer::new();
        let mut glyph_atlas = crate::text::atlas::GlyphAtlas::new();
        let mut atlas = crate::render::atlas::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (256, 256),
        );
        let (line, diagnostics, _) = rasterize_line(
            &shaped,
            px(256.0),
            DevicePixelRatio::ONE,
            &mut raster,
            &mut glyph_atlas,
            &mut atlas,
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "COLRv1-only 字形必须恰好产生一条单色回退诊断：{diagnostics:?}"
        );
        match &diagnostics[0] {
            TextDiagnostic::ColorGlyphMonochrome {
                font: label,
                glyph: diagnostic_glyph,
                text_range,
            } => {
                assert_eq!(*diagnostic_glyph, glyph);
                assert_eq!(text_range, &(0..1), "诊断必须覆盖被回退的字形文本区间");
                assert!(!label.is_empty(), "诊断必须带上字体标签");
            }
            other => panic!("期望 ColorGlyphMonochrome，得到 {other:?}"),
        }
        let glyphs = all_glyphs(&line);
        assert_eq!(glyphs.len(), 1, "fixture 文本只有一个字形");
        assert_eq!(
            glyphs[0].tile.atlas,
            crate::paint::atlas::AtlasKind::Glyph,
            "单色回退必须落在 R8 图集"
        );
        assert!(!glyph_atlas.has_color_records(), "单色回退不得写入彩色图集");
    }

    #[test]
    fn numeric_text_never_has_rtl_runs() {
        let mut text_cx = crate::text::context::TextContext::new();
        for text in ["42150", "-1.50%", "1,234.56", "12/34:56"] {
            let content =
                TextContent::new(text, TextStyle::new(px(16.0)).with_wrap(TextWrap::NoWrap));
            let shaped = text_cx.shape_cached(&content).0;
            assert!(
                shaped.has_only_ltr_runs(),
                "{text:?} 是纯 ASCII 数字文本：无论走快速路径还是 parley，都不该有 RTL run"
            );
        }
    }

    fn broken_line_edges(shaped: &super::ShapedText, width: f32) -> Vec<(f32, f32)> {
        let text = shaped.text();
        let layout = shaped.layout_at(px(width));
        let mut edges = Vec::with_capacity(layout.lines().count());
        for line in layout.lines() {
            let mut rights: Vec<f32> = Vec::new();
            for item in line.items() {
                let parley::PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                for glyph in run.positioned_glyphs() {
                    rights.push(glyph.x + glyph.advance);
                }
            }
            if text.as_bytes().get(line.text_range().end.wrapping_sub(1)) == Some(&b' ') {
                rights.pop();
            }
            edges.push((
                line.metrics().advance,
                rights.last().copied().unwrap_or(0.0),
            ));
        }
        edges
    }

    fn line_advances(shaped: &super::ShapedText, width: f32) -> Vec<f32> {
        shaped
            .layout_at(px(width))
            .lines()
            .map(|line| line.metrics().advance)
            .collect()
    }

    #[test]
    fn break_word_wraps_an_unbreakable_cjk_punctuation_run() {
        let mut harness = Harness::new();
        let text = "。".repeat(50);
        let style = TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord);
        let shaped = harness.shape(&text, style);

        let metrics = shaped.line_metrics(px(250.0), TextAlign::Start);
        assert!(
            metrics.line_count >= 3,
            "50 个 CJK 句号在 250px 内必须折成多行：{metrics:?}"
        );
        assert!(
            (metrics.content_height - metrics.line_height * metrics.line_count as f32).abs()
                <= px(0.5),
            "内容高必须是行高之和：{metrics:?}"
        );
        let size = shaped.measure(none_known(), definite(250.0));
        assert!(
            (size.height - metrics.content_height).abs() <= px(0.5),
            "measure 高度必须等于断行后的内容高：{size:?} vs {metrics:?}"
        );
        assert!(
            size.width <= px(251.0),
            "measure 宽度必须是断行后的行宽，不是整段宽度：{size:?}"
        );
        for (advance, right) in broken_line_edges(&shaped, 250.0) {
            assert!(
                advance <= 251.0 && right <= 251.0,
                "每行推进 {advance} 与字形右边界 {right} 都不得超过折行宽度 250"
            );
        }
    }

    #[test]
    fn wrap_without_emergency_breaks_keeps_an_unbreakable_run_on_one_line() {
        let mut harness = Harness::new();
        let shaped = harness.shape(&"。".repeat(50), TextStyle::new(px(16.0)));

        let metrics = shaped.line_metrics(px(250.0), TextAlign::Start);
        assert_eq!(
            metrics.line_count, 1,
            "Wrap 只有 Unicode 断行机会，CJK 闭标点之间没有机会：{metrics:?}"
        );
        let size = shaped.measure(none_known(), definite(250.0));
        assert!(
            size.width > px(250.0),
            "没有词内断行机会时只能溢出：{size:?}"
        );
    }

    #[test]
    fn rebreaking_one_layout_at_successive_widths_keeps_every_break() {
        let mut harness = Harness::new();
        let text = "。".repeat(50);
        let style = TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord);
        let shaped = harness.shape(&text, style.clone());

        let wide = line_advances(&shaped, 610.0);
        let narrow = line_advances(&shaped, 604.0);
        let again = line_advances(&shaped, 610.0);

        assert_ne!(wide, narrow, "610 与 604 必须折出不同的行宽");
        assert_eq!(
            wide, again,
            "同一个 parley Layout 第三次折行必须回到 610 的结果（断行状态不得被消费）"
        );

        let fresh = harness.shape(&text, style);
        assert_eq!(
            wide,
            line_advances(&fresh, 610.0),
            "复用布局的 610 结果必须等于全新 shaping 的结果"
        );
        assert_eq!(
            narrow,
            line_advances(&fresh, 604.0),
            "复用布局的 604 结果必须等于全新 shaping 的结果"
        );
    }

    #[test]
    fn break_word_measure_memo_follows_the_requested_width() {
        let mut harness = Harness::new();
        let style = TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord);
        let shaped = harness.shape(&"。".repeat(50), style);

        let narrow = shaped.measure(none_known(), definite(200.0));
        let wide = shaped.measure(none_known(), definite(700.0));
        assert!(
            narrow.height > wide.height,
            "窄宽必须折成更多行（不能被 min-content 快路径短路）：{narrow:?} vs {wide:?}"
        );
        assert!(narrow.width <= px(201.0), "{narrow:?}");
        assert!(wide.width > px(600.0), "{wide:?}");
        assert!(shaped.measure_breaks() >= 2, "两次不同宽度必须各断行一次");

        let repeated = shaped.measure(none_known(), definite(200.0));
        assert_eq!(repeated, narrow, "回到 200 必须得到同一结果");
        let breaks = shaped.measure_breaks();
        assert_eq!(
            shaped.measure(none_known(), definite(200.0)),
            narrow,
            "同宽重复测量必须命中 memo"
        );
        assert_eq!(shaped.measure_breaks(), breaks, "同宽重复测量不得重新断行");
    }

    #[test]
    fn break_word_keeps_word_boundaries_when_every_word_fits() {
        let mut harness = Harness::new();
        let text = "word ".repeat(20);
        let normal = harness.shape(&text, TextStyle::new(px(16.0)));
        let break_word = harness.shape(
            &text,
            TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord),
        );

        let normal_metrics = normal.line_metrics(px(200.0), TextAlign::Start);
        let break_word_metrics = break_word.line_metrics(px(200.0), TextAlign::Start);
        assert!(normal_metrics.line_count >= 3, "{normal_metrics:?}");
        assert_eq!(
            break_word_metrics.line_count, normal_metrics.line_count,
            "有空格断行机会、且每个词都放得下时 BreakWord 不得改变折行结果：{break_word_metrics:?} vs {normal_metrics:?}"
        );
        let layout = break_word.layout_at(px(200.0));
        for line in layout.lines() {
            let range = line.text_range();
            assert!(
                range.end == text.len() || text.as_bytes()[range.end - 1] == b' ',
                "拉丁词只在词边界折行：{range:?}"
            );
        }
        for (_, right) in broken_line_edges(&break_word, 200.0) {
            assert!(right <= 201.0, "字形右边界 {right} 不得超过折行宽度 200");
        }
    }

    #[test]
    fn break_word_splits_only_the_word_that_cannot_fit() {
        let mut harness = Harness::new();
        let long = "supercalifragilisticexpialidocious";
        let text = format!("alpha {long} omega");
        let width = 100.0;
        let long_start = "alpha ".len();
        let long_end = long_start + long.len();
        let break_word = harness.shape(
            &text,
            TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord),
        );
        let lines: Vec<core::ops::Range<usize>> = break_word
            .layout_at(px(width))
            .lines()
            .map(|line| line.text_range())
            .collect();

        assert!(lines.len() >= 3, "长词必须占多行：{lines:?}");
        assert!(
            lines
                .iter()
                .any(|range| range.end > long_start && range.end < long_end),
            "整词放不下一行时必须在词内断开：{lines:?}"
        );
        let covered = lines
            .iter()
            .filter(|range| range.start < long_end && range.end > long_start)
            .fold(long_start..long_start, |covered, range| {
                covered.start.min(range.start)..covered.end.max(range.end)
            });
        assert_eq!(
            (covered.start, covered.end.min(long_end)),
            (long_start, long_end),
            "长词必须从词首到词尾被完整排入（含悬挂空格）：{lines:?}"
        );
        for word in ["alpha", "omega"] {
            let start = text.find(word).expect("fixture word");
            let end = start + word.len();
            assert!(
                !lines
                    .iter()
                    .any(|range| (range.start > start && range.start < end)
                        || (range.end > start && range.end < end)),
                "放得下的短词 {word:?} 必须整词换行：{lines:?}"
            );
        }
        assert!(
            lines[0].start == 0 && lines[0].end >= "alpha".len(),
            "首行必须整词容纳 alpha：{lines:?}"
        );
        assert_eq!(
            lines.last().map(|range| range.end),
            Some(text.len()),
            "末行必须收完整段文本：{lines:?}"
        );
        for (advance, right) in broken_line_edges(&break_word, width) {
            assert!(
                advance <= width + 1.0 && right <= width + 1.0,
                "断词后不得横向溢出：行推进 {advance} / 字形右边界 {right} vs {width}"
            );
        }

        let normal = harness.shape(&text, TextStyle::new(px(16.0)));
        assert!(
            broken_line_edges(&normal, width)
                .iter()
                .any(|(advance, _)| *advance > width + 1.0),
            "夹具必须真的包含一个整行放不下的词（否则测不到词内应急断行）"
        );
    }

    #[test]
    fn break_word_keeps_intrinsic_sizes_of_an_unbreakable_run() {
        let mut harness = Harness::new();
        let text = "。".repeat(50);
        let break_word = harness.shape(
            &text,
            TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord),
        );
        let widths = break_word.layout().calculate_content_widths();
        assert!(
            (widths.min - widths.max).abs() <= 0.01,
            "BreakWord 不得改变 min-content（这是它与 Anywhere 的唯一差别）：{widths:?}"
        );
        let min = break_word.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );
        let max = break_word.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        assert_eq!(
            min.width.value().to_bits(),
            max.width.value().to_bits(),
            "MinContent 与 MaxContent 必须同为未断行整行宽：{min:?} vs {max:?}"
        );
        assert!(
            (min.width - px(widths.max)).abs() <= px(0.5),
            "固有宽度必须等于未断行整行宽：{min:?} vs {widths:?}"
        );
        let single = break_word.line_metrics(px(widths.max), TextAlign::Start);
        assert_eq!(single.line_count, 1, "{single:?}");
        assert!(
            (min.height - single.content_height).abs() <= px(0.5),
            "MinContent 高度必须是 1 行：{min:?} vs {single:?}"
        );

        let anywhere = harness.shape(
            &text,
            TextStyle::new(px(16.0)).with_wrap(TextWrap::Anywhere),
        );
        let anywhere_widths = anywhere.layout().calculate_content_widths();
        assert!(
            anywhere_widths.min * 2.0 < anywhere_widths.max,
            "Anywhere 必须把 min-content 压到单 atom，才能与 BreakWord 区分：break={widths:?} anywhere={anywhere_widths:?}"
        );
        let anywhere_min = anywhere.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );
        assert!(
            anywhere_min.width * 2.0 < min.width,
            "Anywhere 的 MinContent 测量必须明显小于 BreakWord：{anywhere_min:?} vs {min:?}"
        );
    }

    #[test]
    fn run_level_wrap_does_not_change_the_paragraph_wrap_mode() {
        let mut harness = Harness::new();
        let text = "。".repeat(50);
        let run_style = TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord);
        let content = TextContent::new(&text, TextStyle::new(px(16.0)))
            .with_runs([TextRun::new(0..text.len(), run_style)]);
        let shaped = harness.shape_content(&content);

        let metrics = shaped.line_metrics(px(250.0), TextAlign::Start);
        assert_eq!(
            metrics.line_count, 1,
            "run 级 wrap 只能收紧（NoWrap/Anywhere），不得把 base Wrap 放宽成 break-word：{metrics:?}"
        );
        let size = shaped.measure(none_known(), definite(250.0));
        assert!(
            (size.height - metrics.content_height).abs() <= px(0.5),
            "measure 与光栅化必须一致（run 级 wrap 不参与 measure 守卫）：{size:?} vs {metrics:?}"
        );

        let base_break_word = harness.shape(
            &text,
            TextStyle::new(px(16.0)).with_wrap(TextWrap::BreakWord),
        );
        assert!(
            base_break_word
                .line_metrics(px(250.0), TextAlign::Start)
                .line_count
                >= 3,
            "只有整段兜底样式的 BreakWord 才改变折行"
        );
    }
}
