//! 解析后的 markdown 文档 IR（只读模型，无 UI 框架类型 / 无引擎类型）。
//!
//! IR 分三层：块（[`MarkdownBlock`]）→ 行内片段（[`MarkdownPiece`]）→ 链接表。块之间保留
//! 源码里的空行（[`MarkdownBlock::BlankLine`]），行内片段保留 strong / emphasis / strike /
//! code / link / kbd 六种标记与行内图片。
//!
//! 与旧实现（`markdown/document.rs`）的差别（P2-3 交付的**扩展**，见
//! `.docs/gui-migration/spec-deltas/kit-text.md`）：
//!
//! - 字符串从 旧 UI 层的 `SharedString` 换成 `SmolStr`、尺寸从 `Size<Pixels>` 换成引擎的
//!   [`ui_core::math::Size`]（逻辑像素 `f32`）；
//! - 新增 `CodeBlock` / `Quote` / `List` / `Table` / `ThematicBreak` 五种块与行内 `code` /
//!   `strike` 两种标记：旧实现把这些节点整棵丢弃，本批按引擎节点树把它们表达出来。

use std::sync::Arc;

use smol_str::SmolStr;
use ui_core::math::Size;

use super::source::InlineImageFormat;

/// 文档里一条可打开的链接在 [`MarkdownDocument::links`] 中的下标。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LinkId(pub usize);

/// 一段 kbd 内容（一对 `<kbd>` / `</kbd>` 之间的内容）在本篇文档里的编号。
///
/// comrak 把这对标签解析成两个互不相干的 `HtmlInline` 节点，编号在这对标签的内容里共用。
/// 相邻的两对标签编号不同，所以它们各自画一个框：编号是渲染层判断"哪几个片段属于同一段内容"
/// 的唯一依据。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KbdId(pub usize);

/// 标题级别。CommonMark 只产生 1..=6。
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
    /// 越界一律按 H6 处理：CommonMark 只产生 1..=6，这里的兜底只为不 panic。
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

    /// 相对正文继承字号的倍率。
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

/// 段落的水平对齐：markdown 自己的段落是左对齐，`<p align="…">` 标签对给出另外两种。
/// 表格单元格的对齐来自表格分隔行（`:---:`），同样用这个类型表达。
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ParagraphAlign {
    #[default]
    Left,
    Right,
    Center,
}

/// 块内的一段行内内容。
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownPiece {
    /// 一段连续、样式相同的文字。解析层负责把相邻同样式的文字合并成一段。
    Text {
        text: SmolStr,
        strong: bool,
        emphasis: bool,
        /// 删除线（`~~x~~`）。旧实现整棵丢弃，本批作为行内标记保留。
        strike: bool,
        /// 行内代码（`` `x` ``）。旧实现整棵丢弃，本批作为行内标记保留。
        code: bool,
        link: Option<LinkId>,
        /// 这段文字落在哪一段 kbd 内容里；不在 `<kbd>` 里时为 `None`。
        kbd: Option<KbdId>,
    },
    /// 一张行内图片。
    ///
    /// `size` 是解析时就已经定下来的绘制尺寸（原文写了 `size` 就是它，否则是资源自身的尺寸），
    /// 视图层因此不需要在挂载阶段再问一次资源有多大。
    InlineImage {
        /// 资源路径，[`MarkdownAssets::load`](super::source::MarkdownAssets::load) 收到的那一串。
        path: SmolStr,
        format: InlineImageFormat,
        size: Size,
        link: Option<LinkId>,
        /// 这张图片落在哪一段 kbd 内容里；不在 `<kbd>` 里时为 `None`。
        kbd: Option<KbdId>,
    },
    /// SoftBreak / LineBreak：块内换行。
    ///
    /// 空行本身会结束段落，所以**块与块之间、以及标签对之间的空行**都由
    /// [`MarkdownBlock::BlankLine`] 表达。连着两个这一片段只有一种来源：一个段落里连写两个
    /// 硬换行（`a\\\n\\\nb`，中间那一行是空的）——它作为**块内的一行空行**留在同一个块里。
    LineBreak,
}

