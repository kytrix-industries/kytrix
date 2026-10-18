use std::borrow::Cow;

use gpui_kit::{AssetSource, Pixels, SharedString, px, size};

use super::{
    document::{
        HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownPiece, ParagraphAlign,
    },
    parse,
    source::InlineImageFormat,
};

/// 测试用的资源源：只认 [`INTRINSIC_ASSET`] 一条路径，交出一张 [`INTRINSIC_BYTES`] 里的位图。
///
/// 生产代码的资源源就是产品自己的 [`Assets`](crate::assets::Assets)，这里换成一份最小的假资源，
/// 是为了让"没写 `size` 就用资源自身的尺寸"这条在解析层可验；真资源的那份断言在
/// `source::tests` 里。
struct TestAssets;

/// [`TestAssets`] 唯一认得的那条路径。
const INTRINSIC_ASSET: &str = "test/intrinsic.bmp";
/// 那份假位图的宽高。
const INTRINSIC_WIDTH: u32 = 40;
const INTRINSIC_HEIGHT: u32 = 30;
/// 那份假位图的字节：一张 `INTRINSIC_WIDTH × INTRINSIC_HEIGHT` 的 24 位 BMP 的文件头。
///
/// 只给头是不够的——`image` 的 BMP 解码器在头之后还要读到声明的像素数据才肯给出尺寸——
/// 所以 [`TestAssets::load`] 在后面再补 0 字节（全黑像素），两者拼起来才是一份完整的文件。
const INTRINSIC_HEADER: [u8; 54] = {
    let width = INTRINSIC_WIDTH.to_le_bytes();
    let height = INTRINSIC_HEIGHT.to_le_bytes();
    let len = INTRINSIC_FILE_LEN as u32;
    let mut bytes = [0_u8; 54];

    bytes[0] = b'B';
    bytes[1] = b'M';
    bytes[2] = len as u8;
    bytes[3] = (len >> 8) as u8;
    bytes[4] = (len >> 16) as u8;
    bytes[5] = (len >> 24) as u8;
    bytes[10] = 54; // 像素数据偏移 = 14（文件头）+ 40（DIB 头）
    bytes[14] = 40; // DIB 头长度 = BITMAPINFOHEADER
    bytes[18] = width[0];
    bytes[19] = width[1];
    bytes[20] = width[2];
    bytes[21] = width[3];
    bytes[22] = height[0];
    bytes[23] = height[1];
    bytes[24] = height[2];
    bytes[25] = height[3];
    bytes[26] = 1; // 颜色平面
    bytes[28] = 24; // 每像素位数

    bytes
};

/// 每行像素补齐到 4 字节之后，一行占多少字节。
const INTRINSIC_STRIDE: usize = (INTRINSIC_WIDTH as usize * 3).div_ceil(4) * 4;
/// 整份假 BMP 的长度：头 + 32 位对齐的像素数据。
const INTRINSIC_FILE_LEN: usize = 54 + INTRINSIC_STRIDE * INTRINSIC_HEIGHT as usize;

impl AssetSource for TestAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path != INTRINSIC_ASSET {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(INTRINSIC_FILE_LEN);
        bytes.extend_from_slice(&INTRINSIC_HEADER);
        bytes.resize(INTRINSIC_FILE_LEN, 0);

        Ok(Some(Cow::Owned(bytes)))
    }

    fn list(&self, _path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// 用 [`TestAssets`] 解析一份 markdown 原文，用例里因此只看见"解析"。
fn parse(source: &str) -> MarkdownDocument {
    parse::parse_document(source, &TestAssets)
}

/// 一个普通的文字段：不加粗、不斜体、不在链接里、不在 kbd 里。
fn text(value: &str) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SharedString::from(value),
        strong: false,
        emphasis: false,
        link: None,
        kbd: None,
    }
}

/// 指定了加粗/斜体的文字段。
fn styled(value: &str, strong: bool, emphasis: bool) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SharedString::from(value),
        strong,
        emphasis,
        link: None,
        kbd: None,
    }
}

/// 归属到链接表第 `id` 条链接的文字段。
fn linked_text(value: &str, id: usize) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SharedString::from(value),
        strong: false,
        emphasis: false,
        link: Some(LinkId(id)),
        kbd: None,
    }
}

/// 归入第 `id` 段 kbd 内容的普通文字段。
fn kbd_text(value: &str, id: usize) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SharedString::from(value),
        strong: false,
        emphasis: false,
        link: None,
        kbd: Some(KbdId(id)),
    }
}

/// 一张写明了尺寸的行内图片：路径与画法由参数给出，宽高都是 `side`。
fn image(path: &str, format: InlineImageFormat, side: Pixels) -> MarkdownPiece {
    sized_image(path, format, size(side, side))
}

/// 一张写明了宽高的行内图片。
fn sized_image(
    path: &str,
    format: InlineImageFormat,
    size: gpui_kit::Size<Pixels>,
) -> MarkdownPiece {
    MarkdownPiece::InlineImage {
        path: SharedString::from(path),
        format,
        size,
        link: None,
        kbd: None,
    }
}

fn empty_document() -> MarkdownDocument {
    MarkdownDocument {
        blocks: Vec::new(),
        links: Vec::new().into(),
    }
}

/// 由若干块直接拼一份文档（没有链接）。
fn document_of(blocks: Vec<MarkdownBlock>) -> MarkdownDocument {
    MarkdownDocument {
        blocks,
        links: Vec::new().into(),
    }
}

/// 一个段落块：对齐方式与片段都由参数给出。
fn paragraph_block(align: ParagraphAlign, pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    MarkdownBlock::Paragraph {
        align,
        pieces: pieces.into(),
    }
}

/// 一个左对齐的段落块（markdown 自己的段落）。
fn left_block(pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    paragraph_block(ParagraphAlign::Left, pieces)
}

/// 一个居中段落块（`<p align="center">` 标签对）。
fn center_block(pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    paragraph_block(ParagraphAlign::Center, pieces)
}

/// 一个标题块：级别与片段都由参数给出。
fn heading_block(level: HeadingLevel, pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    MarkdownBlock::Heading {
        level,
        pieces: pieces.into(),
    }
}

/// 只有一个段落的文档：对齐方式、片段与链接表都由参数给出。
fn one_paragraph(
    align: ParagraphAlign,
    pieces: Vec<MarkdownPiece>,
    links: &[&str],
) -> MarkdownDocument {
    MarkdownDocument {
        blocks: vec![MarkdownBlock::Paragraph {
            align,
            pieces: pieces.into(),
        }],
        links: links.iter().copied().map(SharedString::from).collect(),
    }
}

/// 只有一个左对齐段落的文档（markdown 自己的段落）。
fn paragraph(pieces: Vec<MarkdownPiece>) -> MarkdownDocument {
    one_paragraph(ParagraphAlign::Left, pieces, &[])
}

/// 只有一个居中段落的文档（`<p align="center">` 标签对）。
fn centered(pieces: Vec<MarkdownPiece>) -> MarkdownDocument {
    one_paragraph(ParagraphAlign::Center, pieces, &[])
}

/// 只有一个标题的文档。
fn heading(level: HeadingLevel, pieces: Vec<MarkdownPiece>) -> MarkdownDocument {
    MarkdownDocument {
        blocks: vec![MarkdownBlock::Heading {
            level,
            pieces: pieces.into(),
        }],
        links: Vec::new().into(),
    }
}

