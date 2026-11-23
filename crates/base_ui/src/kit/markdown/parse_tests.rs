use std::borrow::Cow;

use smol_str::SmolStr;
use ui_core::math::Size;

use super::parse;
use crate::kit::markdown::{
    document::{
        HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownListItem,
        MarkdownPiece, MarkdownTableCell, MarkdownTableRow, ParagraphAlign,
    },
    source::{InlineImageFormat, MarkdownAssets},
};

struct TestAssets;

const INTRINSIC_ASSET: &str = "test/intrinsic.bmp";
const INTRINSIC_WIDTH: u32 = 40;
const INTRINSIC_HEIGHT: u32 = 30;
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
    bytes[10] = 54;
    bytes[14] = 40;
    bytes[18] = width[0];
    bytes[19] = width[1];
    bytes[20] = width[2];
    bytes[21] = width[3];
    bytes[22] = height[0];
    bytes[23] = height[1];
    bytes[24] = height[2];
    bytes[25] = height[3];
    bytes[26] = 1;
    bytes[28] = 24;

    bytes
};

const INTRINSIC_STRIDE: usize = (INTRINSIC_WIDTH as usize * 3).div_ceil(4) * 4;
const INTRINSIC_FILE_LEN: usize = 54 + INTRINSIC_STRIDE * INTRINSIC_HEIGHT as usize;

impl MarkdownAssets for TestAssets {
    fn load(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        if path != INTRINSIC_ASSET {
            return None;
        }
        let mut bytes = Vec::with_capacity(INTRINSIC_FILE_LEN);
        bytes.extend_from_slice(&INTRINSIC_HEADER);
        bytes.resize(INTRINSIC_FILE_LEN, 0);

        Some(Cow::Owned(bytes))
    }
}

fn parse(source: &str) -> MarkdownDocument {
    parse::parse_document(source, &TestAssets)
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

fn styled(value: &str, strong: bool, emphasis: bool) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SmolStr::new(value),
        strong,
        emphasis,
        strike: false,
        code: false,
        link: None,
        kbd: None,
    }
}

fn strike_text(value: &str) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SmolStr::new(value),
        strong: false,
        emphasis: false,
        strike: true,
        code: false,
        link: None,
        kbd: None,
    }
}

fn code_text(value: &str) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SmolStr::new(value),
        strong: false,
        emphasis: false,
        strike: false,
        code: true,
        link: None,
        kbd: None,
    }
}

fn linked_text(value: &str, id: usize) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SmolStr::new(value),
        strong: false,
        emphasis: false,
        strike: false,
        code: false,
        link: Some(LinkId(id)),
        kbd: None,
    }
}

fn kbd_text(value: &str, id: usize) -> MarkdownPiece {
    MarkdownPiece::Text {
        text: SmolStr::new(value),
        strong: false,
        emphasis: false,
        strike: false,
        code: false,
        link: None,
        kbd: Some(KbdId(id)),
    }
}

fn image(path: &str, format: InlineImageFormat, side: f32) -> MarkdownPiece {
    sized_image(path, format, Size::new(side, side))
}

fn sized_image(path: &str, format: InlineImageFormat, size: Size) -> MarkdownPiece {
    MarkdownPiece::InlineImage {
        path: SmolStr::new(path),
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

fn document_of(blocks: Vec<MarkdownBlock>) -> MarkdownDocument {
    MarkdownDocument {
        blocks,
        links: Vec::new().into(),
    }
}

fn paragraph_block(align: ParagraphAlign, pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    MarkdownBlock::Paragraph {
        align,
        pieces: pieces.into(),
    }
}

fn left_block(pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    paragraph_block(ParagraphAlign::Left, pieces)
}

fn center_block(pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    paragraph_block(ParagraphAlign::Center, pieces)
}

fn heading_block(level: HeadingLevel, pieces: Vec<MarkdownPiece>) -> MarkdownBlock {
    MarkdownBlock::Heading {
        level,
        pieces: pieces.into(),
    }
}

fn code_block(language: Option<&str>, code: &str) -> MarkdownBlock {
    MarkdownBlock::CodeBlock {
        language: language.map(SmolStr::new),
        code: SmolStr::new(code),
    }
}

fn quote_block(blocks: Vec<MarkdownBlock>) -> MarkdownBlock {
    MarkdownBlock::Quote {
        blocks: blocks.into(),
    }
}

fn item(blocks: Vec<MarkdownBlock>) -> MarkdownListItem {
    MarkdownListItem {
        blocks: blocks.into(),
    }
}

fn list_block(ordered: bool, start: u64, items: Vec<MarkdownListItem>) -> MarkdownBlock {
    MarkdownBlock::List {
        ordered,
        start,
        items: items.into(),
    }
}

fn table_cell(align: ParagraphAlign, pieces: Vec<MarkdownPiece>) -> MarkdownTableCell {
    MarkdownTableCell {
        align,
        pieces: pieces.into(),
    }
}

fn table_row(header: bool, cells: Vec<MarkdownTableCell>) -> MarkdownTableRow {
    MarkdownTableRow {
        header,
        cells: cells.into(),
    }
}

fn table_block(rows: Vec<MarkdownTableRow>) -> MarkdownBlock {
    MarkdownBlock::Table { rows: rows.into() }
}

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
        links: links.iter().copied().map(SmolStr::new).collect(),
    }
}

