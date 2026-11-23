use std::ops::Range;

use smol_str::SmolStr;
use ui_core::{
    math::Size,
    paint::color::Color,
    text::{
        InlineAlign, InlineBoxPaint, InlineBoxSpec, TextDecoration, TextRun, TextSlant, TextStyle,
    },
};

use super::{
    document::{KbdId, LinkId, MarkdownPiece},
    source::InlineImageFormat,
};

pub const MONO_FAMILY: &str = "Consolas";

pub const DECORATION_THICKNESS: f32 = 1.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Flow {
    pub text: SmolStr,
    pub runs: Vec<TextRun>,
    pub inline_boxes: Vec<InlineBoxSpec>,
    pub link_spans: Vec<LinkSpan>,
    pub kbd_spans: Vec<Range<usize>>,
}

impl Flow {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.inline_boxes.is_empty()
    }

    pub fn link_at(&self, byte: usize) -> Option<LinkId> {
        self.link_spans
            .iter()
            .find(|span| span.range.start <= byte && byte < span.range.end)
            .map(|span| span.link)
    }

    pub fn kbd_at(&self, byte: usize) -> Option<KbdId> {
        self.kbd_spans
            .iter()
            .position(|range| range.start <= byte && byte < range.end)
            .map(KbdId)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkSpan {
    pub range: Range<usize>,
    pub link: LinkId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FlowStyle {
    pub font_size: f32,
    pub color: Color,
    pub link: Color,
    pub code: Color,
    pub bold: bool,
}

impl FlowStyle {
    pub fn new(font_size: f32, color: Color, link: Color, code: Color) -> Self {
        Self {
            font_size,
            color,
            link,
            code,
            bold: false,
        }
    }

    pub fn with_bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }
}

pub trait ImagePaintSource {
    fn paint_for(
        &mut self,
        path: &str,
        format: InlineImageFormat,
        size: Size,
    ) -> Option<InlineBoxPaint>;
}

#[derive(Clone, Copy, Debug)]
pub struct PlaceholderImagePaint {
    pub color: Color,
}

impl ImagePaintSource for PlaceholderImagePaint {
    fn paint_for(
        &mut self,
        _path: &str,
        _format: InlineImageFormat,
        _size: Size,
    ) -> Option<InlineBoxPaint> {
        Some(InlineBoxPaint::Quad {
            color: self.color,
            corner_radius: [2.0; 4],
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InvisibleImages;

impl ImagePaintSource for InvisibleImages {
    fn paint_for(&mut self, _: &str, _: InlineImageFormat, _: Size) -> Option<InlineBoxPaint> {
        None
    }
}

pub fn flow(
    pieces: &[MarkdownPiece],
    style: &FlowStyle,
    images: &mut dyn ImagePaintSource,
) -> Flow {
    let mut builder = FlowBuilder::new(style);
    for piece in pieces {
        match piece {
            MarkdownPiece::Text {
                text,
                strong,
                emphasis,
                strike,
                code,
                link,
                kbd,
            } => builder.push_text(
                text,
                MarkStyle {
                    strong: *strong,
                    emphasis: *emphasis,
                    strike: *strike,
                    code: *code,
                    link: *link,
                    kbd: *kbd,
                },
            ),
            MarkdownPiece::InlineImage {
                path,
                format,
                size,
                link,
                kbd,
            } => builder.push_image(path, *format, *size, *link, *kbd, images),
            MarkdownPiece::LineBreak => builder.push_line_break(),
        }
    }
    builder.finish()
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MarkStyle {
    strong: bool,
    emphasis: bool,
    strike: bool,
    code: bool,
    link: Option<LinkId>,
    kbd: Option<KbdId>,
}

struct FlowBuilder<'a> {
    style: &'a FlowStyle,
    text: String,
    runs: Vec<TextRun>,
    inline_boxes: Vec<InlineBoxSpec>,
    link_spans: Vec<LinkSpan>,
    kbd_spans: Vec<Range<usize>>,
    open_kbd: Option<(KbdId, usize)>,
    open_link: Option<(LinkId, usize)>,
}

impl<'a> FlowBuilder<'a> {
    fn new(style: &'a FlowStyle) -> Self {
        Self {
            style,
            text: String::new(),
            runs: Vec::new(),
            inline_boxes: Vec::new(),
            link_spans: Vec::new(),
            kbd_spans: Vec::new(),
            open_kbd: None,
            open_link: None,
        }
    }

    fn push_text(&mut self, text: &str, mark: MarkStyle) {
        if text.is_empty() {
            self.switch_marks(mark, self.text.len());
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        self.switch_marks(mark, start);
        self.runs
            .push(TextRun::new(start..end, self.run_style(self.style, mark)));
    }

    fn switch_marks(&mut self, mark: MarkStyle, at: usize) {
        if self.open_link.map(|(id, _)| id) != mark.link {
            if let Some((id, start)) = self.open_link.take()
                && start < at
            {
                self.link_spans.push(LinkSpan {
                    range: start..at,
                    link: id,
                });
            }
            if let Some(id) = mark.link {
                self.open_link = Some((id, at));
            }
        }
        if self.open_kbd.map(|(id, _)| id) != mark.kbd {
            if let Some((_, start)) = self.open_kbd.take()
                && start < at
            {
                self.kbd_spans.push(start..at);
            }
            if let Some(id) = mark.kbd {
                self.open_kbd = Some((id, at));
            }
        }
    }

    fn push_image(
        &mut self,
        path: &str,
        format: InlineImageFormat,
        size: Size,
        link: Option<LinkId>,
        kbd: Option<KbdId>,
        images: &mut dyn ImagePaintSource,
    ) {
        let mark = MarkStyle {
            link,
            kbd,
            ..MarkStyle::default()
        };
        self.switch_marks(mark, self.text.len());
        let id = self.inline_boxes.len() as u64;
        self.inline_boxes.push(InlineBoxSpec {
            id,
            index: self.text.len(),
            size,
            baseline: None,
            align: InlineAlign::Middle,
            paint: images.paint_for(path, format, size),
        });
    }

    fn push_line_break(&mut self) {
        let mark = MarkStyle::default();
        self.switch_marks(mark, self.text.len());
        let at = self.text.len();
        self.text.push('\n');
        self.runs
            .push(TextRun::new(at..at + 1, self.run_style(self.style, mark)));
    }

    fn finish(mut self) -> Flow {
        let end = self.text.len();
        self.switch_marks(MarkStyle::default(), end);
        Flow {
            text: SmolStr::new(&self.text),
            runs: self.runs,
            inline_boxes: self.inline_boxes,
            link_spans: self.link_spans,
            kbd_spans: self.kbd_spans,
        }
    }

    fn run_style(&self, base: &FlowStyle, mark: MarkStyle) -> TextStyle {
        let mut style = TextStyle::new(base.font_size);
        style.weight = if base.bold || mark.strong { 700 } else { 400 };
        style.slant = if mark.emphasis {
            TextSlant::Italic
        } else {
            TextSlant::Normal
        };
        if mark.code || mark.kbd.is_some() {
            style = style.with_family(SmolStr::new_static(MONO_FAMILY));
        }
        style = style.with_color(match (mark.link, mark.code || mark.kbd.is_some()) {
            (Some(_), _) => base.link,
            (None, true) => base.code,
            (None, false) => base.color,
        });
        if mark.link.is_some() {
            style = style.with_decoration(
                TextDecoration::underline(DECORATION_THICKNESS).with_color(Some(base.link)),
            );
        } else if mark.strike {
            style = style.with_decoration(
                TextDecoration::strikethrough(DECORATION_THICKNESS).with_color(Some(base.color)),
            );
        } else if mark.kbd.is_some() {
            style = style.with_decoration(
                TextDecoration::underline(DECORATION_THICKNESS).with_color(Some(base.code)),
            );
        }
        style
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use ui_core::{
        math::Size,
        paint::color::Color,
        text::{InlineAlign, InlineBoxPaint, TextDecoration, TextSlant},
    };

    use super::{
        Flow, FlowStyle, ImagePaintSource, InvisibleImages, MONO_FAMILY, PlaceholderImagePaint,
        flow,
    };
    use crate::kit::markdown::{
        document::{KbdId, LinkId, MarkdownPiece},
        source::InlineImageFormat,
    };

    fn color(r: f32) -> Color {
        Color::Srgb {
            r,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        }
    }

    fn style() -> FlowStyle {
        FlowStyle::new(16.0, color(1.0), color(0.5), color(0.25))
    }

    fn text(value: &str) -> MarkdownPiece {
        MarkdownPiece::Text {
            text: SmolStr::new(value),
            strong: false,
            emphasis: false,
            strike: false,
            code: false,
            link: None,
            kbd: None,
        }
    }

    fn marked(
        value: &str,
        strong: bool,
        emphasis: bool,
        strike: bool,
        code: bool,
        link: Option<usize>,
        kbd: Option<usize>,
    ) -> MarkdownPiece {
        MarkdownPiece::Text {
            text: SmolStr::new(value),
            strong,
            emphasis,
            strike,
            code,
            link: link.map(LinkId),
            kbd: kbd.map(KbdId),
        }
    }

    fn image(path: &str, format: InlineImageFormat, side: f32) -> MarkdownPiece {
        MarkdownPiece::InlineImage {
            path: SmolStr::new(path),
            format,
            size: Size::new(side, side),
            link: None,
            kbd: None,
        }
    }

    fn simple(texts: &[&str]) -> Flow {
        let pieces: Vec<MarkdownPiece> = texts.iter().map(|t| text(t)).collect();
        flow(&pieces, &style(), &mut InvisibleImages)
    }

    #[test]
    fn empty_pieces_make_an_empty_flow() {
        let flow = simple(&[]);
        assert!(flow.is_empty());
        assert_eq!(flow.text, "");
        assert!(flow.runs.is_empty());
        assert!(flow.link_spans.is_empty());
    }

    #[test]
    fn plain_text_becomes_one_run_spanning_the_whole_text() {
        let flow = simple(&["hello", " ", "world"]);
        assert_eq!(flow.text, "hello world");
        assert_eq!(flow.runs.len(), 3, "每个片段一个 run（映射层不合并）");
        assert_eq!(flow.runs[0].range, 0..5);
        assert_eq!(flow.runs[1].range, 5..6);
        assert_eq!(flow.runs[2].range, 6..11);
        assert_eq!(flow.runs[2].style.size, 16.0);
        assert_eq!(flow.runs[2].style.weight, 400);
        assert_eq!(flow.runs[2].style.slant, TextSlant::Normal);
        assert!(flow.runs[2].style.decoration().is_none());
    }

    #[test]
    fn strong_and_emphasis_only_affect_their_own_range() {
        let pieces = [
            text("a"),
            marked("b", true, false, false, false, None, None),
            marked("c", false, true, false, false, None, None),
            text("d"),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.text, "abcd");
        assert_eq!(flow.runs.len(), 4);
        assert_eq!(flow.runs[0].style.weight, 400);
        assert_eq!(flow.runs[1].style.weight, 700, "粗体只覆盖 b");
        assert_eq!(flow.runs[2].style.slant, TextSlant::Italic);
        assert_eq!(flow.runs[3].style.weight, 400);
        assert_eq!(flow.runs[3].style.slant, TextSlant::Normal);
    }

    #[test]
    fn heading_style_bolds_every_run() {
        let pieces = [
            text("a"),
            marked("b", false, true, false, false, None, None),
        ];
        let flow = flow(&pieces, &style().with_bold(true), &mut InvisibleImages);
        assert!(flow.runs.iter().all(|run| run.style.weight == 700));
        assert_eq!(flow.runs[1].style.slant, TextSlant::Italic);
    }

    #[test]
    fn links_carry_color_underline_and_byte_spans() {
        let pieces = [
            text("go "),
            marked("here", false, false, false, false, Some(0), None),
            text(" now"),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.text, "go here now");
        assert_eq!(
            flow.link_spans,
            vec![super::LinkSpan {
                range: 3..7,
                link: LinkId(0)
            }]
        );
        assert_eq!(flow.link_at(2), None);
        assert_eq!(flow.link_at(3), Some(LinkId(0)));
        assert_eq!(flow.link_at(6), Some(LinkId(0)));
        assert_eq!(flow.link_at(7), None, "半开区间：链接终点不属于链接");
        let decoration = flow.runs[1].style.decoration().expect("链接必须带下划线");
        assert!(matches!(decoration, TextDecoration::Underline { .. }));
        assert_eq!(decoration.color(), Some(color(0.5)));
        assert_eq!(flow.runs[1].style.color, color(0.5));
    }

    #[test]
    fn strikethrough_is_its_own_decoration() {
        let pieces = [marked("x", false, false, true, false, None, None)];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        let decoration = flow.runs[0].style.decoration().expect("删除线");
        assert!(matches!(decoration, TextDecoration::Strikethrough { .. }));
        assert_eq!(decoration.color(), Some(color(1.0)));
    }

    #[test]
    fn inline_code_uses_monospace_and_code_color() {
        let pieces = [marked("let x", false, false, false, true, None, None)];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(
            flow.runs[0].style.family().map(SmolStr::as_str),
            Some(MONO_FAMILY)
        );
        assert_eq!(flow.runs[0].style.color, color(0.25));
    }

    #[test]
    fn kbd_runs_are_tracked_and_styled() {
        let pieces = [
            marked("Ctrl", false, false, false, false, None, Some(0)),
            text("+"),
            marked("K", false, false, false, false, None, Some(0)),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.text, "Ctrl+K");
        assert_eq!(flow.kbd_spans, vec![0..4, 5..6]);
        assert_eq!(flow.kbd_at(0), Some(KbdId(0)));
        assert_eq!(flow.kbd_at(4), None, "`+` 不在 kbd 里");
        let decoration = flow.runs[0].style.decoration().expect("kbd 降级为下划线");
        assert!(matches!(decoration, TextDecoration::Underline { .. }));
    }

    #[test]
    fn hard_breaks_become_newline_bytes_and_reset_marks() {
        let pieces = [
            marked("a", true, false, false, false, None, None),
            MarkdownPiece::LineBreak,
            text("b"),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.text, "a\nb");
        assert_eq!(flow.runs[1].range, 1..2);
        assert_eq!(flow.runs[1].style.weight, 400);
    }

    #[test]
    fn images_do_not_consume_text_bytes_and_carry_their_offset() {
        let pieces = [
            text("a"),
            image("brands/Binance/binance.png", InlineImageFormat::Image, 24.0),
            text("b"),
            image(
                "icons/phosphoricons/graph.svg",
                InlineImageFormat::Svg,
                16.0,
            ),
        ];
        let flow = flow(
            &pieces,
            &style(),
            &mut PlaceholderImagePaint { color: color(0.75) },
        );
        assert_eq!(flow.text, "ab", "图片不占文本字节");
        assert_eq!(flow.inline_boxes.len(), 2);
        assert_eq!(flow.inline_boxes[0].index, 1, "第一张图在 a、b 之间");
        assert_eq!(flow.inline_boxes[1].index, 2);
        assert_eq!(flow.inline_boxes[0].size, Size::new(24.0, 24.0));
        assert_eq!(flow.inline_boxes[0].align, InlineAlign::Middle);
        assert_eq!(flow.inline_boxes[0].id, 0);
        assert_eq!(flow.inline_boxes[1].id, 1);
        assert!(matches!(
            flow.inline_boxes[0].paint,
            Some(InlineBoxPaint::Quad { .. })
        ));
    }

    #[test]
    fn an_image_only_paragraph_keeps_a_box_at_offset_zero() {
        let pieces = [image("x.png", InlineImageFormat::Image, 8.0)];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.text, "");
        assert_eq!(flow.inline_boxes.len(), 1);
        assert_eq!(flow.inline_boxes[0].index, 0);
        assert!(flow.inline_boxes[0].paint.is_none());
    }

    #[test]
    fn multi_byte_offsets_stay_on_char_boundaries() {
        let pieces = [
            text("中文"),
            marked("abc", true, false, false, false, None, None),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.text, "中文abc");
        assert_eq!(flow.runs[1].range, 6..9);
        assert!(flow.text.is_char_boundary(flow.runs[1].range.start));
    }

    #[test]
    fn link_span_covers_multi_byte_text_exactly() {
        let pieces = [
            marked("中文", false, false, false, false, Some(1), None),
            text(" tail"),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.link_spans[0].range, 0..6);
        assert_eq!(flow.link_at(5), Some(LinkId(1)));
        assert_eq!(flow.link_at(6), None);
    }

    #[test]
    fn link_span_and_kbd_span_close_at_piece_boundaries() {
        let pieces = [
            marked("a", false, false, false, false, Some(0), Some(0)),
            text("b"),
            marked("c", false, false, false, false, Some(0), None),
            text("d"),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.link_spans.len(), 2);
        assert_eq!(flow.link_spans[0].range, 0..1);
        assert_eq!(flow.link_spans[1].range, 2..3);
        assert_eq!(flow.kbd_spans, vec![0..1]);
    }

    #[test]
    fn links_around_images_keep_their_ranges() {
        let pieces = [
            marked("a", false, false, false, false, Some(0), None),
            MarkdownPiece::InlineImage {
                path: SmolStr::new_static("x.png"),
                format: InlineImageFormat::Image,
                size: Size::new(8.0, 8.0),
                link: Some(LinkId(0)),
                kbd: None,
            },
            marked("b", false, false, false, false, Some(0), None),
        ];
        let flow = flow(&pieces, &style(), &mut InvisibleImages);
        assert_eq!(flow.inline_boxes.len(), 1);
        assert_eq!(
            flow.link_spans.len(),
            1,
            "图片不打断链接：同一段链接内容里的图片两侧仍是同一条链接（字节区间连续）"
        );
        assert_eq!(flow.link_spans[0].range, 0..2);
        assert_eq!(flow.link_at(0), Some(LinkId(0)));
        assert_eq!(flow.link_at(1), Some(LinkId(0)), "图片插入点也在链接里");
    }

    #[test]
    fn image_paint_source_can_choose_image_or_skip() {
        struct Marker;
        impl ImagePaintSource for Marker {
            fn paint_for(
                &mut self,
                _path: &str,
                _format: InlineImageFormat,
                size: Size,
            ) -> Option<InlineBoxPaint> {
                Some(InlineBoxPaint::Quad {
                    color: color(size.width / 100.0),
                    corner_radius: [0.0; 4],
                })
            }
        }
        let pieces = [image("a.png", InlineImageFormat::Image, 50.0)];
        let flow = flow(&pieces, &style(), &mut Marker);
        match flow.inline_boxes[0].paint.expect("有绘制方式") {
            InlineBoxPaint::Quad { color: fill, .. } => assert_eq!(fill, color(0.5)),
            other => panic!("expected quad, got {other:?}"),
        }
    }
}