/// 两侧各几千个 `*` 的深层嵌套不会打爆递归栈：超过 `MAX_INLINE_NESTING` 的嵌套整棵丢弃，
/// 解析正常返回、没有任何块。回归：这条输入曾让进程以 `STATUS_STACK_OVERFLOW` abort。
#[test]
fn deeply_nested_emphasis_is_dropped_instead_of_overflowing_the_stack() {
    let source = format!("{}a{}", "*".repeat(9_600), "*".repeat(9_600));

    assert_eq!(parse(&source), empty_document());
}

/// 上限之内的嵌套照常解析：两侧各 16 个 `*` 是 8 层嵌套（comrak 实测的深度就是 8），
/// 仍然产出那一段文字，只是带上了逐层累积的加粗 / 斜体样式。
#[test]
fn emphasis_nested_within_the_limit_is_kept() {
    let source = format!("{}a{}", "*".repeat(16), "*".repeat(16));

    assert_eq!(parse(&source), paragraph(vec![styled("a", true, false)]));
}

/// 一个块里所有 `Text` piece 的文字按顺序拼接（`LineBreak` 与图片不贡献文字）。
fn text_of(block: &MarkdownBlock) -> String {
    let pieces = match block {
        MarkdownBlock::Heading { pieces, .. } | MarkdownBlock::Paragraph { pieces, .. } => pieces,
        MarkdownBlock::BlankLine => return String::new(),
    };
    pieces
        .iter()
        .filter_map(|piece| match piece {
            MarkdownPiece::Text { text, .. } => Some(text.as_ref()),
            MarkdownPiece::InlineImage { .. } | MarkdownPiece::LineBreak => None,
        })
        .collect()
}

/// 一侧的星号个数：comrak 每两个 `*` 形成一层 Strong / Emph，所以 128 个 `*` = 64 层。
const LIMIT_STARS: usize = 128;
/// 越过上限一层：130 个 `*` = 65 层。
const OVER_LIMIT_STARS: usize = 130;

/// 行内嵌套的硬上限是 `MAX_INLINE_NESTING = 64`，这里用**硬编码的星号个数**守住边界本身：
/// 断言不引用那个常量，否则把 64 改成 8 或 128 时测试会跟着一起漂移，边界就没有被守住。
///
/// 一侧 128 个 `*` 是 comrak 实测的 64 层嵌套（n=126/127/128 都保留、n=129/130 整棵丢弃），
/// 恰好在上限之内，文字照常产出。
#[test]
fn emphasis_nested_exactly_at_the_limit_is_kept() {
    let source = format!("{}a{}", "*".repeat(LIMIT_STARS), "*".repeat(LIMIT_STARS));
    let document = parse(&source);

    assert_eq!(
        document.blocks.len(),
        1,
        "64 levels of inline nesting must still produce the paragraph"
    );
    assert_eq!(
        text_of(&document.blocks[0]),
        "a",
        "the text inside 64 levels of nesting must survive"
    );
}

/// 越过上限一层（一侧 130 个 `*`）就整棵子树丢弃：`blocks` 为空。
#[test]
fn emphasis_nested_past_the_limit_is_dropped() {
    assert_eq!(
        parse(&format!(
            "{}a{}",
            "*".repeat(OVER_LIMIT_STARS),
            "*".repeat(OVER_LIMIT_STARS)
        )),
        empty_document()
    );
}

/// 被丢弃的嵌套不牵连兄弟内容：只少掉那棵子树里的 `a`，两侧原文照旧。
#[test]
fn dropping_an_over_deep_nesting_keeps_its_siblings() {
    let source = format!("keep {}a{} tail", "*".repeat(200), "*".repeat(200));
    let document = parse(&source);

    assert_eq!(
        document.blocks.len(),
        1,
        "the paragraph around the dropped nesting must survive"
    );
    assert_eq!(
        text_of(&document.blocks[0]),
        "keep  tail",
        "only the dropped subtree's text should be missing (the two spaces come from the source)"
    );
}

#[test]
fn headings_take_their_level_from_the_number_of_hashes() {
    assert_eq!(
        parse("# Title"),
        heading(HeadingLevel::H1, vec![text("Title")])
    );
    assert_eq!(
        parse("###### Title"),
        heading(HeadingLevel::H6, vec![text("Title")])
    );
}

#[test]
fn seven_hashes_are_not_a_heading() {
    assert_eq!(
        parse("####### Title"),
        paragraph(vec![text("####### Title")])
    );
}

#[test]
fn plain_text_is_one_paragraph_and_no_links() {
    let document = paragraph(vec![text("plain")]);

    assert_eq!(parse("plain"), document);
    assert!(document.links.is_empty());
}

#[test]
fn strong_and_emphasis_are_piece_styles() {
    assert_eq!(
        parse("**bold**"),
        paragraph(vec![styled("bold", true, false)])
    );
    assert_eq!(
        parse("*italic*"),
        paragraph(vec![styled("italic", false, true)])
    );
    assert_eq!(
        parse("_italic_"),
        paragraph(vec![styled("italic", false, true)])
    );
    assert_eq!(
        parse("***both***"),
        paragraph(vec![styled("both", true, true)])
    );
}

#[test]
fn neighbouring_text_of_different_styles_is_not_merged() {
    assert_eq!(
        parse("a**b**c"),
        paragraph(vec![text("a"), styled("b", true, false), text("c")])
    );
    assert_eq!(
        parse("a*b*c"),
        paragraph(vec![text("a"), styled("b", false, true), text("c")])
    );
    assert_eq!(
        parse("a**b**"),
        paragraph(vec![text("a"), styled("b", true, false)])
    );
}

#[test]
fn link_text_carries_the_link_id_of_its_url() {
    assert_eq!(
        parse("[x](https://e.com)"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![linked_text("x", 0)],
            &["https://e.com"]
        )
    );
}

#[test]
fn link_style_stops_at_the_end_of_the_link() {
    assert_eq!(
        parse("[x](u) tail"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![linked_text("x", 0), text(" tail")],
            &["u"]
        )
    );
}

#[test]
fn styles_inside_a_link_are_kept() {
    assert_eq!(
        parse("[**b**](u)"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![MarkdownPiece::Text {
                text: SharedString::from("b"),
                strong: true,
                emphasis: false,
                link: Some(LinkId(0)),
                kbd: None,
            }],
            &["u"]
        )
    );
}

/// 一对 `<kbd>` / `</kbd>` 之间的内容带上这一段 kbd 的编号，标签两侧的文字不受影响。
#[test]
fn a_kbd_pair_marks_the_pieces_between_its_tags() {
    assert_eq!(
        parse("<kbd>Ctrl</kbd>"),
        paragraph(vec![kbd_text("Ctrl", 0)])
    );
    assert_eq!(
        parse("press <kbd>Ctrl</kbd> now"),
        paragraph(vec![text("press "), kbd_text("Ctrl", 0), text(" now")])
    );
}

/// 相邻的两对标签是两个独立的行：编号不同，渲染层因此各画一个框。
#[test]
fn adjacent_kbd_pairs_are_separate_runs() {
    assert_eq!(
        parse("<kbd>a</kbd><kbd>b</kbd>"),
        paragraph(vec![kbd_text("a", 0), kbd_text("b", 1)])
    );
}

/// 嵌套的标签只算最外层那一对：内层开关不改变编号，内容因此是一个框。
#[test]
fn nested_kbd_tags_share_the_outermost_run() {
    assert_eq!(
        parse("<kbd>a<kbd>b</kbd>c</kbd>"),
        paragraph(vec![kbd_text("abc", 0)])
    );
}