fn paragraph(pieces: Vec<MarkdownPiece>) -> MarkdownDocument {
    one_paragraph(ParagraphAlign::Left, pieces, &[])
}

fn centered(pieces: Vec<MarkdownPiece>) -> MarkdownDocument {
    one_paragraph(ParagraphAlign::Center, pieces, &[])
}

fn heading(level: HeadingLevel, pieces: Vec<MarkdownPiece>) -> MarkdownDocument {
    MarkdownDocument {
        blocks: vec![MarkdownBlock::Heading {
            level,
            pieces: pieces.into(),
        }],
        links: Vec::new().into(),
    }
}

#[test]
fn deeply_nested_emphasis_is_dropped_instead_of_overflowing_the_stack() {
    let source = format!("{}a{}", "*".repeat(9_600), "*".repeat(9_600));

    assert_eq!(parse(&source), empty_document());
}

#[test]
fn emphasis_nested_within_the_limit_is_kept() {
    let source = format!("{}a{}", "*".repeat(16), "*".repeat(16));

    assert_eq!(parse(&source), paragraph(vec![styled("a", true, false)]));
}

fn text_of(block: &MarkdownBlock) -> String {
    let Some(pieces) = block.pieces() else {
        return String::new();
    };
    pieces
        .iter()
        .filter_map(|piece| match piece {
            MarkdownPiece::Text { text, .. } => Some(text.as_str()),
            MarkdownPiece::InlineImage { .. } | MarkdownPiece::LineBreak => None,
        })
        .collect()
}

const LIMIT_STARS: usize = 128;
const OVER_LIMIT_STARS: usize = 130;

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
                text: SmolStr::new_static("b"),
                strong: true,
                emphasis: false,
                strike: false,
                code: false,
                link: Some(LinkId(0)),
                kbd: None,
            }],
            &["u"]
        )
    );
}

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

#[test]
fn adjacent_kbd_pairs_are_separate_runs() {
    assert_eq!(
        parse("<kbd>a</kbd><kbd>b</kbd>"),
        paragraph(vec![kbd_text("a", 0), kbd_text("b", 1)])
    );
}

#[test]
fn nested_kbd_tags_share_the_outermost_run() {
    assert_eq!(
        parse("<kbd>a<kbd>b</kbd>c</kbd>"),
        paragraph(vec![kbd_text("abc", 0)])
    );
}

#[test]
fn kbd_marks_the_inline_styles_inside_it() {
    assert_eq!(
        parse("<kbd>**bold**</kbd>"),
        paragraph(vec![MarkdownPiece::Text {
            text: SmolStr::new_static("bold"),
            strong: true,
            emphasis: false,
            strike: false,
            code: false,
            link: None,
            kbd: Some(KbdId(0)),
        }])
    );
    assert_eq!(
        parse("<kbd>![a](file://icons/phosphoricons/arrow-up.svg)</kbd>"),
        paragraph(vec![MarkdownPiece::InlineImage {
            path: SmolStr::new_static("icons/phosphoricons/arrow-up.svg"),
            format: InlineImageFormat::Svg,
            size: Size::new(16., 16.),
            link: None,
            kbd: Some(KbdId(0)),
        }])
    );
}

