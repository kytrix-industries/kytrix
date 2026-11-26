use std::ops::Range;

use pulldown_cmark::Alignment;
use smol_str::SmolStr;

use super::inline::Mark;

const HTML_BLOCK_TAGS: [&str; 4] = ["pre", "script", "style", "textarea"];
const COMMENT_OPEN: &str = "<!--";
const COMMENT_CLOSE: &str = "-->";

const MAX_HTML_NESTED_PARSES: usize = 16;
const MAX_HTML_INNER_BYTES: usize = 64 * 1024;
pub(super) const MAX_HTML_BLOCK_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct HtmlBudget {
    parses: usize,
    bytes: usize,
}

impl HtmlBudget {
    pub(super) fn admit(&self, bytes: usize) -> bool {
        self.parses < MAX_HTML_NESTED_PARSES && self.bytes + bytes <= MAX_HTML_INNER_BYTES
    }

    pub(super) fn charge(&mut self, bytes: usize) {
        self.parses += 1;
        self.bytes += bytes;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TagName {
    Paragraph,
    Division,
    Section,
    Center,
    Heading(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum FragmentKind {
    Paragraph(Option<Alignment>),
    Heading(u8),
}

impl FragmentKind {
    pub(super) fn align(self) -> Alignment {
        match self {
            FragmentKind::Paragraph(Some(align)) => align,
            FragmentKind::Paragraph(None) | FragmentKind::Heading(_) => Alignment::Left,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum BlockTag {
    Open {
        name: TagName,
        kind: FragmentKind,
        at: usize,
        end: usize,
    },
    Close {
        name: TagName,
        at: usize,
        end: usize,
    },
    Rule {
        at: usize,
        end: usize,
    },
}

impl BlockTag {
    pub(super) fn at(self) -> usize {
        match self {
            BlockTag::Open { at, .. } | BlockTag::Close { at, .. } | BlockTag::Rule { at, .. } => {
                at
            }
        }
    }

    pub(super) fn end(self) -> usize {
        match self {
            BlockTag::Open { end, .. }
            | BlockTag::Close { end, .. }
            | BlockTag::Rule { end, .. } => end,
        }
    }
}

impl TagName {
    fn slot(self) -> usize {
        match self {
            TagName::Paragraph => 0,
            TagName::Division => 1,
            TagName::Section => 2,
            TagName::Center => 3,
            TagName::Heading(level) => 3 + level as usize,
        }
    }
}

pub(super) fn pairs(tags: &[BlockTag]) -> Vec<Option<usize>> {
    const SLOTS: usize = 10;
    let mut pairs = vec![None; tags.len()];
    let mut opens: [Vec<usize>; SLOTS] = Default::default();
    for (index, tag) in tags.iter().enumerate() {
        match *tag {
            BlockTag::Open { name, .. } => {
                if name == TagName::Paragraph && !opens[name.slot()].is_empty() {
                    continue;
                }
                opens[name.slot()].push(index);
            }
            BlockTag::Close { name, .. } => {
                if let Some(opened) = opens[name.slot()].pop() {
                    pairs[opened] = Some(index);
                    pairs[index] = Some(opened);
                }
            }
            BlockTag::Rule { .. } => {}
        }
    }
    pairs
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InlineTag {
    Open(Mark),
    Close(Mark),
    Break,
    Image {
        src: Option<SmolStr>,
        alt: Option<SmolStr>,
    },
    Transparent,
    ParagraphClose,
    Other,
}

pub(super) fn is_tag_block(payload: &str) -> bool {
    let Some(rest) = payload.strip_prefix('<') else {
        return false;
    };
    if rest.starts_with("!--") || rest.starts_with('?') || rest.starts_with("![CDATA[") {
        return true;
    }
    if let Some(name) = rest.strip_prefix('!') {
        return name.starts_with(|character: char| character.is_ascii_alphabetic());
    }
    HTML_BLOCK_TAGS.iter().any(|name| {
        let Some(prefix) = rest.get(..name.len()) else {
            return false;
        };
        if !prefix.eq_ignore_ascii_case(name) {
            return false;
        }
        let Some(tail) = rest.get(name.len()..) else {
            return false;
        };
        tail.is_empty() || tail.starts_with([' ', '\t', '\r', '\n', '>'])
    })
}

pub(super) fn block_tags(payload: &str) -> Vec<BlockTag> {
    let mut tags = Vec::with_capacity(4);
    let mut cursor = 0;
    while let Some(range) = next_tag(&payload[cursor..]) {
        let start = cursor + range.start;
        let end = cursor + range.end;
        if let Some(tag) = block_tag(&payload[start..end], start, end) {
            tags.push(tag);
        }
        cursor = end;
    }
    tags
}

pub(super) fn segments(payload: &str, tags: &[BlockTag]) -> Vec<Range<usize>> {
    let mut segments = Vec::with_capacity(tags.len() + 1);
    let mut cursor = 0;
    for tag in tags {
        segments.push(cursor..tag.at());
        cursor = tag.end();
    }
    segments.push(cursor..payload.len());
    segments
}

pub(super) fn strip_tags(text: &str) -> String {
    strip_where(text, |_| false)
}

pub(super) fn text_region(text: &str) -> String {
    strip_where(text, |tag| {
        matches!(
            inline_tag(tag),
            InlineTag::Open(_)
                | InlineTag::Close(_)
                | InlineTag::Break
                | InlineTag::Image { .. }
                | InlineTag::Transparent
        )
    })
}

pub(super) fn inline_tag(html: &str) -> InlineTag {
    let Some(inner) = html
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return InlineTag::Other;
    };
    if inner.starts_with('!') || inner.starts_with('?') {
        return InlineTag::Other;
    }
    let (closing, body) = match inner.strip_prefix('/') {
        Some(body) => (true, body),
        None => (false, inner),
    };
    let name_end = body
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(body.len());
    let name = &body[..name_end];
    let rest = &body[name_end..];
    let self_closing = rest == "/";
    if !rest.is_empty()
        && !self_closing
        && !rest.starts_with(|character: char| character.is_ascii_whitespace())
    {
        return InlineTag::Other;
    }
    if !closing && name.eq_ignore_ascii_case("br") {
        let rest = rest.trim();
        if rest.is_empty() || rest == "/" {
            return InlineTag::Break;
        }
    }
    if !closing && name.eq_ignore_ascii_case("img") {
        let (src, alt) = image_attributes(rest);
        return InlineTag::Image { src, alt };
    }
    if name.eq_ignore_ascii_case("kbd") {
        return match html {
            "<kbd>" => InlineTag::Open(Mark::Kbd),
            "</kbd>" => InlineTag::Close(Mark::Kbd),
            _ => InlineTag::Other,
        };
    }
    if name.eq_ignore_ascii_case("p") {
        return if closing && html == "</p>" {
            InlineTag::ParagraphClose
        } else {
            InlineTag::Other
        };
    }
    if name.eq_ignore_ascii_case("span") {
        return InlineTag::Transparent;
    }
    if self_closing {
        return InlineTag::Other;
    }
    let Some(mark) = mark_of(name) else {
        return InlineTag::Other;
    };
    if closing {
        InlineTag::Close(mark)
    } else {
        InlineTag::Open(mark)
    }
}

fn mark_of(name: &str) -> Option<Mark> {
    if name.eq_ignore_ascii_case("b") || name.eq_ignore_ascii_case("strong") {
        return Some(Mark::Strong);
    }
    if name.eq_ignore_ascii_case("i") || name.eq_ignore_ascii_case("em") {
        return Some(Mark::Emphasis);
    }
    if name.eq_ignore_ascii_case("del") || name.eq_ignore_ascii_case("s") {
        return Some(Mark::Strike);
    }
    if name.eq_ignore_ascii_case("code") {
        return Some(Mark::Code);
    }
    if name.eq_ignore_ascii_case("a") {
        return Some(Mark::Link);
    }
    None
}

fn strip_where(text: &str, keep: impl Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(range) = next_tag(&text[cursor..]) {
        let start = cursor + range.start;
        let end = cursor + range.end;
        out.push_str(&text[cursor..start]);
        if keep(&text[start..end]) {
            out.push_str(&text[start..end]);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn block_tag(tag: &str, at: usize, end: usize) -> Option<BlockTag> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    if inner.starts_with(['!', '?']) {
        return None;
    }
    let Some(body) = inner.strip_prefix('/') else {
        return open_block_tag(inner, at, end);
    };
    let (name, rest) = split_name(body);
    if !rest.is_empty() {
        return None;
    }
    let name = close_name(name)?;
    Some(BlockTag::Close { name, at, end })
}

fn open_block_tag(inner: &str, at: usize, end: usize) -> Option<BlockTag> {
    let (name, rest) = split_name(inner);
    if name == "hr" {
        let compact = rest.trim();
        return (compact.is_empty() || compact == "/").then_some(BlockTag::Rule { at, end });
    }
    if name == "p" {
        return Some(BlockTag::Open {
            name: TagName::Paragraph,
            kind: FragmentKind::Paragraph(parse_align(rest)?),
            at,
            end,
        });
    }
    if !rest.is_empty() {
        return None;
    }
    let (name, kind) = open_name(name)?;
    Some(BlockTag::Open {
        name,
        kind,
        at,
        end,
    })
}

fn split_name(text: &str) -> (&str, &str) {
    let name_end = text
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(text.len());
    (&text[..name_end], &text[name_end..])
}

fn close_name(name: &str) -> Option<TagName> {
    match name {
        "p" => Some(TagName::Paragraph),
        "div" => Some(TagName::Division),
        "section" => Some(TagName::Section),
        "center" => Some(TagName::Center),
        "h1" => Some(TagName::Heading(1)),
        "h2" => Some(TagName::Heading(2)),
        "h3" => Some(TagName::Heading(3)),
        "h4" => Some(TagName::Heading(4)),
        "h5" => Some(TagName::Heading(5)),
        "h6" => Some(TagName::Heading(6)),
        _ => None,
    }
}

fn open_name(name: &str) -> Option<(TagName, FragmentKind)> {
    match name {
        "div" => Some((TagName::Division, FragmentKind::Paragraph(None))),
        "section" => Some((TagName::Section, FragmentKind::Paragraph(None))),
        "center" => Some((
            TagName::Center,
            FragmentKind::Paragraph(Some(Alignment::Center)),
        )),
        "h1" => Some((TagName::Heading(1), FragmentKind::Heading(1))),
        "h2" => Some((TagName::Heading(2), FragmentKind::Heading(2))),
        "h3" => Some((TagName::Heading(3), FragmentKind::Heading(3))),
        "h4" => Some((TagName::Heading(4), FragmentKind::Heading(4))),
        "h5" => Some((TagName::Heading(5), FragmentKind::Heading(5))),
        "h6" => Some((TagName::Heading(6), FragmentKind::Heading(6))),
        _ => None,
    }
}

fn parse_align(rest: &str) -> Option<Option<Alignment>> {
    if rest.is_empty() {
        return Some(None);
    }
    let rest = rest.strip_prefix(" align=\"")?;
    let end = rest.find('"')?;
    let align = match &rest[..end] {
        "left" => Alignment::Left,
        "center" => Alignment::Center,
        "right" => Alignment::Right,
        _ => return None,
    };
    rest[end + 1..].is_empty().then_some(Some(align))
}

fn image_attributes(body: &str) -> (Option<SmolStr>, Option<SmolStr>) {
    let mut src = None;
    let mut alt = None;
    let mut rest = body;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return (src, alt);
        }
        if let Some(after) = rest.strip_prefix('/') {
            rest = after;
            continue;
        }
        let name_end = rest
            .find(|character: char| character.is_ascii_whitespace() || character == '=')
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        rest = rest[name_end..].trim_start();
        let Some(after) = rest.strip_prefix('=') else {
            return (src, alt);
        };
        rest = after.trim_start();
        let value;
        if let Some(after) = rest.strip_prefix('"') {
            let Some(end) = after.find('"') else {
                return (src, alt);
            };
            value = &after[..end];
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('\'') {
            let Some(end) = after.find('\'') else {
                return (src, alt);
            };
            value = &after[..end];
            rest = &after[end + 1..];
        } else {
            let end = rest
                .find(|character: char| character.is_ascii_whitespace())
                .unwrap_or(rest.len());
            value = &rest[..end];
            rest = &rest[end..];
        }
        if name.eq_ignore_ascii_case("src") {
            src = Some(SmolStr::new(value));
        } else if name.eq_ignore_ascii_case("alt") {
            alt = Some(SmolStr::new(value));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_block_types_one_to_five_are_recognised_by_prefix() {
        for payload in [
            "<!-- comment -->",
            "<?xml version=\"1.0\"?>",
            "<![CDATA[x]]>",
            "<!DOCTYPE html>",
            "<PRE>",
            "<pre>",
            "<pre",
            "<script src=\"x\">",
            "<style>",
            "<textarea>",
            "<TEXTAREA>",
        ] {
            assert!(is_tag_block(payload), "{payload}");
        }
        for payload in [
            "<p>",
            "<div>",
            "<h1>",
            "<param>",
            "<prefix>",
            "text",
            "<p align=\"center\">",
        ] {
            assert!(!is_tag_block(payload), "{payload}");
        }
    }

    #[test]
    fn block_tags_only_take_exact_forms() {
        assert_eq!(
            block_tags("<p align=\"center\">x</p>"),
            vec![
                BlockTag::Open {
                    name: TagName::Paragraph,
                    kind: FragmentKind::Paragraph(Some(Alignment::Center)),
                    at: 0,
                    end: 18,
                },
                BlockTag::Close {
                    name: TagName::Paragraph,
                    at: 19,
                    end: 23,
                },
            ]
        );
        for payload in [
            "<p align='center'>x</p>",
            "<p ALIGN=\"center\">x</p>",
            "<p align=\"justify\">x</p>",
            "<p align=center>x</p>",
            "<p align = \"center\">x</p>",
            "<p >x</p>",
            "<p\n>x</p>",
            "<div >x</div>",
            "<center >x</center>",
            "<h1 >x</h1>",
            "</p >",
            "<p align=\"center\" class=\"x\">x</p>",
        ] {
            assert!(
                block_tags(payload)
                    .iter()
                    .all(|tag| !matches!(tag, BlockTag::Open { .. })),
                "{payload}"
            );
        }
        assert_eq!(
            block_tags("<h1>a</h1>")[0],
            BlockTag::Open {
                name: TagName::Heading(1),
                kind: FragmentKind::Heading(1),
                at: 0,
                end: 4,
            }
        );
        assert_eq!(block_tags("<hr/>"), vec![BlockTag::Rule { at: 0, end: 5 }]);
    }

    #[test]
    fn tags_pair_by_name_and_nested_paragraphs_are_content() {
        let tags = block_tags("<div>a<p>x</p>b</div>");
        assert_eq!(
            tags.iter().map(|tag| tag.at()).collect::<Vec<usize>>(),
            vec![0, 6, 10, 15]
        );
        assert_eq!(pairs(&tags), vec![Some(3), Some(2), Some(1), Some(0)]);

        let tags = block_tags("<center>a<p>x</p>b</center>");
        assert_eq!(pairs(&tags), vec![Some(3), Some(2), Some(1), Some(0)]);

        let tags = block_tags("<div>a<div>b</div>c</div>");
        assert_eq!(pairs(&tags), vec![Some(3), Some(2), Some(1), Some(0)]);

        let tags = block_tags("<p>a<p>b</p>c</p>");
        assert_eq!(pairs(&tags), vec![Some(2), None, Some(0), None]);

        let tags = block_tags("<center>a</p>b</center>");
        assert_eq!(pairs(&tags), vec![Some(2), None, Some(0)]);
    }

    #[test]
    fn the_tag_scanner_respects_quotes_and_comments() {
        let payload = "<div title=\"x <p align=center>y</p>\">z</div>";
        assert_eq!(
            block_tags(payload),
            vec![BlockTag::Close {
                name: TagName::Division,
                at: 38,
                end: 44,
            }]
        );
        assert_eq!(segments(payload, &block_tags(payload)).len(), 2);

        let comment =
            "<div>\n<!-- > <p align=\"center\">hidden</p> -->\n<p align=\"center\">x</p>\n</div>";
        let tags = block_tags(comment);
        assert_eq!(tags.len(), 4);
        assert_eq!(tags[0].at(), 0);

        assert_eq!(text_region("a <kbd>K</kbd> b"), "a <kbd>K</kbd> b");
        assert_eq!(text_region("<p align=\"center\">a</p>"), "a");
    }

    #[test]
    fn inline_tags_follow_the_whitelist() {
        assert_eq!(inline_tag("<kbd>"), InlineTag::Open(Mark::Kbd));
        assert_eq!(inline_tag("</kbd>"), InlineTag::Close(Mark::Kbd));
        assert_eq!(inline_tag("<KBD>"), InlineTag::Other);
        assert_eq!(inline_tag("<kbd class=\"k\">"), InlineTag::Other);
        assert_eq!(inline_tag("<B>"), InlineTag::Open(Mark::Strong));
        assert_eq!(inline_tag("</strong>"), InlineTag::Close(Mark::Strong));
        assert_eq!(inline_tag("<em>"), InlineTag::Open(Mark::Emphasis));
        assert_eq!(inline_tag("<del>"), InlineTag::Open(Mark::Strike));
        assert_eq!(inline_tag("<code>"), InlineTag::Open(Mark::Code));
        assert_eq!(inline_tag("<a href=\"u\">"), InlineTag::Open(Mark::Link));
        assert_eq!(inline_tag("<span>"), InlineTag::Transparent);
        assert_eq!(inline_tag("<br>"), InlineTag::Break);
        assert_eq!(inline_tag("<br/>"), InlineTag::Break);
        assert_eq!(inline_tag("<BR />"), InlineTag::Break);
        assert_eq!(inline_tag("</p>"), InlineTag::ParagraphClose);
        assert_eq!(inline_tag("</p >"), InlineTag::Other);
        assert_eq!(inline_tag("<!-- c -->"), InlineTag::Other);
        assert_eq!(
            inline_tag("<img src=\"file://a.png?size=24\" alt=\"z\">"),
            InlineTag::Image {
                src: Some(SmolStr::new_static("file://a.png?size=24")),
                alt: Some(SmolStr::new_static("z")),
            }
        );
        assert_eq!(
            inline_tag("<img alt='a>b'>"),
            InlineTag::Image {
                src: None,
                alt: Some(SmolStr::new_static("a>b")),
            }
        );
    }

    #[test]
    fn inline_tag_names_must_end_at_the_tag_boundary() {
        for tag in ["<a/>", "<a-b>", "<b-tag>", "<code-x>", "</a-b>"] {
            assert_eq!(inline_tag(tag), InlineTag::Other, "{tag}");
        }
        assert_eq!(inline_tag("<a href=\"u\">"), InlineTag::Open(Mark::Link));
        assert_eq!(inline_tag("<B>"), InlineTag::Open(Mark::Strong));
        assert_eq!(inline_tag("<b>"), InlineTag::Open(Mark::Strong));
        assert_eq!(inline_tag("<br/>"), InlineTag::Break);
    }
}