/// kbd 内容里的行内样式照旧生效，编号与样式两个维度互不覆盖。
#[test]
fn kbd_marks_the_inline_styles_inside_it() {
    assert_eq!(
        parse("<kbd>**bold**</kbd>"),
        paragraph(vec![MarkdownPiece::Text {
            text: SharedString::from("bold"),
            strong: true,
            emphasis: false,
            link: None,
            kbd: Some(KbdId(0)),
        }])
    );
    assert_eq!(
        parse("<kbd>![a](file://icons/phosphoricons/arrow-up.svg)</kbd>"),
        paragraph(vec![MarkdownPiece::InlineImage {
            path: SharedString::from("icons/phosphoricons/arrow-up.svg"),
            format: InlineImageFormat::Svg,
            size: size(px(16.), px(16.)),
            link: None,
            kbd: Some(KbdId(0)),
        }])
    );
}

/// 链接里的 kbd 内容同时属于链接与 kbd 段，两个标记都保留。
#[test]
fn a_link_inside_kbd_keeps_both_marks() {
    assert_eq!(
        parse("<kbd>[x](u)</kbd>"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![MarkdownPiece::Text {
                text: SharedString::from("x"),
                strong: false,
                emphasis: false,
                link: Some(LinkId(0)),
                kbd: Some(KbdId(0)),
            }],
            &["u"]
        )
    );
}

/// 未配对的标签按 comrak 给出的节点顺序处理，不做额外校验：
/// 孤立的 `</kbd>` 是空操作（两侧文字照旧合并），未闭合的 `<kbd>` 一直管到块尾。
#[test]
fn unmatched_kbd_tags_follow_the_nodes_comrak_produces() {
    assert_eq!(parse("a </kbd> b"), paragraph(vec![text("a  b")]));
    assert_eq!(
        parse("press <kbd>Ctrl"),
        paragraph(vec![text("press "), kbd_text("Ctrl", 0)])
    );
}

/// 只有精确的 `<kbd>` 与 `</kbd>` 算 kbd：大小写不同、带属性的写法都只是被忽略的行内 HTML。
#[test]
fn only_the_exact_kbd_tags_count() {
    for source in ["<KBD>x</KBD>", "<kbd class=\"k\">x</kbd>"] {
        assert_eq!(
            parse(source),
            paragraph(vec![text("x")]),
            "{source} should leave the text plain"
        );
    }
}

/// 转义后的标签名不再是 HTML 节点，字面显示、不带 kbd。
#[test]
fn an_escaped_kbd_tag_stays_plain_text() {
    assert_eq!(
        parse("\\<kbd>x\\</kbd>"),
        paragraph(vec![text("<kbd>x</kbd>")])
    );
}

/// 空的一对标签不产出任何片段，整段（空）因此不进 `blocks`。
#[test]
fn an_empty_kbd_pair_produces_no_piece() {
    assert_eq!(parse("<kbd></kbd>"), empty_document());
}

/// kbd 里的软换行照旧是一个块内换行，两侧的文字带着同一个编号。
#[test]
fn a_soft_break_inside_kbd_keeps_the_run_number_on_both_lines() {
    assert_eq!(
        parse("<kbd>a\nb</kbd>"),
        paragraph(vec![
            kbd_text("a", 0),
            MarkdownPiece::LineBreak,
            kbd_text("b", 0),
        ])
    );
}

#[test]
fn a_link_without_content_is_not_recorded() {
    assert_eq!(parse("[]()"), empty_document());
}

/// 一条 `file:` URI 就是一张行内图片：路径去掉 `file://`、画法按扩展名、尺寸按 `size`。
#[test]
fn a_file_image_becomes_a_piece() {
    assert_eq!(
        parse("![a](file://icons/phosphoricons/arrow-up.svg)"),
        paragraph(vec![image(
            "icons/phosphoricons/arrow-up.svg",
            InlineImageFormat::Svg,
            px(16.)
        )])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=24)"),
        paragraph(vec![image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            px(24.)
        )])
    );
}

/// 没写 `size` 的位图用资源自身的尺寸：这里的资源是 [`TestAssets`] 交出来的 40×30 位图，
/// 所以读出来的是那两个数（真资源的同一件事在 `source::tests` 里验）。
#[test]
fn an_image_without_a_size_takes_the_asset_size() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp)"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            size(px(40.), px(30.))
        )])
    );
}

/// `type` 覆盖扩展名；两种取值都允许带双引号。
///
/// 这条走的是**真实的 markdown 解析**（comrak）：`&` 与 `"` 在链接目标里是原样保留的，
/// 所以带引号的写法不会在解析这一步被打散成百分号编码——URI 解析器看到的就是原文。
///
/// 顺带钉住"尺寸与画法无关"：`type=svg` 只是换一种画法，没写 `size` 时仍然按资源自身的
/// 尺寸取值（这里是一张 40×30 的位图，所以是 40×30，而不是 svg 的默认 16）。
#[test]
fn a_type_overrides_the_extension() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?type=svg)"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Svg,
            size(px(INTRINSIC_WIDTH as f32), px(INTRINSIC_HEIGHT as f32))
        )])
    );
    assert_eq!(
        parse("![a](file://icons/phosphoricons/arrow-up.svg?type=\"img\")"),
        paragraph(vec![image(
            "icons/phosphoricons/arrow-up.svg",
            InlineImageFormat::Image,
            px(16.)
        )])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?type=\"img\"&size=24)"),
        paragraph(vec![image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            px(24.)
        )])
    );
}

/// `size` 可以只给一个数（宽高都是它），也可以给 `宽,高`。
#[test]
fn a_size_may_carry_one_number_or_two() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=24)"),
        paragraph(vec![image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            px(24.)
        )])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=24,48)"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            size(px(24.), px(48.))
        )])
    );
}

/// 资源路径里 comrak 不认识的字符原样交给 URI 解析器：`@`（真资源名里就有它）与非 ASCII
/// 都不会被百分号编码或转义，所以 `file://` 后面写什么，资源源就收到什么。
///
/// 这里同时钉住示例文档里那几张 `brands/**/colored@72px.png`：路径里的 `@` 走完整条
/// markdown → URI 的路之后仍然是 `@`。
#[test]
fn a_path_with_at_signs_or_non_ascii_survives_the_markdown_parser() {
    assert_eq!(
        parse("![a](file://brands/Solana/colored@72px.png)"),
        paragraph(vec![image(
            "brands/Solana/colored@72px.png",
            InlineImageFormat::Image,
            px(16.)
        )])
    );
    assert_eq!(
        parse("![a](file://中文/图.png)"),
        paragraph(vec![image(
            "中文/图.png",
            InlineImageFormat::Image,
            px(16.)
        )])
    );
}

/// `size` 的两个值与 `type` 一样可以带双引号，而且这条走的是**真实的 markdown 解析**：
/// comrak 不在链接目标里动引号，所以 `size="24,48"` 到 URI 解析器手里仍然是 `"24,48"`。
#[test]
fn a_quoted_size_survives_the_markdown_parser() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=\"24,48\")"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            size(px(24.), px(48.))
        )])
    );
}

/// 很长的资源路径也是路径：comrak 到 URI 解析器到 IR 一路原样保留，既不截断也不丢 query——
/// 路径与 query 的分界始终是**第一个** `?`，与路径有多长无关。资源找不到，所以按默认边长。
#[test]
fn a_very_long_asset_path_reaches_the_ir_verbatim() {
    let path = format!("{}/icon.svg", "deep-segment/".repeat(200));

    assert_eq!(
        parse(&format!("![a](file://{path})")),
        paragraph(vec![image(&path, InlineImageFormat::Svg, px(16.))])
    );
    assert_eq!(
        parse(&format!("![a](file://{path}?size=24)")),
        paragraph(vec![image(&path, InlineImageFormat::Svg, px(24.))])
    );
}

