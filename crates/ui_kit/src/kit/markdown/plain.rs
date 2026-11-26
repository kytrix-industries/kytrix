use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use smol_str::SmolStr;

use super::html::{self, BlockTag, HtmlBudget, InlineTag, MAX_HTML_BLOCK_BYTES};

pub(super) fn plain_text(source: &str) -> SmolStr {
    let mut budget = HtmlBudget::default();
    SmolStr::new(text_with_budget(source, &mut budget))
}

fn text_with_budget(source: &str, budget: &mut HtmlBudget) -> String {
    let mut collector = Collector {
        buffers: vec![String::new()],
        cell: None,
        cells: 0,
        code: None,
        html: None,
        image: 0,
        budget,
    };
    collector.collect(source);
    collector.finish()
}

struct HtmlBuf {
    start: usize,
    top_level: bool,
    payload: String,
}

struct Collector<'a> {
    buffers: Vec<String>,
    cell: Option<String>,
    cells: usize,
    code: Option<String>,
    html: Option<HtmlBuf>,
    image: usize,
    budget: &'a mut HtmlBudget,
}

impl Collector<'_> {
    fn collect(&mut self, source: &str) {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            self.event(event, range, source);
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>, source: &str) {
        if self.image > 0 {
            match event {
                Event::Start(Tag::Image { .. }) => self.image += 1,
                Event::End(TagEnd::Image) => self.image -= 1,
                _ => {}
            }
            return;
        }
        if let Some(code) = self.code.as_mut() {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().unwrap_or_default();
                    self.push_block(code.strip_suffix('\n').unwrap_or(&code));
                }
                _ => {}
            }
            return;
        }
        if self.html.is_some() {
            match &event {
                Event::Html(text) | Event::Text(text) => {
                    if !range.is_empty()
                        && let Some(html) = self.html.as_mut()
                        && !html.top_level
                    {
                        html.payload.push_str(text);
                    }
                }
                Event::End(TagEnd::HtmlBlock) => self.finish_html_block(&range, source),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Code(text) => self.sink().push_str(&text),
            Event::SoftBreak | Event::HardBreak => self.sink().push('\n'),
            Event::InlineHtml(tag) => {
                if matches!(html::inline_tag(&tag), InlineTag::Break) {
                    self.sink().push('\n');
                }
            }
            Event::Rule
            | Event::Html(_)
            | Event::TaskListMarker(_)
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::List(_)
            | Tag::Item
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow => self.buffers.push(String::new()),
            Tag::TableCell => self.cell = Some(String::new()),
            Tag::CodeBlock(_) => self.code = Some(String::new()),
            Tag::HtmlBlock => {
                self.html = Some(HtmlBuf {
                    start: range.start,
                    top_level: self.buffers.len() == 1,
                    payload: String::new(),
                });
            }
            Tag::Image { .. } => self.image = 1,
            Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Link { .. }
            | Tag::Superscript
            | Tag::Subscript
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow => {
                self.cells = 0;
                self.close_buffer();
            }
            TagEnd::TableCell => {
                let cell = self.cell.take().unwrap_or_default();
                let separator = self.cells > 0;
                self.cells += 1;
                let buffer = self.sink();
                if separator {
                    buffer.push(' ');
                }
                buffer.push_str(&cell);
            }
            TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::Image
            | TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn finish_html_block(&mut self, range: &Range<usize>, source: &str) {
        let Some(html) = self.html.take() else {
            return;
        };
        let payload = if html.top_level {
            source.get(html.start..range.end).unwrap_or_default()
        } else {
            html.payload.as_str()
        };
        self.push_html(payload);
    }

    fn push_html(&mut self, payload: &str) {
        if html::is_tag_block(payload) {
            return;
        }
        if payload.len() > MAX_HTML_BLOCK_BYTES {
            let text = html::strip_tags(payload);
            self.push_block(&text);
            return;
        }
        let tags = html::block_tags(payload);
        if tags.is_empty() {
            self.push_fragment(&html::strip_tags(payload));
            return;
        }
        let pairs = html::pairs(&tags);
        let segments = html::segments(payload, &tags);
        let mut outside: Vec<Range<usize>> = Vec::new();
        let mut index = 0;
        while index < tags.len() {
            match tags[index] {
                BlockTag::Open { end, .. } if pairs[index].is_some() => {
                    collect_text(payload, segments[index].clone(), &mut outside);
                    let close = pairs[index].unwrap_or(index);
                    if let Some(content) = payload.get(end..tags[close].at()) {
                        self.push_fragment(content);
                    }
                    index = close + 1;
                }
                _ => {
                    collect_text(payload, segments[index].clone(), &mut outside);
                    index += 1;
                }
            }
        }
        collect_text(payload, segments[tags.len()].clone(), &mut outside);
        if outside.is_empty() {
            return;
        }
        let mut text = String::new();
        for range in outside {
            let piece = html::text_region(payload.get(range).unwrap_or_default());
            if piece.trim().is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&piece);
        }
        self.push_fragment(&text);
    }

    fn push_fragment(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if !self.budget.admit(text.len()) {
            let text = html::strip_tags(text);
            self.push_block(&text);
            return;
        }
        self.budget.charge(text.len());
        let text = text_with_budget(text, &mut *self.budget);
        self.push_block(&text);
    }

    fn close_buffer(&mut self) {
        if self.buffers.len() < 2 {
            return;
        }
        let buffer = self.buffers.pop().unwrap_or_default();
        self.push_block(&buffer);
    }

    fn push_block(&mut self, text: &str) {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            return;
        }
        let buffer = self.buffers.last_mut().expect("plain text root buffer");
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(text);
    }

    fn sink(&mut self) -> &mut String {
        if let Some(cell) = self.cell.as_mut() {
            return cell;
        }
        self.buffers.last_mut().expect("plain text root buffer")
    }

    fn finish(self) -> String {
        let root = self.buffers.into_iter().next().unwrap_or_default();
        root.trim_end_matches('\n').to_owned()
    }
}

fn collect_text(payload: &str, range: Range<usize>, outside: &mut Vec<Range<usize>>) {
    if payload
        .get(range.clone())
        .is_some_and(|text| !text.trim().is_empty())
    {
        outside.push(range);
    }
}
