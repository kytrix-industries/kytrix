use std::sync::Arc;

use gpui_kit::{Pixels, SharedString, Size};

use super::source::InlineImageFormat;

/// 文档里一条可打开的链接在 `MarkdownDocument::links` 中的下标。
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

/// 段落的水平对齐：只由 `<p align="…">…</p>` 标签对给出，markdown 自己的段落是左对齐。
/// 一行怎么在这个宽度里偏移由布局层决定（见 `inline_flow::align_offset`）。
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
    ///
    /// 这里用 `SharedString` 而不是 `SmolStr`：它会被直接交给
    /// `TextSystem::shape_line`，用 `SmolStr` 会在每帧多一次转换。这是对根
    /// `AGENTS.md` §2.2 的有意例外。
    Text {
        text: SharedString,
        strong: bool,
        emphasis: bool,
        link: Option<LinkId>,
        /// 这段文字落在哪一段 kbd 内容里；不在 `<kbd>` 里时为 `None`。
        kbd: Option<KbdId>,
    },
    /// 一张行内图片，占据逻辑索引空间里的 1 个字节。
    ///
    /// `size` 是解析时就已经定下来的绘制尺寸（原文写了 `size` 就是它，否则是资源自身的尺寸），
    /// 所以折行与定位都不需要在绘制阶段再问一次资源有多大。
    InlineImage {
        /// 资源路径，`AssetSource::load` 收到的那一串（见 [`InlineImageSource`]）。
        ///
        /// [`InlineImageSource`]: super::source::InlineImageSource
        path: SharedString,
        format: InlineImageFormat,
        size: Size<Pixels>,
        link: Option<LinkId>,
        /// 这张图片落在哪一段 kbd 内容里；不在 `<kbd>` 里时为 `None`。
        kbd: Option<KbdId>,
    },
    /// SoftBreak / LineBreak：块内换行。
    ///
    /// 空行本身会结束段落，所以**块与块之间、以及标签对之间的空行**都由
    /// [`MarkdownBlock::BlankLine`] 表达。连着两个这一片段只有一种来源：一个段落里连写两个
    /// 硬换行（`a\\\n\\\nb`，中间那一行是空的）——它作为**块内的一行空行**留在同一个块里，
    /// 由排版层的空硬换行画出来。其余情况它只表示"这一行到头了"。
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
                kbd,
                ..
            } => MarkdownPiece::Text {
                text,
                strong,
                emphasis,
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
    /// 空行不是 markdown 的块，但本组件不折叠源码里的空白：两个块之间的每一个空行都产出
    /// 一个这样的块（几行空行就是几个），排版层把它画成一行正文行高。空行只画在**两个真的
    /// 产出块的块级结构之间**——文档首尾、以及第一个产出块之前的空行都不进 `blocks`，所以
    /// `"\n\na"` 与 `"a\n\n"` 都与 `"a"` 一样。
    BlankLine,
}

#[derive(Debug, PartialEq)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
    /// 文档里所有**确实被渲染出来**的链接 URL，下标即 `LinkId`。
    pub links: Arc<[SharedString]>,
}

impl MarkdownDocument {
    /// 这篇文档被"读"而不是被"看"时说的话。
    ///
    /// 按文档顺序把每一个块读成它那一行（空行读成空行），块与块之间读作一个换行；段内的换行
    /// （[`MarkdownPiece::LineBreak`]）同样读作换行，图片读作空——图片就站在它所属的那个词旁边，
    /// 两者不该被读两遍。空白不折叠，理由与组件绘制时不折叠它相同：这里交出的是文档本身持有的
    /// 东西，不是一份整理过的版本，所以源码里的 `a\n\nb` 读出来也是 `a\n\nb`。
    pub fn plain_text(&self) -> SharedString {
        let mut plain = String::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                plain.push('\n');
            }
            let pieces = match block {
                MarkdownBlock::Heading { pieces, .. } => pieces,
                MarkdownBlock::Paragraph { pieces, .. } => pieces,
                MarkdownBlock::BlankLine => continue,
            };
            for piece in pieces.iter() {
                match piece {
                    MarkdownPiece::Text { text, .. } => plain.push_str(text),
                    MarkdownPiece::InlineImage { .. } => {}
                    MarkdownPiece::LineBreak => plain.push('\n'),
                }
            }
        }
        SharedString::from(plain)
    }
}