/// 链接里的图片带着链接的编号：它既是图片，也是链接的一部分（命中与打开由渲染层负责）。
#[test]
fn an_image_inside_a_link_carries_the_link() {
    assert_eq!(
        parse("[![a](file://test/intrinsic.bmp?size=24)](https://example.com)"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![MarkdownPiece::InlineImage {
                path: SharedString::from(INTRINSIC_ASSET),
                format: InlineImageFormat::Image,
                size: size(px(24.), px(24.)),
                link: Some(LinkId(0)),
                kbd: None,
            }],
            &["https://example.com"],
        )
    );
}

/// 用不了的 `size` 回退到资源自身的尺寸（40×30），整条 URI 仍然有效。
#[test]
fn an_unusable_size_falls_back_to_the_asset_size() {
    for value in ["abc", "-3", "1e3", "20px", "1.2.3", "20,"] {
        let source = format!("![a](file://test/intrinsic.bmp?size={value})");

        assert_eq!(
            parse(&source),
            paragraph(vec![sized_image(
                INTRINSIC_ASSET,
                InlineImageFormat::Image,
                size(px(INTRINSIC_WIDTH as f32), px(INTRINSIC_HEIGHT as f32))
            )]),
            "{source} should fall back to the asset size"
        );
    }
}

/// 显式写出的尺寸 clamp 到 1..=256；资源自身的尺寸不 clamp。
#[test]
fn an_explicit_size_is_clamped_to_the_supported_range() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=0)"),
        paragraph(vec![image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            px(1.)
        )])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=9999)"),
        paragraph(vec![image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            px(256.)
        )])
    );
}

/// 不是 `file:` URI 的图片整张忽略（连 alt 文本也不画），而同一段里的 **合法** 图片照常产出。
#[test]
fn unusable_images_leave_no_block_behind() {
    for source in [
        "![a](icon:ArrowUp)",
        "![a](https://e.com/a.png)",
        "![a](file://)",
        "![a](file://a/b.png?type=ico)",
        "![a](file://a/b.png?size=16&size=20)",
        "![a](FILE://a/b.png)",
    ] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} should be ignored"
        );
    }
}

#[test]
fn an_ignored_image_still_splits_the_text_around_it() {
    assert_eq!(
        parse("before ![a](https://e.com/a.png) after"),
        paragraph(vec![text("before "), text(" after")])
    );
}

/// 图片读作空，alt 文本也一样不读：它站在两侧文字的中间，既不贡献字符、也不插入空格，
/// 所以"读"出来的就是两侧文字拼起来的那一个词。
#[test]
fn plain_text_skips_an_image_instead_of_reading_its_alt_text() {
    let document = parse("a![Markets](file://icons/phosphoricons/graph.svg)b");

    assert_eq!(document.plain_text().as_ref(), "ab");
}

#[test]
fn breaks_inside_a_paragraph_become_line_breaks() {
    assert_eq!(
        parse("a\nb"),
        paragraph(vec![text("a"), MarkdownPiece::LineBreak, text("b")])
    );
    assert_eq!(
        parse("a  \nb"),
        paragraph(vec![text("a"), MarkdownPiece::LineBreak, text("b")])
    );
}

/// 一个段落里连写两个硬换行时，**块内**留下一行空行：连着两个 `LineBreak` 片段就是排版层画
/// 空硬换行的依据（见 `inline_flow` 的空硬换行）。
///
/// 这是唯一还会连着出现两个 `LineBreak` 的写法：普通空行结束段落，那些空行是
/// [`MarkdownBlock::BlankLine`]（`a\n\nb` 那条测试钉住的就是它）。标签对里面同样如此——跨过
/// 空行的段落是各自的块，硬换行才留在同一个块里。
#[test]
fn two_hard_breaks_in_a_row_leave_a_blank_line_inside_the_block() {
    let inside = vec![
        text("a"),
        MarkdownPiece::LineBreak,
        MarkdownPiece::LineBreak,
        text("b"),
    ];

    assert_eq!(parse("a\\\n\\\nb"), paragraph(inside.clone()));
    assert_eq!(
        parse("<p align=\"center\">a\\\n\\\nb</p>"),
        centered(inside)
    );
    assert_eq!(
        parse("a\\\n\\\nb").plain_text().as_ref(),
        "a\n\nb",
        "the blank line inside the block reads as a blank line"
    );
}

