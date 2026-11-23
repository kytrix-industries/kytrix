use std::{ops::Range, sync::Arc};

use comrak::{
    Arena, Options,
    nodes::{ListType, Node, NodeLink, NodeTable, NodeValue, TableAlignment},
    options::Extension,
};
use smol_str::SmolStr;

use super::{
    document::{
        HeadingLevel, KbdId, LinkId, MarkdownBlock, MarkdownDocument, MarkdownListItem,
        MarkdownPiece, MarkdownTableCell, MarkdownTableRow, ParagraphAlign,
    },
    source::{InlineImageSource, MarkdownAssets},
};

const MAX_INLINE_NESTING: usize = 64;

const MAX_BLOCK_NESTING: usize = 64;
const FIRST_TAG_BLOCK_TYPE: u8 = 6;
const ALIGN_TAG_CLOSE: &str = "</p>";
const COMMENT_OPEN: &str = "<!--";
const COMMENT_CLOSE: &str = "-->";

struct SourceLines {
    starts: Vec<usize>,
    blank: Vec<bool>,
}

impl SourceLines {
    fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut starts = vec![0];
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                    index += 2;
                    starts.push(index);
                }
                b'\r' | b'\n' => {
                    index += 1;
                    starts.push(index);
                }
                _ => index += 1,
            }
        }

        let blank = (0..starts.len())
            .map(|line| {
                let start = starts[line];
                let end = starts.get(line + 1).copied().unwrap_or(source.len());
                source[start..end].trim().is_empty()
            })
            .collect();

        Self { starts, blank }
    }

    fn line_offset(&self, line: usize) -> Option<usize> {
        self.starts.get(line.checked_sub(1)?).copied()
    }

    fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|start| *start <= offset)
    }

    fn blank_between(&self, previous_end: usize, next_start: usize) -> usize {
        (previous_end..next_start.saturating_sub(1))
            .filter(|index| self.blank.get(*index).copied().unwrap_or(false))
            .count()
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub(super) struct PieceStyle {
    pub strong: bool,
    pub emphasis: bool,
    pub strike: bool,
    pub link: Option<LinkId>,
    pub kbd: Option<KbdId>,
}

struct KbdState {
    depth: usize,
    current: Option<KbdId>,
    next: usize,
}

impl KbdState {
    fn new() -> Self {
        Self {
            depth: 0,
            current: None,
            next: 0,
        }
    }

    fn open(&mut self) {
        if self.depth == 0 {
            self.current = Some(KbdId(self.next));
            self.next += 1;
        }
        self.depth += 1;
    }

    fn close(&mut self) {
        if self.depth == 0 {
            return;
        }
        self.depth -= 1;
        if self.depth == 0 {
            self.current = None;
        }
    }
}

pub(super) struct TextBuffer {
    pub text: String,
    pub style: PieceStyle,
}

pub fn parse_document(source: &str, assets: &dyn MarkdownAssets) -> MarkdownDocument {
    let lines = SourceLines::new(source);
    let options = Options {
        extension: Extension {
            table: true,
            strikethrough: true,
            ..Extension::default()
        },
        ..Options::default()
    };
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, source, &options);
    let nodes = root.children().collect::<Vec<_>>();
    let open_tags = open_tags(&nodes, source, &lines);
    let mut blocks = Vec::new();
    let mut links = Vec::new();
    let mut previous_end = None;
    let mut consumed_before = 0;
    let mut next_open = 0;

    for &node in &nodes {
        let data = node.data();
        let consumed = lines
            .line_offset(data.sourcepos.start.line)
            .is_some_and(|start| start < consumed_before);
        if consumed && !matches!(data.value, NodeValue::HtmlBlock(_)) {
            continue;
        }

        match &data.value {
            NodeValue::HtmlBlock(html) => {
                if html.block_type < FIRST_TAG_BLOCK_TYPE {
                    continue;
                }
                let block_end = lines
                    .line_offset(data.sourcepos.end.line + 1)
                    .unwrap_or(source.len());
                while let Some(tag) = open_tags.get(next_open)
                    && tag.tag.start < block_end
                {
                    let index = next_open;
                    next_open += 1;
                    if tag.tag.start < consumed_before {
                        continue;
                    }
                    let bound = pair_bound(source, &open_tags, index);
                    let Some(pair) = align_pair(source, &lines, tag, bound) else {
                        continue;
                    };
                    consumed_before = pair.end;
                    let blocks_of_pair = pair_blocks(
                        &source[pair.content.clone()],
                        pair.align,
                        assets,
                        &mut links,
                        &options,
                    );
                    if blocks_of_pair.is_empty() {
                        continue;
                    }
                    push_blank_lines(
                        &lines,
                        previous_end,
                        lines.line_of(tag.tag.start),
                        &mut blocks,
                    );
                    blocks.extend(blocks_of_pair);
                    previous_end = Some(pair.close_line);
                }
            }
            _ => {
                let Some((block, end)) =
                    content_block(node, ParagraphAlign::Left, assets, &mut links, 0)
                else {
                    continue;
                };
                push_blank_lines(&lines, previous_end, data.sourcepos.start.line, &mut blocks);
                blocks.push(block);
                previous_end = Some(end);
            }
        }
    }

    MarkdownDocument {
        blocks,
        links: links.into(),
    }
}