#[test]
fn a_link_inside_kbd_keeps_both_marks() {
    assert_eq!(
        parse("<kbd>[x](u)</kbd>"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![MarkdownPiece::Text {
                text: SmolStr::new_static("x"),
                strong: false,
                emphasis: false,
                strike: false,
                code: false,
                link: Some(LinkId(0)),
                kbd: Some(KbdId(0)),
            }],
            &["u"]
        )
    );
}

#[test]
fn unmatched_kbd_tags_follow_the_nodes_comrak_produces() {
    assert_eq!(parse("a </kbd> b"), paragraph(vec![text("a  b")]));
    assert_eq!(
        parse("press <kbd>Ctrl"),
        paragraph(vec![text("press "), kbd_text("Ctrl", 0)])
    );
}

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

#[test]
fn an_escaped_kbd_tag_stays_plain_text() {
    assert_eq!(
        parse("\\<kbd>x\\</kbd>"),
        paragraph(vec![text("<kbd>x</kbd>")])
    );
}

#[test]
fn an_empty_kbd_pair_produces_no_piece() {
    assert_eq!(parse("<kbd></kbd>"), empty_document());
}

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

#[test]
fn a_file_image_becomes_a_piece() {
    assert_eq!(
        parse("![a](file://icons/phosphoricons/arrow-up.svg)"),
        paragraph(vec![image(
            "icons/phosphoricons/arrow-up.svg",
            InlineImageFormat::Svg,
            16.
        )])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=24)"),
        paragraph(vec![image(INTRINSIC_ASSET, InlineImageFormat::Image, 24.)])
    );
}

#[test]
fn an_image_without_a_size_takes_the_asset_size() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp)"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            Size::new(40., 30.)
        )])
    );
}

#[test]
fn a_type_overrides_the_extension() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?type=svg)"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Svg,
            Size::new(INTRINSIC_WIDTH as f32, INTRINSIC_HEIGHT as f32)
        )])
    );
    assert_eq!(
        parse("![a](file://icons/phosphoricons/arrow-up.svg?type=\"img\")"),
        paragraph(vec![image(
            "icons/phosphoricons/arrow-up.svg",
            InlineImageFormat::Image,
            16.
        )])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?type=\"img\"&size=24)"),
        paragraph(vec![image(INTRINSIC_ASSET, InlineImageFormat::Image, 24.)])
    );
}

#[test]
fn a_size_may_carry_one_number_or_two() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=24)"),
        paragraph(vec![image(INTRINSIC_ASSET, InlineImageFormat::Image, 24.)])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=24,48)"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            Size::new(24., 48.)
        )])
    );
}

#[test]
fn a_path_with_at_signs_or_non_ascii_survives_the_markdown_parser() {
    assert_eq!(
        parse("![a](file://brands/Solana/colored@72px.png)"),
        paragraph(vec![image(
            "brands/Solana/colored@72px.png",
            InlineImageFormat::Image,
            16.
        )])
    );
    assert_eq!(
        parse("![a](file://中文/图.png)"),
        paragraph(vec![image("中文/图.png", InlineImageFormat::Image, 16.)])
    );
}

#[test]
fn a_quoted_size_survives_the_markdown_parser() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=\"24,48\")"),
        paragraph(vec![sized_image(
            INTRINSIC_ASSET,
            InlineImageFormat::Image,
            Size::new(24., 48.)
        )])
    );
}

#[test]
fn a_very_long_asset_path_reaches_the_ir_verbatim() {
    let path = format!("{}/icon.svg", "deep-segment/".repeat(200));

    assert_eq!(
        parse(&format!("![a](file://{path})")),
        paragraph(vec![image(&path, InlineImageFormat::Svg, 16.)])
    );
    assert_eq!(
        parse(&format!("![a](file://{path}?size=24)")),
        paragraph(vec![image(&path, InlineImageFormat::Svg, 24.)])
    );
}

#[test]
fn an_image_inside_a_link_carries_the_link() {
    assert_eq!(
        parse("[![a](file://test/intrinsic.bmp?size=24)](https://example.com)"),
        one_paragraph(
            ParagraphAlign::Left,
            vec![MarkdownPiece::InlineImage {
                path: SmolStr::new_static(INTRINSIC_ASSET),
                format: InlineImageFormat::Image,
                size: Size::new(24., 24.),
                link: Some(LinkId(0)),
                kbd: None,
            }],
            &["https://example.com"],
        )
    );
}