/// 空行分段，而成段的空行自己也是一个块：源码里的每个空行都产出 [`MarkdownBlock::BlankLine`]。
#[test]
fn a_blank_line_starts_a_new_paragraph() {
    assert_eq!(
        parse("a\n\nb"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

/// 连续几个空行就是几个空行块，不折叠成一个：`a\n\n\n\nb` 比 `a\n\nb` 多两行。
#[test]
fn every_blank_line_is_kept() {
    assert_eq!(
        parse("a\n\n\n\nb"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            MarkdownBlock::BlankLine,
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

/// 只有**真空行**算数：空行按原文里的行数，被丢弃的块级结构占的那一行不算空行。
#[test]
fn only_the_blank_lines_of_the_source_count() {
    assert_eq!(
        parse("a\n\n- item\n\nb"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

/// 只含空白的行也是空行（CommonMark 同样按空行处理）。
#[test]
fn a_line_of_spaces_is_a_blank_line() {
    assert_eq!(
        parse("a\n \nb"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

/// 文档首尾的空行不进 `blocks`：顶部与底部都不留白。
#[test]
fn blank_lines_at_the_edges_of_the_document_are_not_kept() {
    assert_eq!(parse("\n\na"), paragraph(vec![text("a")]));
    assert_eq!(parse("a\n\n\n"), paragraph(vec![text("a")]));
    assert_eq!(
        parse("a\n\nb\n\n"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

/// 空行在 CRLF 与孤立的 CR 下同样成立（行尾口径与 comrak 一致）。
#[test]
fn blank_lines_are_counted_for_every_line_ending() {
    for source in ["a\r\n\r\nb", "a\r\rb"] {
        assert_eq!(
            parse(source),
            document_of(vec![
                left_block(vec![text("a")]),
                MarkdownBlock::BlankLine,
                left_block(vec![text("b")]),
            ]),
            "{source:?} should leave exactly one blank line"
        );
    }
}

/// `<p align="…">…</p>` 标签对自成一段，对齐方式取自 `align`：缺属性时按默认的左对齐。
#[test]
fn an_align_tag_pair_becomes_a_paragraph_with_its_alignment() {
    assert_eq!(
        parse("<p align=\"center\">hello</p>"),
        centered(vec![text("hello")])
    );
    assert_eq!(
        parse("<p align=\"right\">hello</p>"),
        one_paragraph(ParagraphAlign::Right, vec![text("hello")], &[])
    );
    assert_eq!(
        parse("<p align=\"left\">hello</p>"),
        paragraph(vec![text("hello")])
    );
    assert_eq!(
        parse("<p>hello</p>"),
        paragraph(vec![text("hello")]),
        "a `<p>` with no attribute should fall back to the default left alignment"
    );
}

/// 标签对夹在其它块之间时位置不变，也不影响相邻块的对齐：上下两段的空行照旧是空行。
#[test]
fn an_align_tag_pair_keeps_its_place_between_other_blocks() {
    assert_eq!(
        parse("before\n\n<p align=\"center\">middle</p>\n\nafter"),
        document_of(vec![
            left_block(vec![text("before")]),
            MarkdownBlock::BlankLine,
            center_block(vec![text("middle")]),
            MarkdownBlock::BlankLine,
            left_block(vec![text("after")]),
        ])
    );
}

/// 标签对之间照常按行内 markdown 解析：样式、链接、图片与 kbd 一个都不少。
#[test]
fn the_inside_of_an_align_tag_pair_is_inline_markdown() {
    assert_eq!(
        parse(
            "<p align=\"center\">**bold** [x](u) ![a](file://icons/phosphoricons/arrow-up.svg) <kbd>K</kbd></p>"
        ),
        one_paragraph(
            ParagraphAlign::Center,
            vec![
                styled("bold", true, false),
                text(" "),
                linked_text("x", 0),
                text(" "),
                image(
                    "icons/phosphoricons/arrow-up.svg",
                    InlineImageFormat::Svg,
                    px(16.)
                ),
                text(" "),
                kbd_text("K", 0),
            ],
            &["u"],
        )
    );
}

/// 标签对之间可以跨行：开标签后面的换行不算内容，与 markdown 段落一样被剥掉。
#[test]
fn the_inside_of_an_align_tag_pair_may_span_lines() {
    assert_eq!(
        parse("<p align=\"center\">\nhello\n</p>"),
        centered(vec![text("hello")])
    );
    assert_eq!(
        parse("<p align=\"center\">a\nb</p>"),
        centered(vec![text("a"), MarkdownPiece::LineBreak, text("b")])
    );
}

/// 标签对可以跨空行：跨过去的每一段都照常产出自己的块（段落带这一对的对齐），空行照旧是空行块。
///
/// 回归：这个输入曾经只画出 `b`。CommonMark 的 HTML 块到空行就结束，标签对因此没配成对、整块丢弃，
/// 空行**之前**的那半句于是静默消失——产品文案里插进来的值带一个空行就会踩到它。
#[test]
fn an_align_tag_pair_may_span_blank_lines() {
    assert_eq!(
        parse("<p align=\"center\">a\n\nb</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            center_block(vec![text("b")]),
        ])
    );
}

/// 跨空行的标签对里，连续几个空行就是几行空行，一行不少。
#[test]
fn blank_lines_inside_an_align_tag_pair_are_kept() {
    assert_eq!(
        parse("<p align=\"center\">a\n\n\n\nb</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            MarkdownBlock::BlankLine,
            MarkdownBlock::BlankLine,
            center_block(vec![text("b")]),
        ])
    );
}

/// 产品文案的形状：`%{markets}` 那样的插值里带一个空行时，整句照旧画出来。
///
/// 这正是 `common.market_not_loaded` 的形状（"本组件支持的市场参与者有：" 换行、然后一行一个场所名），
/// 场所列表里多一个空行曾经让整句连同列表一起不显示。
#[test]
fn a_blank_line_in_a_tag_pairs_content_keeps_the_whole_message() {
    assert_eq!(
        parse("<p align=\"center\">are:\nBinance\n\nOKX</p>"),
        document_of(vec![
            center_block(vec![
                text("are:"),
                MarkdownPiece::LineBreak,
                text("Binance")
            ]),
            MarkdownBlock::BlankLine,
            center_block(vec![text("OKX")]),
        ])
    );
}

/// 标签对跨过几个段落时，段与段之间的空行照旧按空行算，两段各自带上这一对的对齐。
#[test]
fn an_align_tag_pair_may_span_several_paragraphs() {
    assert_eq!(
        parse("<p align=\"center\">a\n\nb\nc</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            center_block(vec![text("b"), MarkdownPiece::LineBreak, text("c")]),
        ])
    );
}

/// 标签对**首尾**的空行不画：与文档首尾一样，开头的空白也留白给"第一个块"的位置。
#[test]
fn blank_lines_at_the_edges_of_a_tag_pair_are_not_kept() {
    assert_eq!(
        parse("<p align=\"center\">a\n\n</p>"),
        centered(vec![text("a")])
    );
    assert_eq!(
        parse("<p align=\"center\">\n\n\na</p>"),
        centered(vec![text("a")])
    );
}

/// 孤立的 `\r` 也是行尾：标签对里面的空行按它同样算得出来（行尾口径与 comrak 一致）。
#[test]
fn blank_lines_inside_a_pair_are_counted_for_every_line_ending() {
    assert_eq!(
        parse("<p align=\"center\">a\r\rb</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            center_block(vec![text("b")]),
        ])
    );
}

/// 几对标签之间隔着空行：空行照旧是空行，各对各自成段。
#[test]
fn blank_lines_between_align_tag_pairs_are_kept() {
    assert_eq!(
        parse("<p align=\"center\">a</p>\n\n<p align=\"right\">b</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            paragraph_block(ParagraphAlign::Right, vec![text("b")]),
        ])
    );
}

/// 一个 HTML 块里，闭标签**之后**写的下一个开标签照旧算数：吃掉上一个标签对的那段内容不该把
/// 这一对一起丢掉。
///
/// 这里让闭标签与下一个开标签落在**同一个** HTML 块里（两行之间没有空行，所以它们是一块），
/// 而那一块的起点（`</p>` 那一行）已经落在上一对吃掉的区域里——不把 HTML 块节点也走一遍，
/// 第二对就再没人认领了。
#[test]
fn a_pair_after_a_consumed_close_tag_is_still_drawn() {
    assert_eq!(
        parse("<p align=\"center\">a\n\n</p>\n<p align=\"right\">c</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            paragraph_block(ParagraphAlign::Right, vec![text("c")]),
        ])
    );
}

/// 顶层 HTML 块里的开标签，写在行首还是跟在块内别的标签后面都算元素：行首只是常见写法。
///
/// 回归：这种"从行中间开始"的标签对跨空行往后配对时，被它跨过的**标题**曾经连一个字都不剩
/// （内容里的块级结构照旧丢弃）。现在内容照常产出块，所以跨过去的标题照旧画出来。
#[test]
fn an_open_tag_inside_an_html_block_is_a_pair() {
    assert_eq!(
        parse("<div>x <p align=\"center\">y</p></div>"),
        centered(vec![text("y")])
    );
    assert_eq!(
        parse("<div>x <p align=\"center\">y"),
        empty_document(),
        "an unpaired tag still leaves nothing behind"
    );
    assert_eq!(
        parse("<div>x <p align=\"center\">y\n\n# Heading\n\nmore </p>"),
        document_of(vec![
            center_block(vec![text("y")]),
            MarkdownBlock::BlankLine,
            heading_block(HeadingLevel::H1, vec![text("Heading")]),
            MarkdownBlock::BlankLine,
            center_block(vec![text("more")]),
        ]),
        "the blocks the pair spans are still drawn: the heading is not lost"
    );
    assert_eq!(
        parse("<p align=\"center\">a</p><p align=\"right\">b</p>"),
        document_of(vec![
            center_block(vec![text("a")]),
            paragraph_block(ParagraphAlign::Right, vec![text("b")]),
        ]),
        "both pairs of that line are elements of the same HTML block"
    );
    assert_eq!(
        parse("<div>\n    <p align=\"center\">x</p>\n</div>"),
        centered(vec![text("x")])
    );
}

/// 一个没配对的开标签不会把后面那一对吞掉：跨空行往后找闭标签时，下一个 HTML 块里的开标签就是
/// 终点（见 `pair_bound`），所以没配对的整块丢弃、后面配好的照旧显示。
#[test]
fn an_unpaired_open_tag_does_not_swallow_the_pair_after_it() {
    assert_eq!(
        parse("<p align=\"center\">a\n\n<p align=\"right\">b</p>"),
        one_paragraph(ParagraphAlign::Right, vec![text("b")], &[]),
        "the pair that is closed should survive the one that is not"
    );
}

/// 同一个 HTML 块里连写的开标签仍按块内规则处理：先写的赢，内容到第一个 `</p>` 为止，后面那个
/// 开标签落在内容里（HTML 里 `<p>` 不能嵌套，所以它那一行按内容丢弃）。
#[test]
fn a_supported_open_tag_nested_in_one_block_is_content() {
    assert_eq!(
        parse("<p align=\"center\">a\n<p align=\"right\">b</p>"),
        centered(vec![text("a")])
    );
}

/// 标签对内容里组件本来就不画的块级结构（代码块、列表、别的 HTML 块）照旧连子树丢弃，
/// 于是整段没有片段、也不留空块。
#[test]
fn block_constructs_inside_an_align_tag_pair_are_dropped() {
    for source in [
        "<p align=\"center\">```\ncode\n```</p>",
        "<p align=\"center\"><div>x</div></p>",
        "<p align=\"center\">1. item</p>",
    ] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} has nothing this component draws"
        );
    }
}

/// 标签对内容里的标题照旧是标题：段落带这一对的对齐，标题按它自己的级别画（标题在本组件里恒为
/// 左对齐）。Setext 标题（`===` 那一条）同样算标题。
#[test]
fn headings_inside_an_align_tag_pair_are_drawn() {
    assert_eq!(
        parse("<p align=\"center\"># Title</p>"),
        document_of(vec![heading_block(HeadingLevel::H1, vec![text("Title")])])
    );
    assert_eq!(
        parse("<p align=\"center\">## Title</p>"),
        document_of(vec![heading_block(HeadingLevel::H2, vec![text("Title")])])
    );
    assert_eq!(
        parse("<p align=\"center\">a\n===</p>"),
        document_of(vec![heading_block(HeadingLevel::H1, vec![text("a")])])
    );
}

/// 不是标签的 `<p …>` 不生效：注释里、原始文本块（`<pre>` / `<script>` / `<style>` /
/// `<textarea>`）里与属性值里的那对都不是元素，一个字也不显示。
///
/// 属性值那一条特意让引号在上一行开、下一行合：只有"被引号包住的属性值整体跳过"这条规则能让它
/// 不算数（`<p …>` 在原文里确实开了一行）。
#[test]
fn a_tag_pair_that_is_not_markup_is_not_an_align_tag() {
    for source in [
        "<!-- <p align=\"center\">hidden</p> -->",
        "<!--\n<p align=\"center\">hidden</p>\n-->",
        "<pre>\n<p align=\"center\">hidden</p>\n</pre>",
        "<script>\n<p align=\"center\">hidden</p>\n</script>",
        "<style>\n<p align=\"center\">hidden</p>\n</style>",
        "<textarea>\n<p align=\"center\">hidden</p>\n</textarea>",
        "<div title=\"x <p align=center>y</p>\">z</div>",
        "<div title=\"\n<p align=\"center\">hidden</p>\n\">z</div>",
        "<div>\n<!-- <p align=\"center\">hidden</p> -->\n</div>",
    ] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} contains no `<p …>` element, so nothing should be drawn"
        );
    }
}

/// 注释与属性值挡不住真正的标签对：同一个 HTML 块里，标签对之外的注释照旧不显示。
#[test]
fn a_comment_beside_an_align_tag_pair_does_not_hide_it() {
    assert_eq!(
        parse("<div>\n<!-- note -->\n<p align=\"center\">x</p>\n</div>"),
        centered(vec![text("x")])
    );
}

/// 注释整体跳过：里面的引号（`don't`）与 `>` 都不会把后面的原文带偏，注释之后的标签对照旧生效。
#[test]
fn a_comment_does_not_disturb_the_align_tags_after_it() {
    assert_eq!(
        parse("<div>\n<!-- don't -->\n<p align=\"center\">x</p>\n</div>"),
        centered(vec![text("x")]),
        "an apostrophe inside a comment must not end the scan"
    );
    assert_eq!(
        parse(
            "<div>\n<!-- > <p align=\"center\">hidden</p> -->\n<p align=\"center\">x</p>\n</div>"
        ),
        centered(vec![text("x")]),
        "a `>` inside a comment must not end the comment early"
    );
    assert_eq!(
        parse("<p align=\"center\">a</p>\n<!-- don't remove -->\n<p align=\"center\">b</p>"),
        MarkdownDocument {
            blocks: vec![
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Center,
                    pieces: vec![text("a")].into(),
                },
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Center,
                    pieces: vec![text("b")].into(),
                },
            ],
            links: Vec::new().into(),
        }
    );
}

/// 注释里的 `</p>` 不是闭标签：注释整体跳过，标签对在真正的 `</p>` 上闭合，注释本身不显示。
#[test]
fn a_close_tag_inside_a_comment_does_not_end_the_pair() {
    assert_eq!(
        parse("<p align=\"center\">a<!-- </p> -->b</p>"),
        centered(vec![text("ab")])
    );
}

/// HTML 允许的空注释 `<!-->` 与 `<!--->` 也是一段注释：它们到此为止，不吃掉后面的原文。
#[test]
fn an_abbreviated_comment_does_not_swallow_the_tags_after_it() {
    for comment in ["<!-->", "<!--->"] {
        assert_eq!(
            parse(&format!(
                "<div>\n{comment}\n<p align=\"center\">x</p>\n</div>"
            )),
            centered(vec![text("x")]),
            "{comment} should end where it starts"
        );
    }
}

/// 没闭合的注释一直吃到原文结尾，而原文可以不以换行结尾、最后一个字符还可以是多字节：扫描器
/// 不能按"最后一个字节"反推标签内容（那会切在字符中间 panic）。回归：这两条输入曾经 panic。
#[test]
fn an_unterminated_comment_at_the_end_of_a_multibyte_source_does_not_panic() {
    assert_eq!(
        parse("<p align=\"center\">x</p>\n<!-- 中"),
        centered(vec![text("x")])
    );
    assert_eq!(parse("<div>\n<!-- 中"), empty_document());
}

/// 标签对的内容一直到**第一个** `</p>` 为止，正文里的小于号不算标签：`if a<b then c` 与
/// `a <b` 都照常显示，后面的标签对也一个不少。
#[test]
fn a_stray_less_than_sign_does_not_swallow_the_closing_tag() {
    assert_eq!(
        parse("<p align=\"center\">if a<b then c</p>"),
        centered(vec![text("if a<b then c")])
    );
    assert_eq!(
        parse("<p align=\"center\">a <b</p>"),
        centered(vec![text("a <b")])
    );
    assert_eq!(
        parse("<p align=\"center\">a <b</p>\n<p align=\"right\">c</p>"),
        MarkdownDocument {
            blocks: vec![
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Center,
                    pieces: vec![text("a <b")].into(),
                },
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Right,
                    pieces: vec![text("c")].into(),
                },
            ],
            links: Vec::new().into(),
        }
    );
}

