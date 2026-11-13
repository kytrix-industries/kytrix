//! 形状：`parley` 的唯一调用点（`design/phase-05-text.md` §7）。
//!
//! [`ShapedText`] 是一次 shape 的**不可变快照**（parley `Layout` + 内容宽），被 `Rc` 共享：
//! - measure：`clone()` 后 `break_all_lines` + `align`（宽度变化不重建 `Layout`）；
//! - 光栅：遍历 `positioned_glyphs()`，按 run 的字体光栅并装箱，产出 [`ShapedLine`]。

use std::borrow::Cow;

use parley::{
    FontContext, FontStyle as ParleyFontStyle, FontWeight as ParleyFontWeight, LayoutContext,
    LineHeight as ParleyLineHeight, OverflowWrap as ParleyOverflowWrap, PositionedLayoutItem,
    StyleProperty, TextWrapMode as ParleyTextWrapMode, WordBreak as ParleyWordBreak,
    layout::{Alignment, AlignmentOptions},
};
use smol_str::{SmolStr, format_smolstr};
use taffy::AvailableSpace;

use super::{
    InlineBoxSpec, ShapedGlyphRun, ShapedInlineBox, ShapedLine, ShapedTextItem, ShapedTextLine,
    TextAlign, TextContent, TextDiagnostic, TextDirection, TextSlant, TextStyle, TextWrap,
    atlas::{GlyphAtlas, GlyphAtlasStats},
    inline_flow,
    raster::GlyphRasterizer,
};
use crate::{
    error::EngineError,
    math::{Point, Rect, Size},
    paint::{color::ColorBrush, primitive::PositionedGlyph},
    render::atlas::AtlasAllocator,
};

/// 一次 shape 的不可变快照。
pub(crate) struct ShapedText {
    text: SmolStr,
    style: TextStyle,
    scale: f32,
    font_stack_version: u32,
    layout: parley::Layout<ColorBrush>,
    widths: parley::ContentWidths,
    /// 行内盒的边界/超宽诊断（shape 期间产生，随 `rasterize` 交给 `TextContext`）。
    inline_diagnostics: Vec<TextDiagnostic>,
    inline_boxes: Box<[InlineBoxSpec]>,
}

impl ShapedText {
    /// 跑一次 `RangedBuilder`（样式变化/文本变化才调用；宽度变化走 [`ShapedText::measure`]）。
    pub(crate) fn shape(
        font_cx: &mut FontContext,
        layout_cx: &mut LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        content: &TextContent,
        scale: f32,
        font_stack_version: u32,
    ) -> Self {
        let style = &content.style;
        let mut inline_diagnostics: Vec<TextDiagnostic> = Vec::new();
        let mut builder = layout_cx.ranged_builder(font_cx, &content.text, scale, true);
        builder.set_base_direction(match style.direction {
            TextDirection::Auto => parley::BaseDirection::Auto,
            TextDirection::Ltr => parley::BaseDirection::Ltr,
            TextDirection::Rtl => parley::BaseDirection::Rtl,
        });
        builder.push_default(StyleProperty::FontFamily(family.clone()));
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
        builder.push_default(StyleProperty::Brush(ColorBrush { color: style.color }));
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
        for spec in &content.inline_boxes {
            let (inline_box, diagnostic) = inline_flow::to_inline_box(spec, &content.text, scale);
            if let Some(diagnostic) = diagnostic {
                inline_diagnostics.push(diagnostic);
            }
            builder.push_inline_box(inline_box);
        }
        let mut layout = builder.build(&content.text);
        // 快照自身先按 max-content 断行 + 对齐：rasterize 要遍历 `lines()`，measure 再 clone 后重排。
        layout.break_all_lines(None);
        layout.align(align_of(style.alignment), AlignmentOptions::default());
        let widths = layout.calculate_content_widths();
        Self {
            text: content.text.clone(),
            style: style.clone(),
            scale,
            font_stack_version,
            layout,
            widths,
            inline_diagnostics,
            inline_boxes: content.inline_boxes.clone(),
        }
    }