#[test]
fn an_unusable_size_falls_back_to_the_asset_size() {
    for value in ["abc", "-3", "1e3", "20px", "1.2.3", "20,"] {
        let source = format!("![a](file://test/intrinsic.bmp?size={value})");

        assert_eq!(
            parse(&source),
            paragraph(vec![sized_image(
                INTRINSIC_ASSET,
                InlineImageFormat::Image,
                Size::new(INTRINSIC_WIDTH as f32, INTRINSIC_HEIGHT as f32)
            )]),
            "{source} should fall back to the asset size"
        );
    }
}

#[test]
fn an_explicit_size_is_clamped_to_the_supported_range() {
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=0)"),
        paragraph(vec![image(INTRINSIC_ASSET, InlineImageFormat::Image, 1.)])
    );
    assert_eq!(
        parse("![a](file://test/intrinsic.bmp?size=9999)"),
        paragraph(vec![image(INTRINSIC_ASSET, InlineImageFormat::Image, 256.)])
    );
}

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

#[test]
fn plain_text_skips_an_image_instead_of_reading_its_alt_text() {
    let document = parse("a![Markets](file://icons/phosphoricons/graph.svg)b");

    assert_eq!(document.plain_text().as_str(), "ab");
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
        parse("a\\\n\\\nb").plain_text().as_str(),
        "a\n\nb",
        "the blank line inside the block reads as a blank line"
    );
}

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

#[test]
fn only_the_blank_lines_of_the_source_count() {
    assert_eq!(
        parse("a\n\n- item\n\nb"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            list_block(false, 1, vec![item(vec![left_block(vec![text("item")])])]),
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

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
                    16.
                ),
                text(" "),
                kbd_text("K", 0),
            ],
            &["u"],
        )
    );
}

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

#[test]
fn an_unpaired_open_tag_does_not_swallow_the_pair_after_it() {
    assert_eq!(
        parse("<p align=\"center\">a\n\n<p align=\"right\">b</p>"),
        one_paragraph(ParagraphAlign::Right, vec![text("b")], &[]),
        "the pair that is closed should survive the one that is not"
    );
}

#[test]
fn a_supported_open_tag_nested_in_one_block_is_content() {
    assert_eq!(
        parse("<p align=\"center\">a\n<p align=\"right\">b</p>"),
        centered(vec![text("a")])
    );
}

#[test]
fn block_constructs_inside_an_align_tag_pair_are_drawn() {
    assert_eq!(
        parse("<p align=\"center\">```\ncode\n```</p>"),
        document_of(vec![code_block(None, "code")])
    );
    assert_eq!(
        parse("<p align=\"center\">1. item</p>"),
        document_of(vec![list_block(
            true,
            1,
            vec![item(vec![left_block(vec![text("item")])])]
        )])
    );
    assert_eq!(
        parse("<p align=\"center\"><div>x</div></p>"),
        empty_document(),
        "an HTML block inside the pair is still dropped: tag pairs do not nest"
    );
}

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

#[test]
fn a_comment_beside_an_align_tag_pair_does_not_hide_it() {
    assert_eq!(
        parse("<div>\n<!-- note -->\n<p align=\"center\">x</p>\n</div>"),
        centered(vec![text("x")])
    );
}

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

#[test]
fn a_close_tag_inside_a_comment_does_not_end_the_pair() {
    assert_eq!(
        parse("<p align=\"center\">a<!-- </p> -->b</p>"),
        centered(vec![text("ab")])
    );
}

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

#[test]
fn an_unterminated_comment_at_the_end_of_a_multibyte_source_does_not_panic() {
    assert_eq!(
        parse("<p align=\"center\">x</p>\n<!-- 中"),
        centered(vec![text("x")])
    );
    assert_eq!(parse("<div>\n<!-- 中"), empty_document());
}

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

#[test]
fn a_tag_name_that_only_starts_with_p_is_not_the_align_tag() {
    assert_eq!(
        parse("<div>\n<palign=\"center\">x</p>\n</div>"),
        empty_document()
    );
}

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

#[test]
fn an_align_tag_pair_wrapped_in_other_html_still_counts() {
    assert_eq!(
        parse("<div>\n<p align=\"center\">x</p>\n</div>"),
        centered(vec![text("x")])
    );
}

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