/// 标签对之间被块级结构打断时，两边的内容照常产出自己的块，中间的标题照旧是标题：
/// `# 标题` 不需要空行就能打断段落，所以这里的 `after` 不是空谈。
#[test]
fn a_heading_inside_an_align_tag_pair_is_still_a_heading() {
    assert_eq!(
        parse("<p align=\"center\">before\n# H\nafter</p>"),
        document_of(vec![
            center_block(vec![text("before")]),
            heading_block(HeadingLevel::H1, vec![text("H")]),
            center_block(vec![text("after")]),
        ]),
        "the heading the pair spans is drawn as a heading, and the paragraphs around it keep the pair's alignment"
    );
}

/// 标签名必须恰好是 `p`：`<palign="center">` 这种名字里带着 `align` 的写法不是 `<p …>`。
/// （它得裹在别的 HTML 块里才走得进扫描器：单独一行时 comrak 根本不把它当标签。）
#[test]
fn a_tag_name_that_only_starts_with_p_is_not_the_align_tag() {
    assert_eq!(
        parse("<div>\n<palign=\"center\">x</p>\n</div>"),
        empty_document()
    );
}

/// 一个 HTML 块里可以连写几对标签：各成一个段落，块里其余内容照旧不显示。
#[test]
fn several_align_tag_pairs_in_one_html_block_become_several_paragraphs() {
    assert_eq!(
        parse("<p align=\"center\">a</p>\n<p align=\"right\">b</p>"),
        MarkdownDocument {
            blocks: vec![
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Center,
                    pieces: vec![text("a")].into(),
                },
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Right,
                    pieces: vec![text("b")].into(),
                },
            ],
            links: Vec::new().into(),
        }
    );
    assert_eq!(
        parse("<p align=\"center\">a</p>\ntail"),
        centered(vec![text("a")]),
        "the rest of the HTML block is still raw HTML and stays hidden"
    );
}