fn content_block(
    node: Node<'_>,
    align: ParagraphAlign,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
    depth: usize,
) -> Option<(MarkdownBlock, usize)> {
    let data = node.data();
    let end = data.sourcepos.end.line;
    let block = match &data.value {
        NodeValue::Heading(heading) => {
            let pieces = block_pieces(node, assets, links);
            if pieces.is_empty() {
                return None;
            }
            MarkdownBlock::Heading {
                level: HeadingLevel::from_u8(heading.level),
                pieces: pieces.into(),
            }
        }
        NodeValue::Paragraph => {
            let pieces = block_pieces(node, assets, links);
            if pieces.is_empty() {
                return None;
            }
            MarkdownBlock::Paragraph {
                align,
                pieces: pieces.into(),
            }
        }
        NodeValue::CodeBlock(code) => MarkdownBlock::CodeBlock {
            language: code.info.split_whitespace().next().map(SmolStr::new),
            code: SmolStr::new(code.literal.strip_suffix('\n').unwrap_or(&code.literal)),
        },
        NodeValue::BlockQuote => {
            let blocks = if depth + 1 >= MAX_BLOCK_NESTING {
                Arc::from(vec![flattened_block(node)])
            } else {
                container_blocks(node, assets, links, depth + 1).into()
            };
            MarkdownBlock::Quote { blocks }
        }
        NodeValue::List(list) => {
            let ordered = list.list_type == ListType::Ordered;
            let start = if ordered { list.start as u64 } else { 1 };
            let items = if depth + 1 >= MAX_BLOCK_NESTING {
                vec![MarkdownListItem {
                    blocks: Arc::from(vec![flattened_block(node)]),
                }]
            } else {
                node.children()
                    .filter_map(|item| match &item.data().value {
                        NodeValue::Item(..) => Some(MarkdownListItem {
                            blocks: container_blocks(item, assets, links, depth + 1).into(),
                        }),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            MarkdownBlock::List {
                ordered,
                start,
                items: items.into(),
            }
        }
        NodeValue::ThematicBreak => MarkdownBlock::ThematicBreak,
        NodeValue::Table(table) => MarkdownBlock::Table {
            rows: table_rows(node, table, assets, links).into(),
        },
        _ => return None,
    };

    Some((block, end))
}

fn flattened_block(node: Node<'_>) -> MarkdownBlock {
    let mut pieces: Vec<MarkdownPiece> = Vec::new();
    let mut text = String::new();
    let push_text = |text: &mut String, pieces: &mut Vec<MarkdownPiece>| {
        if !text.is_empty() {
            pieces.push(MarkdownPiece::Text {
                text: SmolStr::new(std::mem::take(text)),
                strong: false,
                emphasis: false,
                strike: false,
                code: false,
                link: None,
                kbd: None,
            });
        }
    };
    for descendant in node.descendants() {
        match &descendant.data().value {
            NodeValue::Text(literal) => text.push_str(literal),
            NodeValue::Code(code) => text.push_str(&code.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => {
                push_text(&mut text, &mut pieces);
                pieces.push(MarkdownPiece::LineBreak);
            }
            _ => {}
        }
    }
    push_text(&mut text, &mut pieces);
    MarkdownBlock::Paragraph {
        align: ParagraphAlign::Left,
        pieces: pieces.into(),
    }
}

fn container_blocks(
    node: Node<'_>,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
    depth: usize,
) -> Vec<MarkdownBlock> {
    node.children()
        .filter_map(|child| content_block(child, ParagraphAlign::Left, assets, links, depth))
        .map(|(block, _)| block)
        .collect()
}

fn table_rows(
    node: Node<'_>,
    table: &NodeTable,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
) -> Vec<MarkdownTableRow> {
    node.children()
        .filter_map(|row| {
            let NodeValue::TableRow(header) = &row.data().value else {
                return None;
            };
            let cells = row
                .children()
                .enumerate()
                .map(|(column, cell)| MarkdownTableCell {
                    align: match table.alignments.get(column) {
                        Some(TableAlignment::Center) => ParagraphAlign::Center,
                        Some(TableAlignment::Right) => ParagraphAlign::Right,
                        _ => ParagraphAlign::Left,
                    },
                    pieces: block_pieces(cell, assets, links).into(),
                })
                .collect::<Vec<_>>();

            Some(MarkdownTableRow {
                header: *header,
                cells: cells.into(),
            })
        })
        .collect()
}

fn push_blank_lines(
    lines: &SourceLines,
    previous_end: Option<usize>,
    next_start: usize,
    blocks: &mut Vec<MarkdownBlock>,
) {
    let Some(previous_end) = previous_end else {
        return;
    };
    blocks.extend(std::iter::repeat_n(
        MarkdownBlock::BlankLine,
        lines.blank_between(previous_end, next_start),
    ));
}

struct OpenTag {
    tag: Range<usize>,
    align: ParagraphAlign,
    block_end_line: usize,
}

struct AlignPair {
    align: ParagraphAlign,
    content: Range<usize>,
    end: usize,
    close_line: usize,
}

fn open_tags(nodes: &[Node<'_>], source: &str, lines: &SourceLines) -> Vec<OpenTag> {
    let mut open_tags = Vec::new();
    for &node in nodes {
        let data = node.data();
        let NodeValue::HtmlBlock(html) = &data.value else {
            continue;
        };
        if html.block_type < FIRST_TAG_BLOCK_TYPE {
            continue;
        }
        let Some(start) = lines.line_offset(data.sourcepos.start.line) else {
            continue;
        };
        let end = lines
            .line_offset(data.sourcepos.end.line + 1)
            .unwrap_or(source.len());
        let mut rest = &source[start..end];
        let mut base = start;
        while let Some(tag) = next_tag(rest) {
            let inner = &rest[tag.start + 1..tag.end];
            if let Some(align) = open_tag(inner.strip_suffix('>').unwrap_or(inner)) {
                open_tags.push(OpenTag {
                    tag: base + tag.start..base + tag.end,
                    align,
                    block_end_line: data.sourcepos.end.line,
                });
            }
            base += tag.end;
            rest = &rest[tag.end..];
        }
    }

    open_tags
}

fn pair_bound(source: &str, open_tags: &[OpenTag], index: usize) -> usize {
    let tag = &open_tags[index];
    open_tags[index + 1..]
        .iter()
        .find(|next| next.block_end_line > tag.block_end_line)
        .map_or(source.len(), |next| next.tag.start)
}

fn align_pair(source: &str, lines: &SourceLines, tag: &OpenTag, bound: usize) -> Option<AlignPair> {
    let close = tag.tag.end + find_close_tag(&source[tag.tag.end..bound])?;

    Some(AlignPair {
        align: tag.align,
        content: tag.tag.end..close,
        end: close + ALIGN_TAG_CLOSE.len(),
        close_line: lines.line_of(close),
    })
}

fn block_pieces(
    node: Node<'_>,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
) -> Vec<MarkdownPiece> {
    let mut pieces = Vec::new();
    parse_pieces(
        node,
        PieceStyle::default(),
        0,
        &mut KbdState::new(),
        assets,
        links,
        &mut pieces,
    );
    pieces
}

fn find_close_tag(text: &str) -> Option<usize> {
    let mut rest = text;
    let mut base = 0;
    while let Some(offset) = rest.find(ALIGN_TAG_CLOSE) {
        let Some(comment) = rest[..offset].rfind(COMMENT_OPEN) else {
            return Some(base + offset);
        };
        let end = comment + comment_end(&rest[comment..]);
        base += end;
        rest = &rest[end..];
    }
    None
}

fn comment_end(text: &str) -> usize {
    let body = &text[COMMENT_OPEN.len()..];
    match body.as_bytes() {
        [b'>', ..] => COMMENT_OPEN.len() + 1,
        [b'-', b'>', ..] => COMMENT_OPEN.len() + 2,
        _ => body.find(COMMENT_CLOSE).map_or(text.len(), |offset| {
            COMMENT_OPEN.len() + offset + COMMENT_CLOSE.len()
        }),
    }
}

fn next_tag(text: &str) -> Option<Range<usize>> {
    let mut from = 0;
    while let Some(offset) = text[from..].find('<') {
        let start = from + offset;
        let rest = &text[start + 1..];
        if rest.starts_with("!--") {
            return Some(start..start + comment_end(&text[start..]));
        }
        let opens_tag = match rest.chars().next() {
            Some(character) if character.is_ascii_alphabetic() => true,
            Some('!') => true,
            Some('/') => rest[1..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic()),
            _ => false,
        };
        if !opens_tag {
            from = start + 1;
            continue;
        }

        let mut quote = None;
        for (offset, character) in rest.char_indices() {
            match quote {
                Some(open) => {
                    if character == open {
                        quote = None;
                    }
                }
                None => match character {
                    '"' | '\'' => quote = Some(character),
                    '>' => return Some(start..start + offset + 2),
                    _ => {}
                },
            }
        }
        return None;
    }
    None
}

fn open_tag(inner: &str) -> Option<ParagraphAlign> {
    let mut rest = inner.strip_prefix('p')?;
    if !rest.is_empty() && !rest.starts_with(|character: char| character.is_ascii_whitespace()) {
        return None;
    }

    let mut align = None;
    loop {
        rest = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if rest.is_empty() {
            return Some(align.unwrap_or_default());
        }
        if align.is_some() {
            return None;
        }
        let after = rest.strip_prefix("align=")?.strip_prefix('"')?;
        let end = after.find('"')?;
        align = Some(match &after[..end] {
            "left" => ParagraphAlign::Left,
            "right" => ParagraphAlign::Right,
            "center" => ParagraphAlign::Center,
            _ => return None,
        });
        rest = &after[end + 1..];
    }
}

fn pair_blocks(
    source: &str,
    align: ParagraphAlign,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
    options: &Options<'_>,
) -> Vec<MarkdownBlock> {
    let lines = SourceLines::new(source);
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, source, options);
    let mut blocks = Vec::new();
    let mut previous_end = None;

    for node in root.children() {
        let Some((block, end)) = content_block(node, align, assets, links, 0) else {
            continue;
        };
        push_blank_lines(
            &lines,
            previous_end,
            node.data().sourcepos.start.line,
            &mut blocks,
        );
        blocks.push(block);
        previous_end = Some(end);
    }

    blocks
}

fn parse_pieces(
    node: Node<'_>,
    style: PieceStyle,
    nesting: usize,
    kbd: &mut KbdState,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
    out: &mut Vec<MarkdownPiece>,
) {
    if nesting > MAX_INLINE_NESTING {
        return;
    }
    let mut buffer = None;
    for child in node.children() {
        let style = PieceStyle {
            kbd: kbd.current,
            ..style
        };
        match &child.data().value {
            NodeValue::HtmlInline(tag) => match tag.as_str() {
                "<kbd>" => kbd.open(),
                "</kbd>" => kbd.close(),
                _ => {}
            },
            NodeValue::Text(text) => push_text(&mut buffer, out, text, &style),
            NodeValue::Code(code) => {
                if code.literal.is_empty() {
                    continue;
                }
                flush(&mut buffer, out);
                out.push(MarkdownPiece::Text {
                    text: SmolStr::new(&code.literal),
                    strong: style.strong,
                    emphasis: style.emphasis,
                    strike: style.strike,
                    code: true,
                    link: style.link,
                    kbd: style.kbd,
                });
            }
            NodeValue::Strong => {
                flush(&mut buffer, out);
                parse_pieces(
                    child,
                    PieceStyle {
                        strong: true,
                        ..style
                    },
                    nesting + 1,
                    kbd,
                    assets,
                    links,
                    out,
                );
            }
            NodeValue::Emph => {
                flush(&mut buffer, out);
                parse_pieces(
                    child,
                    PieceStyle {
                        emphasis: true,
                        ..style
                    },
                    nesting + 1,
                    kbd,
                    assets,
                    links,
                    out,
                );
            }
            NodeValue::Strikethrough => {
                flush(&mut buffer, out);
                parse_pieces(
                    child,
                    PieceStyle {
                        strike: true,
                        ..style
                    },
                    nesting + 1,
                    kbd,
                    assets,
                    links,
                    out,
                );
            }
            NodeValue::Link(link) => {
                flush(&mut buffer, out);
                parse_link(child, &link.url, style, nesting, kbd, assets, links, out);
            }
            NodeValue::Image(image) => {
                flush(&mut buffer, out);
                parse_image(image, &style, assets, out);
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => {
                flush(&mut buffer, out);
                out.push(MarkdownPiece::LineBreak);
            }
            _ => {}
        }
    }
    flush(&mut buffer, out);
}

fn parse_link(
    node: Node<'_>,
    url: &str,
    style: PieceStyle,
    nesting: usize,
    kbd: &mut KbdState,
    assets: &dyn MarkdownAssets,
    links: &mut Vec<SmolStr>,
    out: &mut Vec<MarkdownPiece>,
) {
    let mut inner = Vec::new();
    parse_pieces(node, style, nesting + 1, kbd, assets, links, &mut inner);
    if inner.is_empty() {
        return;
    }
    let id = LinkId(links.len());
    links.push(SmolStr::new(url));
    for piece in inner {
        out.push(piece.with_link(id));
    }
}

fn parse_image(
    image: &NodeLink,
    style: &PieceStyle,
    assets: &dyn MarkdownAssets,
    out: &mut Vec<MarkdownPiece>,
) {
    let Some(source) = InlineImageSource::parse(&image.url) else {
        return;
    };
    let size = source.resolve(assets);
    out.push(MarkdownPiece::InlineImage {
        path: source.path,
        format: source.format,
        size,
        link: style.link,
        kbd: style.kbd,
    });
}

pub(super) fn push_text(
    buffer: &mut Option<TextBuffer>,
    out: &mut Vec<MarkdownPiece>,
    text: &str,
    style: &PieceStyle,
) {
    if text.is_empty() {
        return;
    }
    match buffer {
        Some(current) if current.style == *style => current.text.push_str(text),
        _ => {
            flush(buffer, out);
            *buffer = Some(TextBuffer {
                text: text.to_string(),
                style: *style,
            });
        }
    }
}

pub(super) fn flush(buffer: &mut Option<TextBuffer>, out: &mut Vec<MarkdownPiece>) {
    let Some(current) = buffer.take() else {
        return;
    };
    out.push(MarkdownPiece::Text {
        text: SmolStr::from(current.text),
        strong: current.style.strong,
        emphasis: current.style.emphasis,
        strike: current.style.strike,
        code: false,
        link: current.style.link,
        kbd: current.style.kbd,
    });
}
