//! 行内片段 → 引擎富文本模型（`TextContent{runs, inline_boxes}`）。
//!
//! 旧实现的 `inline_flow.rs` 是一个**自绘 `Element`**（1,546 行）：自己塑形、自己折行、自己
//! 画行内盒与链接下划线。引擎 E2 把这些能力搬进了文本管线（parley 折行 + 逐 run 样式 +
//! `TextDecoration` 几何 + `InlineBoxSpec` 落点），所以这里只做**数据映射**：
//!
//! - 粗体 / 斜体 / 行内代码 / 链接色 → [`TextRun`]（逐字节区间覆盖样式）；
//! - 链接下划线 / 删除线 → `TextStyle.decoration`（引擎按行画线，硬换行与 RTL 的裁剪规则见
//!   引擎 E2 规格，应用不需要再算几何）；
//! - 行内图片 → [`InlineBoxSpec`]（尺寸在解析期已定，位置由引擎算；`id` = 图片在片段里的
//!   序数，`InlineBoxPaint` 由视图层按资源通路补）；
//! - 链接命中 → 本模块自持的**字节区间** + 引擎 E5 的 `Window::text_byte_index_at`（把点变成
//!   文本字节偏移，簇边界由 parley 决定），见 [`Flow::link_at`]。
//!
//! 旧实现里值得保留的语义在映射里逐条对上：
//!
//! | 旧行为 | 本模块 |
//! |---|---|
//! | `item_spans` 的逻辑索引空间（图片占 1 字节） | 不需要：图片不占文本字节，`InlineBoxSpec.index` 直接挂在插入点的字节偏移上 |
//! | 硬换行（`LineBreak` → `"\n"`） | 保留：文本里就是一个 `\n`，由 parley 断行 |
//! | 行尾空白悬挂 / 折行复核循环 | 引擎负责（parley 原生），不再有复核循环 |
//! | 段落左/中/右对齐 | `TextStyle.alignment`（视图层按块设置） |
//! | 标题字号倍率 / 粗体 | 视图层传给 [`FlowStyle`] 的基准样式（`scale` + `bold`） |
//! | kbd 的圆角框（文字 + 边框整体不可拆） | **降级**：引擎 `InlineBoxSpec` 只画盒、不承载文字（"带内容的行内盒"是 E2 遗留项），所以 kbd 内容按等宽字体 + 下划线表达，框记录在 [`Flow::kbd_spans`] 里；见 `spec-deltas/kit-text.md` |
//! | hover 才画链接下划线 | **降级为常驻下划线**：`runs` 不是动态数据（`bind_style` 只能换整节点前景色），hover 期间无法只给一条链接加线；常驻下划线是引擎能力内的最接近表达 |

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

/// 行内代码 / kbd 用的等宽字体族。
///
/// 引擎（fontique）对不存在的家族静默跳过并回落到系统默认栈，所以这里写一个跨平台最常见的
/// 等宽家族即可：Linux 上它通常不存在，会回落成正文默认字体（可读性不降级，只是不"等宽"）。
pub const MONO_FAMILY: &str = "Consolas";

/// 链接 / 删除线的线宽（逻辑像素）。
pub const DECORATION_THICKNESS: f32 = 1.0;

/// 一段行内内容在引擎文本模型里的展开结果。
///
/// `text` 是唯一的源文本：runs 与 inline_boxes 的偏移都对着它。段落级别的字号/对齐/换行
/// 由视图层写进 `TextContent.style`（本结构不持有段落级字段）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Flow {
    /// 源文本（`MarkdownPiece::Text` 原样拼接、`LineBreak` 变成一个 `\n`、图片不占字节）。
    pub text: SmolStr,
    /// 富文本 runs（升序、不重叠、落在 char boundary）。
    pub runs: Vec<TextRun>,
    /// 行内盒（行内图片）：`index` 是图片插入点的字节偏移，`id` 是从 0 起的图片序数。
    pub inline_boxes: Vec<InlineBoxSpec>,
    /// 链接的字节区间（按起点升序；命中用它 + E5 的字节索引）。
    pub link_spans: Vec<LinkSpan>,
    /// 每段 kbd 内容的字节区间（按起点升序；样式已经烘进 runs，这里只供命中/测试/后续扩展）。
    pub kbd_spans: Vec<Range<usize>>,
}

impl Flow {
    /// 空内容。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.inline_boxes.is_empty()
    }

    /// 字节偏移落在哪条链接里；`None` = 不在链接里。
    ///
    /// 区间是半开区间：链接末尾的那个字节（下一条链接的起点）不属于它。图片不占文本字节，
    /// 所以"只有图片的链接"没有可命中的区间（解析层的 `link_spans` 里它是空区间，被跳过）——
    /// 这是本批已知限制，见 `spec-deltas/kit-text.md`。
    pub fn link_at(&self, byte: usize) -> Option<LinkId> {
        self.link_spans
            .iter()
            .find(|span| span.range.start <= byte && byte < span.range.end)
            .map(|span| span.link)
    }

    /// 字节偏移落在哪一段 kbd 内容里。
    pub fn kbd_at(&self, byte: usize) -> Option<KbdId> {
        self.kbd_spans
            .iter()
            .position(|range| range.start <= byte && byte < range.end)
            .map(KbdId)
    }
}