/// 标签对的内容一直到**第一个** `</p>` 为止：中间出现的 `<p …>`（写法不合法、或者本来就是
/// HTML 里不允许的嵌套）不是新的标签对，它落在内容里——内容按 markdown 解析，这种以标签开头的
/// 行是一整块原始 HTML，连它后面的文字一起丢掉（见 `pair_blocks`）。后面配好的标签对
/// 照旧各成一个段落，一个都不少。
#[test]
fn the_content_of_an_align_tag_pair_ends_at_the_first_close_tag() {
    assert_eq!(
        parse("<p align=\"center\">a\n<p class=\"x\">b</p>\n<p align=\"right\">c</p>"),
        MarkdownDocument {
            blocks: vec![
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Center,
                    pieces: vec![text("a")].into(),
                },
                MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Right,
                    pieces: vec![text("c")].into(),
                },
            ],
            links: Vec::new().into(),
        }
    );
}

/// 只有精确的写法算标签对：大小写、单引号、不加引号、属性里的空格、别的属性、重复属性、
/// 闭合标签的其它写法，以及没闭合的标签对，全都不生效（整块不显示）。
#[test]
fn only_the_exact_align_tag_pair_counts() {
    for source in [
        "<P align=\"center\">x</P>",
        "<p align='center'>x</p>",
        "<p align=center>x</p>",
        "<p align = \"center\">x</p>",
        "<p class=\"x\">x</p>",
        "<p align=\"center\" class=\"x\">x</p>",
        "<p align=\"center\" align=\"left\">x</p>",
        "<p align=\"justify\">x</p>",
        "<p align=\"center\">x",
        "<p align=\"center\">x</P>",
        "<p align=\"center\">x</p >",
    ] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} is not the supported form, so it should be dropped whole"
        );
    }
}

/// 标签对外面裹着别的 HTML 标签也算：一个 HTML 块里只看标签对，其余部分（含那层 `<div>`）丢弃。
#[test]
fn an_align_tag_pair_wrapped_in_other_html_still_counts() {
    assert_eq!(
        parse("<div>\n<p align=\"center\">x</p>\n</div>"),
        centered(vec![text("x")])
    );
}

/// 没有闭合标签的开标签一个都不生效：`</p>` 在剩下的原文里找不到，后面的开标签也配不成对；
/// 但已经配好的标签对不受影响。
#[test]
fn an_open_align_tag_without_a_closing_tag_produces_nothing() {
    assert_eq!(
        parse("<p align=\"center\">a\n<p align=\"right\">b"),
        empty_document()
    );
    assert_eq!(
        parse("<p align=\"center\">a</p>tail<p align=\"right\">b"),
        centered(vec![text("a")])
    );
}

/// `<p` 开头的别的标签名不是标签对：`<pre>`、`<param>` 都含 `<p`，但都不该被当成对齐标签。
#[test]
fn a_tag_whose_name_merely_starts_with_p_is_not_an_align_tag() {
    for source in ["<pre>x</pre>", "<param>x</param>"] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} is not a `<p …>` tag"
        );
    }
}

/// 空的一对标签不产出任何片段，整段（空）因此不进 `blocks`；它两侧的空行也照旧不画——空标签对
/// 不是一个"真的产出块"的块级结构，它前面第一个块仍然是文档的第一个块。
#[test]
fn an_empty_align_tag_pair_produces_no_block() {
    for source in ["<p align=\"center\"></p>", "<p align=\"center\">   </p>"] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} has no content, so it should leave no block behind"
        );
    }
    assert_eq!(
        parse("<p align=\"center\"></p>\n\na"),
        paragraph(vec![text("a")]),
        "the blank line in front of the first real block is not drawn"
    );
}

/// 标签对写在段落中间时不生效：CommonMark 的 HTML 块必须从行首开始，段落里的 `<p …>` 只是
/// 行内 HTML，按现有规则忽略，两侧的文字照旧合并成一段。
#[test]
fn an_inline_align_tag_changes_nothing() {
    assert_eq!(
        parse("text <p align=\"center\">x</p>"),
        paragraph(vec![text("text x")])
    );
}

#[test]
fn dropped_block_nodes_leave_no_block_behind() {
    let fenced_code = "```\ncode\n```";

    for source in ["- item", "> quote", fenced_code, "<div>x</div>"] {
        assert_eq!(
            parse(source),
            empty_document(),
            "{source} should be dropped with its whole subtree"
        );
    }
}

#[test]
fn strikethrough_is_plain_text_without_the_extension() {
    assert_eq!(parse("~~x~~"), paragraph(vec![text("~~x~~")]));
}

#[test]
fn an_empty_source_has_no_blocks_and_no_links() {
    assert_eq!(parse(""), empty_document());
}

#[test]
fn the_same_source_parses_to_the_same_document() {
    let source = "# Title\n\n**bold** and [a link](https://e.com) and ![an image](file://test/intrinsic.bmp?size=24)\n\n- dropped";

    assert_eq!(parse(source), parse(source));
}

#[test]
fn push_text_merges_one_style_and_lands_it_before_a_style_change() {
    let plain = parse::PieceStyle {
        strong: false,
        emphasis: false,
        link: None,
        kbd: None,
    };
    let bold = parse::PieceStyle {
        strong: true,
        ..plain
    };
    let mut buffer = None;
    let mut out = Vec::new();

    parse::push_text(&mut buffer, &mut out, "a", &plain);
    parse::push_text(&mut buffer, &mut out, "b", &plain);
    assert!(out.is_empty(), "merging happens when the buffer lands");
    assert_eq!(
        buffer.as_ref().map(|buffer| buffer.text.as_str()),
        Some("ab")
    );

    parse::push_text(&mut buffer, &mut out, "c", &bold);
    assert_eq!(out, vec![text("ab")]);
    assert_eq!(
        buffer.as_ref().map(|buffer| buffer.text.as_str()),
        Some("c")
    );

    parse::push_text(&mut buffer, &mut out, "", &bold);
    assert_eq!(
        buffer.as_ref().map(|buffer| buffer.text.as_str()),
        Some("c"),
        "empty text never reaches the buffer"
    );

    parse::flush(&mut buffer, &mut out);
    assert!(buffer.is_none());
    assert_eq!(out, vec![text("ab"), styled("c", true, false)]);
}