#[test]
fn an_inline_align_tag_changes_nothing() {
    assert_eq!(
        parse("text <p align=\"center\">x</p>"),
        paragraph(vec![text("text x")])
    );
}

#[test]
fn formerly_dropped_block_nodes_are_now_drawn() {
    assert_eq!(
        parse("- item"),
        document_of(vec![list_block(
            false,
            1,
            vec![item(vec![left_block(vec![text("item")])])]
        )])
    );
    assert_eq!(
        parse("> quote"),
        document_of(vec![quote_block(vec![left_block(vec![text("quote")])])])
    );
    assert_eq!(
        parse("```\ncode\n```"),
        document_of(vec![code_block(None, "code")])
    );
    assert_eq!(
        parse("<div>x</div>"),
        empty_document(),
        "an HTML block still leaves nothing behind"
    );
}

#[test]
fn an_empty_source_has_no_blocks_and_no_links() {
    assert_eq!(parse(""), empty_document());
}

#[test]
fn the_same_source_parses_to_the_same_document() {
    let source = "# Title\n\n**bold** and [a link](https://e.com) and ![an image](file://test/intrinsic.bmp?size=24)\n\n- item";

    assert_eq!(parse(source), parse(source));
}

#[test]
fn push_text_merges_one_style_and_lands_it_before_a_style_change() {
    let plain = parse::PieceStyle {
        strong: false,
        emphasis: false,
        strike: false,
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
            strike: false,
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
            text: SmolStr::new_static("x"),
            strong: false,
            emphasis: true,
            strike: false,
            code: false,
            link: Some(LinkId(1)),
            kbd: Some(KbdId(2)),
        }]
    );
}

#[test]
fn a_kbd_change_lands_the_buffer_before_the_new_run() {
    let plain = parse::PieceStyle {
        strong: false,
        emphasis: false,
        strike: false,
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
        image("a/b.png", InlineImageFormat::Image, 24.).with_link(LinkId(2)),
        MarkdownPiece::InlineImage {
            path: SmolStr::new_static("a/b.png"),
            format: InlineImageFormat::Image,
            size: Size::new(24., 24.),
            link: Some(LinkId(2)),
            kbd: None,
        }
    );
    assert_eq!(
        kbd_text("x", 3).with_link(LinkId(2)),
        MarkdownPiece::Text {
            text: SmolStr::new_static("x"),
            strong: false,
            emphasis: false,
            strike: false,
            code: false,
            link: Some(LinkId(2)),
            kbd: Some(KbdId(3)),
        }
    );
    assert_eq!(
        MarkdownPiece::LineBreak.with_link(LinkId(2)),
        MarkdownPiece::LineBreak
    );
}

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
        let drawn = document.plain_text();
        let drawn = drawn.as_str();

        for token in tokens {
            assert!(
                drawn.matches(&token).count() <= 1,
                "{token} was drawn more than once for {source:?} -> {drawn:?}"
            );
        }
    }
}

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
        let plain = plain.as_str();
        for token in tokens {
            assert_eq!(
                plain.matches(token.as_str()).count(),
                1,
                "{token} should be read exactly once for {source:?} -> {plain:?}"
            );
        }
    }
}

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
        let plain = plain.as_str();
        for token in tokens {
            assert_eq!(
                plain.matches(&token).count(),
                1,
                "{token} should be drawn exactly once for {source:?} -> {plain:?}"
            );
        }
    }
}

#[test]
fn a_fenced_code_block_takes_the_first_word_of_its_info_as_the_language() {
    assert_eq!(
        parse("```rust\nlet x = 1;\n```"),
        document_of(vec![code_block(Some("rust"), "let x = 1;")])
    );
    assert_eq!(
        parse("```rust linenos\nfn main() {}\n```"),
        document_of(vec![code_block(Some("rust"), "fn main() {}")])
    );
    assert_eq!(
        parse("```\nplain\n```"),
        document_of(vec![code_block(None, "plain")])
    );
}

#[test]
fn a_fenced_code_block_strips_exactly_one_trailing_newline() {
    assert_eq!(
        parse("```\na\nb\n```"),
        document_of(vec![code_block(None, "a\nb")])
    );
    assert_eq!(
        parse("```\na\n\n```"),
        document_of(vec![code_block(None, "a\n")]),
        "the blank line inside the block is kept; only one trailing newline is stripped"
    );
}