/// 一条链接覆盖的字节区间。
#[derive(Clone, Debug, PartialEq)]
pub struct LinkSpan {
    pub range: Range<usize>,
    pub link: LinkId,
}

/// 行内映射的样式来源（挂载期从 `crate::theme` 解析一次）。
#[derive(Clone, Debug, PartialEq)]
pub struct FlowStyle {
    /// 本块基准字号（标题 = 正文字号 × 倍率）。
    pub font_size: f32,
    /// 正文文字色。
    pub color: Color,
    /// 链接色。
    pub link: Color,
    /// 行内代码 / kbd 的文字色。
    pub code: Color,
    /// 本块是否整体加粗（标题）。
    pub bold: bool,
}

impl FlowStyle {
    /// 一个基准样式。
    pub fn new(font_size: f32, color: Color, link: Color, code: Color) -> Self {
        Self {
            font_size,
            color,
            link,
            code,
            bold: false,
        }
    }

    /// 整块加粗（标题）。
    pub fn with_bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }

    /// 本块兜底样式（段落级：字号、颜色、字重）。
    pub fn base_style(&self) -> TextStyle {
        let weight = if self.bold { 700 } else { 400 };
        TextStyle::new(self.font_size)
            .with_color(self.color)
            .with_weight(weight)
    }
}

/// 一张行内图片的绘制方式：视图层按资源通路决定（`Image` / 占位 `Quad` / 不画）。
pub trait ImagePaintSource {
    /// 返回该图片的绘制方式；`None` = 只算落点、不画（引擎语义）。
    fn paint_for(
        &mut self,
        path: &str,
        format: InlineImageFormat,
        size: Size,
    ) -> Option<InlineBoxPaint>;
}

/// 默认的图片绘制方式：画一个占位方块（资源通路尚未接线的批次/测试用）。
#[derive(Clone, Copy, Debug)]
pub struct PlaceholderImagePaint {
    /// 占位色。
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

/// 不画任何行内图片（只保留落点）。
#[derive(Clone, Copy, Debug, Default)]
pub struct InvisibleImages;

impl ImagePaintSource for InvisibleImages {
    fn paint_for(&mut self, _: &str, _: InlineImageFormat, _: Size) -> Option<InlineBoxPaint> {
        None
    }
}

/// 把一段行内片段展开成引擎富文本模型；`images` 决定行内图片画什么。
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

/// 片段上的行内标记（strong/emphasis 等）——单独一个结构体免得 [`flow`] 里参数爆炸。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MarkStyle {
    strong: bool,
    emphasis: bool,
    strike: bool,
    code: bool,
    link: Option<LinkId>,
    kbd: Option<KbdId>,
}

/// 累积中的行内流。
struct FlowBuilder<'a> {
    style: &'a FlowStyle,
    text: String,
    runs: Vec<TextRun>,
    inline_boxes: Vec<InlineBoxSpec>,
    link_spans: Vec<LinkSpan>,
    kbd_spans: Vec<Range<usize>>,
    /// 当前打开的 kbd 段（编号 + 起点）。
    open_kbd: Option<(KbdId, usize)>,
    /// 当前打开的链接（id + 起点）。
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

    /// 追加一段文字：先按标记算出覆盖样式，区间就是追加前后的字节范围。
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

    /// 关闭/开启链接与 kbd 段：标记切换点就是上一段的终点、下一段的起点。
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

    /// 行内图片：不占文本字节，盒子挂在当前插入点。
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
            // 旧实现的图片在行内垂直居中；引擎的 `Middle` 是同一语义。
            align: InlineAlign::Middle,
            paint: images.paint_for(path, format, size),
        });
    }

    /// 硬换行：文本里一个 `\n`。
    fn push_line_break(&mut self) {
        let mark = MarkStyle::default();
        self.switch_marks(mark, self.text.len());
        let at = self.text.len();
        self.text.push('\n');
        self.runs
            .push(TextRun::new(at..at + 1, self.run_style(self.style, mark)));
    }

    /// 收尾：关闭还开着的链接 / kbd 段。
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

    /// 一个区间覆盖样式 = 兜底样式 + 本片段的标记。
    ///
    /// 引擎语义是「逐属性继承 + 可选覆盖」：这里把**所有**要覆盖的属性显式写出来（字号、颜色、
    /// 字重、字形、家族、装饰），不依赖兜底样式的非默认值。
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