#[test]
fn flush_without_a_buffer_does_nothing() {
    let mut buffer = None;
    let mut out = Vec::new();

    parse::flush(&mut buffer, &mut out);
    parse::flush(&mut buffer, &mut out);

    assert!(out.is_empty());
}

#[test]
fn flush_lands_a_buffer_built_by_hand() {
    let mut buffer = Some(parse::TextBuffer {
        text: String::from("x"),
        style: parse::PieceStyle {
            strong: false,
            emphasis: true,
            link: Some(LinkId(1)),
            kbd: Some(KbdId(2)),
        },
    });
    let mut out = Vec::new();

    parse::flush(&mut buffer, &mut out);

    assert!(buffer.is_none());
    assert_eq!(
        out,
        vec![MarkdownPiece::Text {
            text: SharedString::from("x"),
            strong: false,
            emphasis: true,
            link: Some(LinkId(1)),
            kbd: Some(KbdId(2)),
        }]
    );
}

/// kbd 编号一变，缓冲就先落盘：框两侧的文字不会被合并成一段。
#[test]
fn a_kbd_change_lands_the_buffer_before_the_new_run() {
    let plain = parse::PieceStyle {
        strong: false,
        emphasis: false,
        link: None,
        kbd: None,
    };
    let marked = parse::PieceStyle {
        kbd: Some(KbdId(0)),
        ..plain
    };
    let mut buffer = None;
    let mut out = Vec::new();

    parse::push_text(&mut buffer, &mut out, "a", &plain);
    parse::push_text(&mut buffer, &mut out, "b", &marked);

    assert_eq!(out, vec![text("a")]);
    parse::flush(&mut buffer, &mut out);
    assert_eq!(out, vec![text("a"), kbd_text("b", 0)]);
}

#[test]
fn with_link_stamps_text_and_image_and_leaves_a_line_break_alone() {
    assert_eq!(text("x").with_link(LinkId(2)), linked_text("x", 2));
    assert_eq!(
        image("a/b.png", InlineImageFormat::Image, px(24.)).with_link(LinkId(2)),
        MarkdownPiece::InlineImage {
            path: SharedString::from("a/b.png"),
            format: InlineImageFormat::Image,
            size: size(px(24.), px(24.)),
            link: Some(LinkId(2)),
            kbd: None,
        }
    );
    // kbd 编号与链接互不覆盖：加链接时原有编号原样保留。
    assert_eq!(
        kbd_text("x", 3).with_link(LinkId(2)),
        MarkdownPiece::Text {
            text: SharedString::from("x"),
            strong: false,
            emphasis: false,
            link: Some(LinkId(2)),
            kbd: Some(KbdId(3)),
        }
    );
    assert_eq!(
        MarkdownPiece::LineBreak.with_link(LinkId(2)),
        MarkdownPiece::LineBreak
    );
}

/// 一段确定性的组合扫描：同一个假随机序列把上面那些形状（段落、空行、标签对的开/闭、注释、列表、
/// 标题、`<div>`、`</kbd>`）拼成上万份文档，只钉两条**对任何输入都该成立**的性质。
///
/// 手写用例挑不出这些组合的交叉点，而"标签对吃进哪些节点、空行怎么补"正是按节点/偏移记账的：
/// 记错一步就会把同一段文字画两遍，或者把整块吃掉。序列是定死的（xorshift 的种子写死），所以这
/// 条测试每次跑的都是同一批文档。
#[test]
fn a_sweep_of_shape_combinations_never_draws_a_line_twice() {
    const LINES: &[&str] = &[
        "",
        "TEXT",
        "MID",
        "<div>x <p align=\"center\">INLINE",
        "<p align=\"center\">OPEN",
        "<p align=\"right\">OPEN",
        "<p>OPEN",
        "</p>",
        "TAIL</p>",
        "- ITEM",
        "# HEAD",
        "<div>DIV</div>",
        "<!-- </p> -->",
        "<kbd>KEY</kbd>",
    ];

    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for case in 0..4_000u64 {
        let count = 1 + (next() % 8) as usize;
        let mut source = String::new();
        // 每一行的正文都带一个只属于这一行的记号：重复出现就是"同一行被画了两遍"。
        let mut tokens = Vec::new();
        for line in 0..count {
            let pick = LINES[(next() % LINES.len() as u64) as usize];
            if line > 0 {
                source.push('\n');
            }
            if pick.is_empty() {
                continue;
            }
            let token = format!("Z{case}x{line}");
            tokens.push(token.clone());
            match pick {
                "</p>" => source.push_str(&format!("</p>{token}")),
                "TAIL</p>" => source.push_str(&format!("{token}</p>")),
                _ => source.push_str(&format!("{pick}{token}")),
            }
        }

        let document = parse(&source);
        let mut drawn = String::new();
        for block in &document.blocks {
            match block {
                MarkdownBlock::Heading { pieces, .. } | MarkdownBlock::Paragraph { pieces, .. } => {
                    for piece in pieces.iter() {
                        if let MarkdownPiece::Text { text, .. } = piece {
                            drawn.push_str(text);
                        }
                    }
                }
                MarkdownBlock::BlankLine => {}
            }
        }

        for token in tokens {
            assert!(
                drawn.matches(&token).count() <= 1,
                "{token} was drawn more than once for {source:?} -> {drawn:?}"
            );
        }
    }
}

/// 只有段落与空行的文档：每一行必须刚好读出来一次——空行记账不能把块整个吃掉或重复一次。
///
/// 同一段组合扫描里，这是"丢内容"的那一半：那一半的字母表里没有丢内容的块（列表、标题、标签对
/// 都不是），所以"每行各一次"对任何组合都成立。
#[test]
fn a_sweep_of_paragraphs_and_blank_lines_keeps_every_line_once() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for case in 0..1_000u64 {
        let count = 1 + (next() % 6) as usize;
        let mut source = String::new();
        let mut tokens = Vec::new();
        for line in 0..count {
            if line > 0 {
                source.push('\n');
            }
            if next() % 2 == 0 {
                continue;
            }
            let token = format!("P{case}x{line}");
            tokens.push(token.clone());
            source.push_str(&token);
        }

        let plain = parse(&source).plain_text();
        for token in tokens {
            assert_eq!(
                plain.matches(&token).count(),
                1,
                "{token} should be read exactly once for {source:?} -> {plain:?}"
            );
        }
    }
}

/// 只有标签对与空行的文档：每一对必须刚好画出来一次——配对与消费不能漏掉或重复任何一对。
#[test]
fn a_sweep_of_tag_pairs_and_blank_lines_keeps_every_pair_once() {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for case in 0..1_000u64 {
        let count = 1 + (next() % 6) as usize;
        let mut source = String::new();
        let mut tokens = Vec::new();
        for line in 0..count {
            if line > 0 {
                source.push('\n');
            }
            if next() % 2 == 0 {
                continue;
            }
            let token = format!("Q{case}x{line}");
            tokens.push(token.clone());
            source.push_str(&format!("<p align=\"center\">{token}</p>"));
        }

        let plain = parse(&source).plain_text();
        for token in tokens {
            assert_eq!(
                plain.matches(&token).count(),
                1,
                "{token} should be drawn exactly once for {source:?} -> {plain:?}"
            );
        }
    }
}
