use std::sync::Arc;

use smol_str::SmolStr;
use ui_core::math::LogicalSize;

use super::source::InlineImageFormat;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LinkId(pub usize);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KbdId(pub usize);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HeadingLevel {
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
}

impl HeadingLevel {
    pub fn from_u8(level: u8) -> Self {
        match level {
            1 => HeadingLevel::H1,
            2 => HeadingLevel::H2,
            3 => HeadingLevel::H3,
            4 => HeadingLevel::H4,
            5 => HeadingLevel::H5,
            _ => HeadingLevel::H6,
        }
    }

    pub fn scale(self) -> f32 {
        match self {
            HeadingLevel::H1 => 1.75,
            HeadingLevel::H2 => 1.5,
            HeadingLevel::H3 => 1.3,
            HeadingLevel::H4 => 1.15,
            HeadingLevel::H5 => 1.05,
            HeadingLevel::H6 => 1.0,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ParagraphAlign {
    #[default]
    Left,
    Right,
    Center,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownPiece {
    Text {
        text: SmolStr,
        strong: bool,
        emphasis: bool,
        strike: bool,
        code: bool,
        link: Option<LinkId>,
        kbd: Option<KbdId>,
    },
    InlineImage {
        path: SmolStr,
        format: InlineImageFormat,
        size: LogicalSize,
        link: Option<LinkId>,
        kbd: Option<KbdId>,
    },
    LineBreak,
}

impl MarkdownPiece {
    pub(super) fn with_link(self, id: LinkId) -> Self {
        match self {
            MarkdownPiece::Text {
                text,
                strong,
                emphasis,
                strike,
                code,
                kbd,
                ..
            } => MarkdownPiece::Text {
                text,
                strong,
                emphasis,
                strike,
                code,
                link: Some(id),
                kbd,
            },
            MarkdownPiece::InlineImage {
                path,
                format,
                size,
                kbd,
                ..
            } => MarkdownPiece::InlineImage {
                path,
                format,
                size,
                link: Some(id),
                kbd,
            },
            MarkdownPiece::LineBreak => MarkdownPiece::LineBreak,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownListItem {
    pub blocks: Arc<[MarkdownBlock]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownTableRow {
    pub header: bool,
    pub cells: Arc<[MarkdownTableCell]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownTableCell {
    pub align: ParagraphAlign,
    pub pieces: Arc<[MarkdownPiece]>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownBlock {
    Heading {
        level: HeadingLevel,
        pieces: Arc<[MarkdownPiece]>,
    },
    Paragraph {
        align: ParagraphAlign,
        pieces: Arc<[MarkdownPiece]>,
    },
    BlankLine,
    CodeBlock {
        language: Option<SmolStr>,
        code: SmolStr,
    },
    Quote {
        blocks: Arc<[MarkdownBlock]>,
    },
    List {
        ordered: bool,
        start: u64,
        items: Arc<[MarkdownListItem]>,
    },
    ThematicBreak,
    Table {
        rows: Arc<[MarkdownTableRow]>,
    },
}

impl MarkdownBlock {
    pub fn pieces(&self) -> Option<&[MarkdownPiece]> {
        match self {
            MarkdownBlock::Heading { pieces, .. } | MarkdownBlock::Paragraph { pieces, .. } => {
                Some(pieces)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
    pub links: Arc<[SmolStr]>,
}

impl MarkdownDocument {
    pub fn plain_text(&self) -> SmolStr {
        let mut plain = String::new();
        plain_blocks(&self.blocks, &mut plain);
        SmolStr::from(plain)
    }
}

fn plain_blocks(blocks: &[MarkdownBlock], plain: &mut String) {
    for block in blocks {
        if !plain.is_empty() {
            plain.push('\n');
        }
        match block {
            MarkdownBlock::Heading { pieces, .. } | MarkdownBlock::Paragraph { pieces, .. } => {
                plain_pieces(pieces, plain);
            }
            MarkdownBlock::BlankLine => {}
            MarkdownBlock::CodeBlock { code, .. } => plain.push_str(code),
            MarkdownBlock::Quote { blocks } => plain_blocks(blocks, plain),
            MarkdownBlock::List { items, .. } => {
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        plain.push('\n');
                    }
                    plain_blocks(&item.blocks, plain);
                }
            }
            MarkdownBlock::ThematicBreak => {}
            MarkdownBlock::Table { rows } => {
                for (index, row) in rows.iter().enumerate() {
                    if index > 0 {
                        plain.push('\n');
                    }
                    for (cell_index, cell) in row.cells.iter().enumerate() {
                        if cell_index > 0 {
                            plain.push(' ');
                        }
                        plain_pieces(&cell.pieces, plain);
                    }
                }
            }
        }
    }
}

pub(super) fn plain_pieces(pieces: &[MarkdownPiece], plain: &mut String) {
    for piece in pieces {
        match piece {
            MarkdownPiece::Text { text, .. } => plain.push_str(text),
            MarkdownPiece::InlineImage { .. } => {}
            MarkdownPiece::LineBreak => plain.push('\n'),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smol_str::SmolStr;
    use ui_core::math::{LogicalSize, px};

    use super::{
        HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownListItem,
        MarkdownPiece, MarkdownTableCell, MarkdownTableRow, ParagraphAlign,
    };
    use crate::kit::markdown::source::InlineImageFormat;

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

    fn document(blocks: Vec<MarkdownBlock>) -> MarkdownDocument {
        MarkdownDocument {
            blocks,
            links: Arc::from(Vec::<SmolStr>::new()),
        }
    }

    #[test]
    fn heading_levels_map_to_scales() {
        assert_eq!(HeadingLevel::from_u8(1), HeadingLevel::H1);
        assert_eq!(HeadingLevel::from_u8(6), HeadingLevel::H6);
        assert_eq!(
            HeadingLevel::from_u8(7),
            HeadingLevel::H6,
            "越界按 H6 兜底，不 panic"
        );
        assert_eq!(HeadingLevel::H1.scale(), 1.75);
        assert!(HeadingLevel::H1.scale() > HeadingLevel::H3.scale());
        assert_eq!(HeadingLevel::H6.scale(), 1.0);
    }

    #[test]
    fn with_link_keeps_every_other_field() {
        let linked = text("a").with_link(LinkId(3));
        match linked {
            MarkdownPiece::Text { link, text, .. } => {
                assert_eq!(link, Some(LinkId(3)));
                assert_eq!(text, "a");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_reads_blocks_lines_and_inline_marks() {
        let doc = document(vec![
            MarkdownBlock::Heading {
                level: HeadingLevel::H1,
                pieces: Arc::from(vec![text("title")]),
            },
            MarkdownBlock::BlankLine,
            MarkdownBlock::Paragraph {
                align: ParagraphAlign::Left,
                pieces: Arc::from(vec![
                    text("a"),
                    MarkdownPiece::LineBreak,
                    text("b"),
                    MarkdownPiece::InlineImage {
                        path: SmolStr::new_static("x.png"),
                        format: InlineImageFormat::Image,
                        size: LogicalSize::new(px(8.0), px(8.0)),
                        link: None,
                        kbd: Some(KbdId(0)),
                    },
                    text("c"),
                ]),
            },
        ]);

        assert_eq!(doc.plain_text(), "title\n\na\nbc");
    }

    #[test]
    fn plain_text_reads_code_quote_list_and_table_blocks() {
        let doc = document(vec![
            MarkdownBlock::CodeBlock {
                language: Some(SmolStr::new_static("rust")),
                code: SmolStr::new_static("let x = 1;"),
            },
            MarkdownBlock::Quote {
                blocks: Arc::from(vec![MarkdownBlock::Paragraph {
                    align: ParagraphAlign::Left,
                    pieces: Arc::from(vec![text("quoted")]),
                }]),
            },
            MarkdownBlock::List {
                ordered: true,
                start: 2,
                items: Arc::from(vec![
                    MarkdownListItem {
                        blocks: Arc::from(vec![MarkdownBlock::Paragraph {
                            align: ParagraphAlign::Left,
                            pieces: Arc::from(vec![text("one")]),
                        }]),
                    },
                    MarkdownListItem {
                        blocks: Arc::from(vec![MarkdownBlock::Paragraph {
                            align: ParagraphAlign::Left,
                            pieces: Arc::from(vec![text("two")]),
                        }]),
                    },
                ]),
            },
            MarkdownBlock::ThematicBreak,
            MarkdownBlock::Table {
                rows: Arc::from(vec![
                    MarkdownTableRow {
                        header: true,
                        cells: Arc::from(vec![
                            MarkdownTableCell {
                                align: ParagraphAlign::Left,
                                pieces: Arc::from(vec![text("h1")]),
                            },
                            MarkdownTableCell {
                                align: ParagraphAlign::Right,
                                pieces: Arc::from(vec![text("h2")]),
                            },
                        ]),
                    },
                    MarkdownTableRow {
                        header: false,
                        cells: Arc::from(vec![
                            MarkdownTableCell {
                                align: ParagraphAlign::Left,
                                pieces: Arc::from(vec![text("a")]),
                            },
                            MarkdownTableCell {
                                align: ParagraphAlign::Left,
                                pieces: Arc::from(vec![text("b")]),
                            },
                        ]),
                    },
                ]),
            },
        ]);

        assert_eq!(
            doc.plain_text(),
            "let x = 1;\n\nquoted\n\none\n\ntwo\n\nh1 h2\na b"
        );
    }

    #[test]
    fn pieces_only_reports_inline_content_blocks() {
        let paragraph = MarkdownBlock::Paragraph {
            align: ParagraphAlign::Center,
            pieces: Arc::from(vec![text("x")]),
        };
        assert_eq!(paragraph.pieces().map(<[MarkdownPiece]>::len), Some(1));
        assert!(MarkdownBlock::ThematicBreak.pieces().is_none());
        assert!(MarkdownBlock::BlankLine.pieces().is_none());
    }
}