#[test]
fn an_indented_code_block_is_a_code_block() {
    assert_eq!(
        parse("    let x = 1;\n    let y = 2;"),
        document_of(vec![code_block(None, "let x = 1;\nlet y = 2;")])
    );
}

#[test]
fn a_block_quote_holds_the_blocks_inside_it() {
    assert_eq!(
        parse("> quoted"),
        document_of(vec![quote_block(vec![left_block(vec![text("quoted")])])])
    );
    assert_eq!(
        parse("> a\n>\n> b"),
        document_of(vec![quote_block(vec![
            left_block(vec![text("a")]),
            left_block(vec![text("b")]),
        ])]),
        "the quote's own blocks are its blocks; a quote does not synthesize blank lines"
    );
    assert_eq!(
        parse("> # Heading"),
        document_of(vec![quote_block(vec![heading_block(
            HeadingLevel::H1,
            vec![text("Heading")]
        )])])
    );
}

#[test]
fn a_block_quote_may_contain_other_blocks() {
    assert_eq!(
        parse("> > nested"),
        document_of(vec![quote_block(vec![quote_block(vec![left_block(
            vec![text("nested")]
        )])])])
    );
    assert_eq!(
        parse("> - a\n> - b"),
        document_of(vec![quote_block(vec![list_block(
            false,
            1,
            vec![
                item(vec![left_block(vec![text("a")])]),
                item(vec![left_block(vec![text("b")])]),
            ]
        )])])
    );
}

#[test]
fn a_bullet_list_has_items_and_starts_at_one() {
    assert_eq!(
        parse("- a\n- b"),
        document_of(vec![list_block(
            false,
            1,
            vec![
                item(vec![left_block(vec![text("a")])]),
                item(vec![left_block(vec![text("b")])]),
            ]
        )])
    );
}

#[test]
fn an_ordered_list_keeps_its_start() {
    assert_eq!(
        parse("3. a\n4. b"),
        document_of(vec![list_block(
            true,
            3,
            vec![
                item(vec![left_block(vec![text("a")])]),
                item(vec![left_block(vec![text("b")])]),
            ]
        )])
    );
    assert_eq!(
        parse("1. a"),
        document_of(vec![list_block(
            true,
            1,
            vec![item(vec![left_block(vec![text("a")])])]
        )])
    );
}

#[test]
fn a_list_item_may_contain_a_nested_list() {
    assert_eq!(
        parse("- a\n  - b"),
        document_of(vec![list_block(
            false,
            1,
            vec![item(vec![
                left_block(vec![text("a")]),
                list_block(false, 1, vec![item(vec![left_block(vec![text("b")])])]),
            ])]
        )])
    );
}

#[test]
fn a_list_item_may_contain_a_code_block() {
    assert_eq!(
        parse("- item\n  ```\n  code\n  ```"),
        document_of(vec![list_block(
            false,
            1,
            vec![item(vec![
                left_block(vec![text("item")]),
                code_block(None, "code"),
            ])]
        )])
    );
}