    pub(crate) fn inline_boxes(&self) -> &[InlineBoxSpec] {
        &self.inline_boxes
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn style(&self) -> &TextStyle {
        &self.style
    }

    pub(crate) fn scale(&self) -> f32 {
        self.scale
    }

    pub(crate) fn font_stack_version(&self) -> u32 {
        self.font_stack_version
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> &parley::Layout<ColorBrush> {
        &self.layout
    }

    pub(crate) fn widths(&self) -> parley::ContentWidths {
        self.widths
    }

    /// 按**内容宽**（逻辑像素）重排：clone → `break_all_lines` → `align`（B1 的第二阶段）。
    pub(crate) fn layout_at(&self, width: f32) -> parley::Layout<ColorBrush> {
        let mut layout = self.layout.clone();
        layout.break_all_lines(Some(width * self.scale));
        layout.align(align_of(self.style.alignment), AlignmentOptions::default());
        layout
    }

    /// 字节估算（形状缓存的 LRU 预算；只用于逐出决策，不做精确记账）。
    pub(crate) fn approx_bytes(&self) -> usize {
        let glyphs: usize = self
            .layout
            .lines()
            .map(|line| line.items().count() * 32)
            .sum();
        core::mem::size_of::<Self>() + self.text.len() + glyphs
    }

    /// 仅测试：在指定宽度下重排后第一个字形的逻辑 x（验证 `align` 的真实效果）。
    #[cfg(test)]
    pub(crate) fn first_glyph_x_at(&self, width: f32) -> f32 {
        let mut layout = self.layout.clone();
        layout.break_all_lines(Some(width * self.scale));
        layout.align(align_of(self.style.alignment), AlignmentOptions::default());
        layout
            .lines()
            .next()
            .and_then(|line| line.items().next())
            .and_then(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => run
                    .positioned_glyphs()
                    .next()
                    .map(|glyph| glyph.x / self.scale),
                PositionedLayoutItem::InlineBox(_) => None,
            })
            .unwrap_or(0.0)
    }

    /// 按 taffy 语义测量（纯函数：只读 `&self`，clone 后重排）。
    ///
    /// 输入/输出都是**逻辑像素**；内部乘 `scale` 交给 parley（其坐标是物理像素）。
    pub(crate) fn measure(
        &self,
        known: taffy::Size<Option<f32>>,
        available: taffy::Size<AvailableSpace>,
    ) -> Size {
        if self.text.is_empty() {
            return Size::ZERO;
        }
        let width_logical = match available.width {
            AvailableSpace::Definite(width) => Some(width.max(0.0)),
            AvailableSpace::MinContent => Some(self.widths.min / self.scale),
            AvailableSpace::MaxContent => known.width,
        };
        let mut layout = self.layout.clone();
        layout.break_all_lines(width_logical.map(|width| width * self.scale));
        layout.align(align_of(self.style.alignment), AlignmentOptions::default());
        Size::new(
            (layout.width() / self.scale).max(0.0),
            (layout.height() / self.scale).max(0.0),
        )
    }

    /// 光栅 + 装箱，产出只读快照。
    ///
    /// `generation` 是当前字形图集的代际（写进 `ShapedLine`，供 P06 判 stale）。
    pub(crate) fn rasterize(
        &self,
        layout: &parley::Layout<ColorBrush>,
        content_width: f32,
        raster: &mut GlyphRasterizer,
        glyph_atlas: &mut GlyphAtlas,
        atlas: &mut AtlasAllocator,
        generation: u32,
    ) -> Result<(ShapedLine, Vec<TextDiagnostic>, GlyphAtlasStats), EngineError> {
        let scale = self.scale;
        if self.text.is_empty() {
            return Ok((
                ShapedLine::empty(
                    self.text.clone(),
                    generation,
                    self.font_stack_version,
                    self.scale,
                ),
                Vec::new(),
                glyph_atlas.take_stats(),
            ));
        }
        let mut diagnostics: Vec<TextDiagnostic> = self.inline_diagnostics.clone();
        let available = content_width;
        let mut lines = Vec::with_capacity(self.layout.lines().count());
        for line in layout.lines() {
            let metrics = line.metrics();
            let mut items = Vec::new();
            for item in line.items() {
                match item {
                    PositionedLayoutItem::GlyphRun(run) => {
                        let font = run.run().font().clone();
                        // parley 的 `Run::font_size()` **已是物理值**（B2：不能再乘 scale）。
                        let ppem = run.run().font_size().round().clamp(1.0, 512.0);
                        let text_range = run.run().text_range();
                        let color = run.style().brush.color;
                        if run.run().synthesis().any() {
                            diagnostics.push(TextDiagnostic::SyntheticStyle {
                                text_range: text_range.clone(),
                            });
                        }
                        let mut glyphs = Vec::new();
                        for glyph in run.positioned_glyphs() {
                            if glyph.id == 0 {
                                diagnostics.push(TextDiagnostic::MissingGlyph {
                                    font: font_label(&font),
                                    glyph: glyph.id,
                                    text_range: text_range.clone(),
                                });
                                continue;
                            }
                            let tile = glyph_atlas.ensure(raster, atlas, &font, glyph.id, ppem)?;
                            let Some(tile) = tile else {
                                // 空白/无轮廓：不产生像素，advance 已由 parley 推进。
                                continue;
                            };
                            let origin_x = glyph.x.round() + tile.left as f32;
                            let origin_y = glyph.y.round() - tile.top as f32;
                            glyphs.push(PositionedGlyph {
                                tile: tile.tile,
                                position: Point::new(origin_x / scale, origin_y / scale),
                                size: Size::new(
                                    tile.width as f32 / scale,
                                    tile.height as f32 / scale,
                                ),
                            });
                        }
                        items.push(ShapedTextItem::GlyphRun(ShapedGlyphRun {
                            color,
                            text_range,
                            glyphs,
                        }));
                    }
                    PositionedLayoutItem::InlineBox(box_) => {
                        let rect = Rect::new(
                            box_.x / scale,
                            box_.y / scale,
                            box_.width / scale,
                            box_.height / scale,
                        );
                        if rect.size.width > available && available > 0.0 {
                            diagnostics.push(TextDiagnostic::InlineBoxOverflow {
                                id: box_.id,
                                width: rect.size.width,
                                available,
                            });
                        }
                        items.push(ShapedTextItem::InlineBox(ShapedInlineBox {
                            id: box_.id,
                            rect,
                            baseline: box_.baseline.map(|baseline| baseline / scale),
                        }));
                    }
                }
            }
            lines.push(ShapedTextLine {
                text_range: line.text_range(),
                baseline: metrics.baseline / scale,
                line_height: metrics.line_height / scale,
                advance: metrics.advance / scale,
                items,
            });
        }
        let shaped = ShapedLine {
            text: self.text.clone(),
            lines,
            width: layout.width() / scale,
            height: layout.height() / scale,
            first_baseline: self
                .layout
                .first_baseline()
                .map(|baseline| baseline / scale),
            atlas_generation: generation,
            font_stack_version: self.font_stack_version(),
            scale,
        };
        Ok((shaped, diagnostics, glyph_atlas.take_stats()))
    }
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

fn font_label(font: &parley::FontData) -> SmolStr {
    format_smolstr!("{}#{}", font.data.data().len(), font.index)
}

/// 构造 D17 的家族列表（`FontStack` → parley `FontFamily::List` + generic 兜底）。
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
        FontStack, InlineAlign, InlineBoxSpec, ShapedTextItem, TextContent, TextStyle, TextWrap,
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

        fn shape_scaled(&mut self, text: &str, style: TextStyle, scale: f32) -> super::ShapedText {
            super::ShapedText::shape(
                &mut self.font_cx,
                &mut self.layout_cx,
                &self.family,
                &TextContent::new(text, style),
                scale,
                1,
            )
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
            super::ShapedText::shape(
                &mut self.font_cx,
                &mut self.layout_cx,
                &self.family,
                &content,
                1.0,
                1,
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

        assert_eq!(start.layout().alignment(), Some(parley::Alignment::Start));
        assert_eq!(
            centered.layout().alignment(),
            Some(parley::Alignment::Center)
        );
        assert!(
            centered.first_glyph_x_at(200.0) > start.first_glyph_x_at(200.0),
            "centering inside a 200px line box must shift the glyph right: {} vs {}",
            centered.first_glyph_x_at(200.0),
            start.first_glyph_x_at(200.0)
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
        // B1 的反例：max-content 宽相同（advance 不变），定宽下的断行行数不同。
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
    fn inline_box_inserts_an_item_between_runs() {
        let mut harness = Harness::new();
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::Size::new(20.0, 18.0),
            baseline: None,
            align: InlineAlign::Baseline,
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
    fn rasterize_of_empty_text_yields_empty_snapshot() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(16.0));
        let mut raster = crate::text::raster::GlyphRasterizer::new();
        let mut glyphs = crate::text::atlas::GlyphAtlas::new();
        let mut atlas = crate::render::atlas::AtlasAllocator::for_test(
            crate::paint::atlas::AtlasKind::Glyph,
            (256, 256),
        );

        let generation = atlas.generation();
        let (line, diagnostics, _stats) = shaped
            .rasterize(
                shaped.layout(),
                shaped.widths().max,
                &mut raster,
                &mut glyphs,
                &mut atlas,
                generation,
            )
            .expect("empty text cannot fail");

        assert!(line.lines.is_empty());
        assert_eq!(line.width, 0.0);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn rasterize_positions_glyphs_and_sets_tiles() {
        let mut harness = Harness::new();
        let shaped = harness.shape("Ab", TextStyle::new(16.0));
        let mut raster = crate::text::raster::GlyphRasterizer::new();
        let mut glyphs = crate::text::atlas::GlyphAtlas::new();
        let mut atlas = crate::render::atlas::AtlasAllocator::for_test(
            crate::paint::atlas::AtlasKind::Glyph,
            (512, 512),
        );

        let generation = atlas.generation();
        let (line, _diagnostics, stats) = shaped
            .rasterize(
                shaped.layout(),
                shaped.widths().max,
                &mut raster,
                &mut glyphs,
                &mut atlas,
                generation,
            )
            .expect("must succeed");

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
        }
        assert_eq!(stats.rasters, run.glyphs.len());
    }

    #[test]
    fn raster_ppem_is_physical_and_grows_with_scale() {
        // B2：`Run::font_size()` 已是物理值，ppem 只能取整一次，不能乘 scale。
        let mut harness = Harness::new();
        if crate::text::raster::probe_font().is_none() {
            return;
        }
        let mut heights = Vec::new();
        for scale in [1.0f32, 1.25, 2.0] {
            let shaped = harness.shape_scaled("H", TextStyle::new(16.0), scale);
            let mut raster = crate::text::raster::GlyphRasterizer::new();
            let mut glyphs = crate::text::atlas::GlyphAtlas::new();
            let mut atlas = crate::render::atlas::AtlasAllocator::for_test(
                crate::paint::atlas::AtlasKind::Glyph,
                (1024, 1024),
            );
            let generation = atlas.generation();
            let (line, _, stats) = shaped
                .rasterize(
                    shaped.layout(),
                    shaped.widths().max,
                    &mut raster,
                    &mut glyphs,
                    &mut atlas,
                    generation,
                )
                .expect("rasterize");
            assert_eq!(stats.rasters, 1);
            let glyph = line.lines[0]
                .items
                .iter()
                .find_map(|item| match item {
                    ShapedTextItem::GlyphRun(run) => run.glyphs.first().copied(),
                    ShapedTextItem::InlineBox(_) => None,
                })
                .expect("glyph");
            let _ = glyph;
            heights.push((scale, shaped.layout().width(), line.width));
        }
        let physical: Vec<f32> = heights.iter().map(|(_, physical, _)| *physical).collect();
        assert!(
            physical[1] > physical[0],
            "physical width grows with the scale: {physical:?}"
        );
        assert!(
            physical[2] > physical[1],
            "physical width grows with the scale: {physical:?}"
        );
        let logical: Vec<f32> = heights.iter().map(|(_, _, logical)| *logical).collect();
        assert!(
            (logical[0] - logical[2]).abs() < 0.5,
            "logical width is scale-invariant: {logical:?}"
        );
    }

    #[test]
    fn explicit_family_changes_the_shape() {
        // B3：`TextStyle.family` 必须**前置**进 family 列表 —— 用列表内容断言，不依赖宿主字体。
        // （原实现按 advance 比较三个 Windows 专有家族名；Linux/macOS 上它们都不存在，
        //  三者 fallback 到同一字体、advance 完全相同，测试必然失败。）
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
        // 家族真的能改变 advance —— 只在宿主同时装了这两族时才有意义（跨平台可跳过）。
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
            widths.push((family, shaped.widths().max));
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
}