impl MarkdownPiece {
    /// 把这段行内内容归属到链接表的第 `id` 条链接。
    ///
    /// 直接覆盖已有的 `link`：CommonMark 不允许嵌套链接，所以这里不可能遇到
    /// `link: Some(_)`，不需要保留外层链接。
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

/// 列表项：一列块（项里可以带段落、代码块、嵌套列表…）。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownListItem {
    pub blocks: Arc<[MarkdownBlock]>,
}

/// 表格的一行。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownTableRow {
    /// 表头行（`|---|` 分隔行之前的那一行）。
    pub header: bool,
    pub cells: Arc<[MarkdownTableCell]>,
}

/// 表格的一个单元格：一段行内内容 + 该列的对齐方式。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownTableCell {
    pub align: ParagraphAlign,
    pub pieces: Arc<[MarkdownPiece]>,
}

/// 一个块级节点。
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
    /// 源码里的一行空行。
    ///
    /// 空行不是 markdown 的块，但本组件不折叠源码里的空白：两个块之间的每一个空行都产出一个
    /// 这样的块（几行空行就是几个），排版层把它画成一行正文行高。空行只画在**两个真的产出块的
    /// 块级结构之间**——文档首尾、以及第一个产出块之前的空行都不进 `blocks`。
    BlankLine,
    /// 围栏 / 缩进代码块。
    CodeBlock {
        /// 围栏后的语言标记（```rust 的 `rust`）；没有时为 `None`。
        language: Option<SmolStr>,
        /// 代码原文（**不加**行尾换行；行尾的 `\n` 由 view 的排版决定）。
        code: SmolStr,
    },
    /// 引用块：内容是一列块（可嵌套段落、列表、代码块…）。
    Quote { blocks: Arc<[MarkdownBlock]> },
    /// 有序 / 无序列表。`start` 是有序列表的起始编号（无序列表恒 1）。
    List {
        ordered: bool,
        start: u64,
        items: Arc<[MarkdownListItem]>,
    },
    /// 分隔线（`---` / `***`）。
    ThematicBreak,
    /// 表格（GFM 表格扩展；comrak 默认关闭扩展，本批显式打开 `table`）。
    Table { rows: Arc<[MarkdownTableRow]> },
}

impl MarkdownBlock {
    /// 块里的行内片段（非内容型块给 `None`）。
    pub fn pieces(&self) -> Option<&[MarkdownPiece]> {
        match self {
            MarkdownBlock::Heading { pieces, .. } | MarkdownBlock::Paragraph { pieces, .. } => {
                Some(pieces)
            }
            _ => None,
        }
    }
}

/// 一份解析好的 markdown 文档（只读模型：解析一次，视图/测试共用）。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
    /// 文档里所有**确实被渲染出来**的链接 URL，下标即 `LinkId`。
    pub links: Arc<[SmolStr]>,
}

impl MarkdownDocument {
    /// 这篇文档被"读"而不是被"看"时说的话（可访问名称 / i18n 兜底用）。
    ///
    /// 按文档顺序把每一个块读成它那一行（空行读成空行），块与块之间读作一个换行；段内的换行
    /// （[`MarkdownPiece::LineBreak`]）同样读作换行，图片读作空——图片就站在它所属的那个词旁边，
    /// 两者不该被读两遍。代码块读代码原文；列表项之间读作换行；表格一行读作一行、单元格之间
    /// 读作一个空格。
    pub fn plain_text(&self) -> SmolStr {
        let mut plain = String::new();
        plain_blocks(&self.blocks, &mut plain);
        SmolStr::from(plain)
    }
}

/// 递归把块读成纯文本；`plain` 非空时先补一个换行（块与块之间读作一个换行）。
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

/// 把一段行内片段读成纯文本。
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
    use ui_core::math::Size;

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
                        size: Size::new(8.0, 8.0),
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