#[test]
fn a_thematic_break_is_a_block() {
    assert_eq!(
        parse("a\n\n---\n\nb"),
        document_of(vec![
            left_block(vec![text("a")]),
            MarkdownBlock::BlankLine,
            MarkdownBlock::ThematicBreak,
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
}

#[test]
fn a_table_has_a_header_row_and_aligned_columns() {
    assert_eq!(
        parse("| a | b | c |\n|:--|:-:|--:|\n| d | e | f |"),
        document_of(vec![table_block(vec![
            table_row(
                true,
                vec![
                    table_cell(ParagraphAlign::Left, vec![text("a")]),
                    table_cell(ParagraphAlign::Center, vec![text("b")]),
                    table_cell(ParagraphAlign::Right, vec![text("c")]),
                ]
            ),
            table_row(
                false,
                vec![
                    table_cell(ParagraphAlign::Left, vec![text("d")]),
                    table_cell(ParagraphAlign::Center, vec![text("e")]),
                    table_cell(ParagraphAlign::Right, vec![text("f")]),
                ]
            ),
        ])])
    );
}

#[test]
fn table_cells_keep_their_inline_marks() {
    assert_eq!(
        parse("| **b** | `c` |\n|---|---|\n| [x](u) | ~~s~~ |"),
        MarkdownDocument {
            blocks: vec![table_block(vec![
                table_row(
                    true,
                    vec![
                        table_cell(ParagraphAlign::Left, vec![styled("b", true, false)]),
                        table_cell(ParagraphAlign::Left, vec![code_text("c")]),
                    ]
                ),
                table_row(
                    false,
                    vec![
                        table_cell(ParagraphAlign::Left, vec![linked_text("x", 0)]),
                        table_cell(ParagraphAlign::Left, vec![strike_text("s")]),
                    ]
                ),
            ])],
            links: vec![SmolStr::new_static("u")].into(),
        }
    );
}

#[test]
fn inline_code_is_a_text_piece_with_the_code_mark() {
    assert_eq!(parse("`x`"), paragraph(vec![code_text("x")]));
    assert_eq!(
        parse("a`x`b"),
        paragraph(vec![text("a"), code_text("x"), text("b")])
    );
    assert_eq!(
        parse("**`x`**"),
        paragraph(vec![MarkdownPiece::Text {
            text: SmolStr::new_static("x"),
            strong: true,
            emphasis: false,
            strike: false,
            code: true,
            link: None,
            kbd: None,
        }])
    );
    assert_eq!(
        parse("`**x**`"),
        paragraph(vec![code_text("**x**")]),
        "inline code is literal: the stars stay characters"
    );
}

#[test]
fn strikethrough_is_a_piece_mark() {
    assert_eq!(parse("~~x~~"), paragraph(vec![strike_text("x")]));
    assert_eq!(
        parse("~~**x**~~"),
        paragraph(vec![MarkdownPiece::Text {
            text: SmolStr::new_static("x"),
            strong: true,
            emphasis: false,
            strike: true,
            code: false,
            link: None,
            kbd: None,
        }]),
        "strike stacks with the other marks"
    );
}

#[test]
fn a_kbd_pair_is_counted_per_table_cell() {
    assert_eq!(
        parse("| <kbd>a</kbd> | <kbd>b</kbd> |\n|---|---|"),
        document_of(vec![table_block(vec![table_row(
            true,
            vec![
                table_cell(ParagraphAlign::Left, vec![kbd_text("a", 0)]),
                table_cell(ParagraphAlign::Left, vec![kbd_text("b", 0)]),
            ]
        )])])
    );
}

#[test]
fn blank_lines_around_the_new_block_kinds_are_kept() {
    assert_eq!(
        parse("```\na\n```\n\n> q"),
        document_of(vec![
            code_block(None, "a"),
            MarkdownBlock::BlankLine,
            quote_block(vec![left_block(vec![text("q")])]),
        ])
    );
    assert_eq!(
        parse("- a\n\n\nb"),
        document_of(vec![
            list_block(false, 1, vec![item(vec![left_block(vec![text("a")])])]),
            MarkdownBlock::BlankLine,
            MarkdownBlock::BlankLine,
            left_block(vec![text("b")]),
        ])
    );
    assert_eq!(
        parse("> a\n\n> b"),
        document_of(vec![
            quote_block(vec![left_block(vec![text("a")])]),
            MarkdownBlock::BlankLine,
            quote_block(vec![left_block(vec![text("b")])]),
        ])
    );
}

#[test]
fn container_blocks_do_not_synthesize_blank_lines() {
    assert_eq!(
        parse("- a\n\n- b"),
        document_of(vec![list_block(
            false,
            1,
            vec![
                item(vec![left_block(vec![text("a")])]),
                item(vec![left_block(vec![text("b")])]),
            ]
        )])
    );
    assert_eq!(
        parse("> a\n>\n> b"),
        document_of(vec![quote_block(vec![
            left_block(vec![text("a")]),
            left_block(vec![text("b")]),
        ])])
    );
}

#[test]
fn the_deep_nesting_guard_applies_inside_containers() {
    let deep = format!(
        "{}a{}",
        "*".repeat(OVER_LIMIT_STARS),
        "*".repeat(OVER_LIMIT_STARS)
    );

    assert_eq!(
        parse(&format!("> {deep}")),
        document_of(vec![quote_block(Vec::new())]),
        "the over-deep inline content is dropped; the quote itself is still a block"
    );
    assert_eq!(
        parse(&format!("- {deep}")),
        document_of(vec![list_block(false, 1, vec![item(Vec::new())])]),
        "the over-deep inline content is dropped; the list item is still there"
    );
    assert_eq!(
        parse("> **a**"),
        document_of(vec![quote_block(vec![left_block(vec![styled(
            "a", true, false
        )])])]),
        "nesting within the limit inside a quote is still parsed"
    );
}

fn max_container_depth(block: &MarkdownBlock) -> usize {
    let children: Vec<&MarkdownBlock> = match block {
        MarkdownBlock::Quote { blocks } => blocks.iter().collect(),
        MarkdownBlock::List { items, .. } => {
            items.iter().flat_map(|item| item.blocks.iter()).collect()
        }
        _ => return 0,
    };
    let own = usize::from(matches!(
        block,
        MarkdownBlock::Quote { .. } | MarkdownBlock::List { .. }
    ));
    own + children
        .into_iter()
        .map(max_container_depth)
        .max()
        .unwrap_or(0)
}

fn quote_depth(block: &MarkdownBlock) -> usize {
    match block {
        MarkdownBlock::Quote { blocks } => 1 + blocks.iter().map(quote_depth).max().unwrap_or(0),
        MarkdownBlock::List { items, .. } => items
            .iter()
            .flat_map(|item| item.blocks.iter())
            .map(quote_depth)
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

#[test]
fn deeply_nested_quotes_are_capped_instead_of_overflowing_the_stack() {
    let source = format!("{}deep", "> ".repeat(1_000));
    let document = parse(&source);

    assert_eq!(document.blocks.len(), 1, "顶层只有一个引用块");
    assert!(
        document.plain_text().contains("deep"),
        "超限后文字仍然可见（降级为普通段落），而不是整段丢失"
    );
    assert!(
        max_container_depth(&document.blocks[0]) <= 64,
        "IR 的容器深度必须有界：{}",
        max_container_depth(&document.blocks[0])
    );
}

#[test]
fn quotes_exactly_at_the_block_nesting_limit_stay_structured() {
    let source = format!("{}deep", "> ".repeat(64));
    let document = parse(&source);

    assert_eq!(
        quote_depth(&document.blocks[0]),
        64,
        "64 层引用全部保留为 Quote 结构"
    );
    assert!(document.plain_text().contains("deep"));
}

#[test]
fn quotes_past_the_block_nesting_limit_degrade_to_flat_text() {
    let source = format!("{}deep", "> ".repeat(65));
    let document = parse(&source);

    assert_eq!(
        quote_depth(&document.blocks[0]),
        64,
        "容器深度停在 MAX_BLOCK_NESTING = 64 层"
    );
    let mut block = &document.blocks[0];
    while let MarkdownBlock::Quote { blocks } = block {
        match blocks.as_ref() {
            [MarkdownBlock::Quote { .. }] => block = &blocks[0],
            [MarkdownBlock::Paragraph { pieces, .. }] => {
                assert_eq!(
                    pieces
                        .iter()
                        .filter_map(|piece| match piece {
                            MarkdownPiece::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "deep"
                );
                return;
            }
            other => panic!("unexpected deepest quote content: {other:?}"),
        }
    }
    panic!("expected a nested quote chain");
}

#[test]
fn deeply_nested_lists_are_capped_instead_of_overflowing_the_stack() {
    let source = format!("{}deep", "- ".repeat(300));
    let document = parse(&source);

    assert!(
        document.plain_text().contains("deep"),
        "超限后列表文字仍然可见"
    );
    assert!(
        max_container_depth(&document.blocks[0]) <= 64,
        "IR 的列表深度必须有界：{}",
        max_container_depth(&document.blocks[0])
    );
}

#[test]
fn degrading_an_over_deep_container_keeps_its_siblings() {
    let deep = "> ".repeat(100);
    let source = format!("{deep}deep\n\nsibling");
    let document = parse(&source);

    assert_eq!(
        document.blocks.len(),
        3,
        "深引用 + 空行 + 兄弟段落（顶层空行照旧成为一个 BlankLine 块）"
    );
    assert!(document.plain_text().contains("deep"));
    assert!(document.plain_text().contains("sibling"));
}
