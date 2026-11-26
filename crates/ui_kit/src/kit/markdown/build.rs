use std::ops::Range;

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    math::{LogicalPx, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        style::Style,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextAlign, TextStyle, TextWrap},
    window::WindowId,
};

use super::{
    MarkdownAssets, MarkdownStyle,
    assets::InlineImageSource,
    html::{self, BlockTag, FragmentKind, HtmlBudget, InlineTag, MAX_HTML_BLOCK_BYTES},
    inline::{InlineBuf, InlineContent, InlineKind, Mark, Marks},
    lines::SourceLines,
    plain,
    table::{CellBuf, TableBuf},
};

const BLOCK_GAP: LogicalPx = LogicalPx::new(8.0);
const HEADING_SPACE_ABOVE: LogicalPx = LogicalPx::new(8.0);
const CODE_PADDING: LogicalPx = LogicalPx::new(8.0);
const CODE_RADIUS: LogicalPx = LogicalPx::new(4.0);
const QUOTE_BORDER_WIDTH: LogicalPx = LogicalPx::new(3.0);
const QUOTE_PADDING: LogicalPx = LogicalPx::new(8.0);
const LIST_MARKER_WIDTH: LogicalPx = LogicalPx::new(20.0);
pub(super) const CELL_PADDING: LogicalPx = LogicalPx::new(4.0);
const TABLE_RULE_HEIGHT: LogicalPx = LogicalPx::new(1.0);
const TASK_SIZE: LogicalPx = LogicalPx::new(14.0);
const TASK_RADIUS: LogicalPx = LogicalPx::new(2.0);
const TASK_BORDER_WIDTH: LogicalPx = LogicalPx::new(1.5);
const TASK_CHECK_SIZE: LogicalPx = LogicalPx::new(8.0);
const MAX_BLOCK_NESTING: usize = 64;
const MAX_INLINE_NESTING: usize = 64;
const MAX_HTML_BLOCK_TAGS: usize = 4096;
const MAX_HTML_DOCUMENT_TAGS: usize = 16 * 1024;

const CODE_ID: &str = "MARKDOWN_CODE";
const QUOTE_ID: &str = "MARKDOWN_QUOTE";
const QUOTE_BAR_ID: &str = "MARKDOWN_QUOTE_BAR";
const QUOTE_BODY_ID: &str = "MARKDOWN_QUOTE_BODY";
const LIST_ID: &str = "MARKDOWN_LIST";
const LIST_ITEM_ID: &str = "MARKDOWN_LIST_ITEM";
const LIST_MARKER_ID: &str = "MARKDOWN_LIST_MARKER";
const LIST_CONTENT_ID: &str = "MARKDOWN_LIST_CONTENT";
const LIST_CONTENT_TIGHT_ID: &str = "MARKDOWN_LIST_CONTENT_TIGHT";
const TABLE_ID: &str = "MARKDOWN_TABLE";
const TABLE_ROW_ID: &str = "MARKDOWN_TABLE_ROW";
const TABLE_CELL_ID: &str = "MARKDOWN_TABLE_CELL";
const TABLE_RULE_ID: &str = "MARKDOWN_TABLE_RULE";
const RULE_ID: &str = "MARKDOWN_RULE";
const SPACER_ID: &str = "MARKDOWN_SPACER";

pub(super) const ROOT_ID: &str = "MARKDOWN";

pub(super) struct Builder<'a> {
    app: &'a mut App,
    window: WindowId,
    style: &'a MarkdownStyle,
    assets: &'a dyn MarkdownAssets,
    containers: Vec<Frame>,
    inline: Option<InlineBuf>,
    cell: Option<InlineBuf>,
    marks: Vec<Mark>,
    clipped_marks: usize,
    skipped_blocks: usize,
    suppressed: String,
    lines: SourceLines,
    last_block: Option<Range<usize>>,
    open_block: Option<usize>,
    code: Option<String>,
    html: Option<HtmlBuf>,
    image: Option<ImageBuf>,
    table: Option<TableBuf>,
    align_windows: Vec<Alignment>,
    pending_window_closes: usize,
    fragment: Option<FragmentKind>,
    base_depth: usize,
    parse_depth: usize,
    html_tags: usize,
    budget: HtmlBudget,
}

struct Frame {
    node: NodeId,
    kind: FrameKind,
    block_index: usize,
    previous_blank: bool,
}

enum FrameKind {
    Root,
    Quote,
    List {
        next: u64,
        ordered: bool,
        loose: bool,
        last_item_end: Option<usize>,
        unfilled: Vec<NodeId>,
    },
    Item(ItemState),
}

struct ItemState {
    row: NodeId,
    loose: bool,
    column: bool,
    filled: bool,
    marker: Option<Marker>,
    task: Option<bool>,
    marked: bool,
}

struct Marker {
    text: SmolStr,
    width: LogicalPx,
}

struct HtmlBuf {
    start: usize,
    top_level: bool,
    payload: String,
}

#[derive(Default)]
struct OutsideText {
    ranges: Vec<Range<usize>>,
    kind: Option<FragmentKind>,
}

struct ImageBuf {
    dest: SmolStr,
    alt: String,
    depth: usize,
}

struct FragmentState {
    inline: Option<InlineBuf>,
    cell: Option<InlineBuf>,
    marks: Vec<Mark>,
    clipped_marks: usize,
    skipped_blocks: usize,
    suppressed: String,
    lines: SourceLines,
    last_block: Option<Range<usize>>,
    open_block: Option<usize>,
    code: Option<String>,
    html: Option<HtmlBuf>,
    image: Option<ImageBuf>,
    table: Option<TableBuf>,
    align_windows: Vec<Alignment>,
    pending_window_closes: usize,
    fragment: Option<FragmentKind>,
    base_depth: usize,
    parse_depth: usize,
}

impl<'a> Builder<'a> {
    pub(super) fn new(
        app: &'a mut App,
        window: WindowId,
        style: &'a MarkdownStyle,
        assets: &'a dyn MarkdownAssets,
        root: NodeId,
    ) -> Self {
        Self {
            app,
            window,
            style,
            assets,
            containers: vec![Frame {
                node: root,
                kind: FrameKind::Root,
                block_index: 0,
                previous_blank: false,
            }],
            inline: None,
            cell: None,
            marks: Vec::with_capacity(4),
            clipped_marks: 0,
            skipped_blocks: 0,
            suppressed: String::new(),
            lines: SourceLines::default(),
            last_block: None,
            open_block: None,
            code: None,
            html: None,
            image: None,
            table: None,
            align_windows: Vec::with_capacity(2),
            pending_window_closes: 0,
            fragment: None,
            base_depth: 0,
            parse_depth: 0,
            html_tags: 0,
            budget: HtmlBudget::default(),
        }
    }

    pub(super) fn build(mut self, source: &str) -> Result<(), EngineError> {
        self.lines = SourceLines::new(source);
        self.parse(source)?;
        self.flush_inline()
    }

    fn parse(&mut self, source: &str) -> Result<(), EngineError> {
        #[cfg(test)]
        PARSE_COUNT.with(|count| count.set(count.get() + 1));
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            self.handle(event, range, source)?;
        }
        Ok(())
    }

    fn handle(
        &mut self,
        event: Event<'_>,
        range: Range<usize>,
        source: &str,
    ) -> Result<(), EngineError> {
        if self.image.is_some() {
            return self.image_event(event);
        }
        if self.code.is_some() {
            match event {
                Event::Text(text) => {
                    if self.skipped_blocks > 0 {
                        self.suppressed.push_str(&text);
                    } else if let Some(code) = self.code.as_mut() {
                        code.push_str(&text);
                    }
                    return Ok(());
                }
                Event::End(TagEnd::CodeBlock) => return self.finish_code_block(&range),
                _ => return Ok(()),
            }
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
                Event::End(TagEnd::HtmlBlock) => return self.finish_html_block(&range, source),
                _ => {}
            }
            return Ok(());
        }
        match event {
            Event::Start(tag) => self.start(tag, range, source),
            Event::End(tag) => self.end(tag, range, source),
            Event::Text(text) => {
                self.push_text(&text);
                Ok(())
            }
            Event::Code(text) => {
                self.push_code(&text);
                Ok(())
            }
            Event::SoftBreak | Event::HardBreak => self.push_break(),
            Event::Rule => self.push_rule(&range, source),
            Event::InlineHtml(tag) => self.push_inline_html(&tag),
            Event::TaskListMarker(checked) => {
                self.set_task(checked);
                Ok(())
            }
            Event::Html(_)
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => Ok(()),
        }
    }

    fn start(
        &mut self,
        tag: Tag<'_>,
        range: Range<usize>,
        source: &str,
    ) -> Result<(), EngineError> {
        match tag {
            Tag::Paragraph => {
                let kind = self.paragraph_kind();
                self.start_leaf_block(range.start, source, kind)
            }
            Tag::Heading { level, .. } => self.start_leaf_block(
                range.start,
                source,
                InlineKind::Heading(heading_level(level)),
            ),
            Tag::BlockQuote(_) => self.start_quote(range.start, source),
            Tag::List(start) => self.start_list(start, range.start, source),
            Tag::Item => self.start_item(range.start, source),
            Tag::CodeBlock(_) => self.start_code_block(range.start, source),
            Tag::HtmlBlock => self.start_html_block(range.start, source),
            Tag::Table(alignments) => self.start_table(alignments, range.start, source),
            Tag::TableHead => self.begin_table_row(true),
            Tag::TableRow => self.begin_table_row(false),
            Tag::TableCell => self.start_cell(),
            Tag::Emphasis => {
                self.start_mark(Mark::Emphasis);
                Ok(())
            }
            Tag::Strong => {
                self.start_mark(Mark::Strong);
                Ok(())
            }
            Tag::Strikethrough => {
                self.start_mark(Mark::Strike);
                Ok(())
            }
            Tag::Link { .. } => {
                self.start_mark(Mark::Link);
                Ok(())
            }
            Tag::Image { dest_url, .. } => {
                self.start_image(&dest_url);
                Ok(())
            }
            Tag::Superscript
            | Tag::Subscript
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => Ok(()),
        }
    }

    fn end(&mut self, tag: TagEnd, range: Range<usize>, source: &str) -> Result<(), EngineError> {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline()?;
                self.close_alignment_windows();
                self.close_top_block(&range);
                Ok(())
            }
            TagEnd::Heading(_) => {
                self.flush_inline()?;
                self.close_alignment_windows();
                self.close_top_block(&range);
                Ok(())
            }
            TagEnd::Item => {
                self.ensure_marker()?;
                self.flush_inline()?;
                self.close_alignment_windows();
                if self.item_needs_content_column() {
                    self.ensure_content()?;
                }
                let item_row = match self.containers.last().map(|frame| &frame.kind) {
                    Some(FrameKind::Item(item)) => Some((item.row, item.column || item.filled)),
                    _ => None,
                };
                self.leave_block_container(&range)?;
                if let Some((row, filled)) = item_row
                    && let Some(FrameKind::List {
                        last_item_end,
                        unfilled,
                        ..
                    }) = self.containers.last_mut().map(|frame| &mut frame.kind)
                {
                    *last_item_end = Some(range.end);
                    if !filled {
                        unfilled.push(row);
                    }
                }
                Ok(())
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) => {
                self.flush_inline()?;
                self.leave_block_container(&range)
            }
            TagEnd::CodeBlock => self.finish_code_block(&range),
            TagEnd::HtmlBlock => self.finish_html_block(&range, source),
            TagEnd::TableHead | TagEnd::TableRow => self.finish_row(),
            TagEnd::TableCell => {
                self.flush_inline()?;
                self.close_alignment_windows();
                self.finish_cell()
            }
            TagEnd::Table => self.finish_table(&range),
            TagEnd::Emphasis => {
                self.end_mark(Mark::Emphasis);
                Ok(())
            }
            TagEnd::Strong => {
                self.end_mark(Mark::Strong);
                Ok(())
            }
            TagEnd::Strikethrough => {
                self.end_mark(Mark::Strike);
                Ok(())
            }
            TagEnd::Link => {
                self.end_mark(Mark::Link);
                Ok(())
            }
            TagEnd::Image => self.finish_image(),
            TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => Ok(()),
        }
    }

    fn leave_block_container(&mut self, range: &Range<usize>) -> Result<(), EngineError> {
        if self.skipped_blocks > 0 {
            self.leave_container()?;
        } else {
            self.pop_frame();
        }
        self.close_alignment_windows();
        self.close_top_block(range);
        Ok(())
    }

    fn close_alignment_windows(&mut self) {
        for _ in 0..self.pending_window_closes {
            self.align_windows.pop();
        }
        self.pending_window_closes = 0;
    }

    fn start_leaf_block(
        &mut self,
        start: usize,
        source: &str,
        kind: InlineKind,
    ) -> Result<(), EngineError> {
        if self.skipped_blocks > 0 {
            self.separate_suppressed();
            return Ok(());
        }
        self.mark_loose()?;
        self.flush_inline()?;
        self.open_top_block(start, source)?;
        self.inline = Some(InlineBuf::new(kind));
        Ok(())
    }

    fn start_quote(&mut self, start: usize, source: &str) -> Result<(), EngineError> {
        if self.enter_container() {
            return Ok(());
        }
        self.ensure_content()?;
        self.flush_inline()?;
        self.open_top_block(start, source)?;
        let gap = self.take_gap(false);
        let row = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .width_full()
                        .min_width_0()
                        .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                        .build(),
                )
                .id(QUOTE_ID),
            self.parent(),
        )?;
        self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(QUOTE_BORDER_WIDTH)
                        .height_full()
                        .build(),
                )
                .paint(
                    PaintBuilder::new()
                        .background(self.style.quote_border)
                        .build(),
                )
                .id(QUOTE_BAR_ID),
            row,
        )?;
        let body = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .padding_sides(px(0.0), px(0.0), px(0.0), QUOTE_PADDING)
                        .build(),
                )
                .id(QUOTE_BODY_ID),
            row,
        )?;
        self.push_frame(body, FrameKind::Quote);
        Ok(())
    }

    fn start_list(
        &mut self,
        number: Option<u64>,
        block_start: usize,
        source: &str,
    ) -> Result<(), EngineError> {
        if self.enter_container() {
            return Ok(());
        }
        self.ensure_content()?;
        self.flush_inline()?;
        self.open_top_block(block_start, source)?;
        let gap = self.take_gap(false);
        let list = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .width_full()
                        .min_width_0()
                        .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                        .build(),
                )
                .id(LIST_ID),
            self.parent(),
        )?;
        self.push_frame(
            list,
            FrameKind::List {
                next: number.unwrap_or(1),
                ordered: number.is_some(),
                loose: false,
                last_item_end: None,
                unfilled: Vec::new(),
            },
        );
        Ok(())
    }

    fn start_item(&mut self, start: usize, source: &str) -> Result<(), EngineError> {
        if self.enter_container() {
            return Ok(());
        }
        self.flush_inline()?;
        let mut pending: Vec<NodeId> = Vec::new();
        if let Some(frame) = self.containers.last_mut()
            && let FrameKind::List {
                loose,
                last_item_end,
                unfilled,
                ..
            } = &mut frame.kind
            && let Some(previous) = last_item_end.take()
            && source
                .get(previous..start)
                .is_some_and(|gap| gap.contains('\n'))
        {
            *loose = true;
            pending = std::mem::take(unfilled);
        }
        if !pending.is_empty() {
            self.fill_loose_columns(pending)?;
        }
        let (text, width) = match self.containers.last_mut().map(|frame| &mut frame.kind) {
            Some(FrameKind::List { next, ordered, .. }) => {
                let number = *next;
                *next += 1;
                if *ordered {
                    let width = LIST_MARKER_WIDTH.max(px(8.0 + 7.0 * digits_of(number) as f32));
                    (format_smolstr!("{number}."), width)
                } else {
                    (SmolStr::new_static("•"), LIST_MARKER_WIDTH)
                }
            }
            _ => (SmolStr::new_static("•"), LIST_MARKER_WIDTH),
        };
        let mut row_style = StyleBuilder::new()
            .flex_row()
            .width_full()
            .min_width_0()
            .build();
        row_style.min_height_length(self.style.line_height());
        let row = self.build_node(
            NodeBuilder::boxed().style(row_style).id(LIST_ITEM_ID),
            self.parent(),
        )?;
        self.push_frame(
            row,
            FrameKind::Item(ItemState {
                row,
                loose: false,
                column: false,
                filled: false,
                marker: Some(Marker { text, width }),
                task: None,
                marked: false,
            }),
        );
        Ok(())
    }

    fn fill_loose_columns(&mut self, rows: Vec<NodeId>) -> Result<(), EngineError> {
        for row in rows {
            self.push_frame(
                row,
                FrameKind::Item(ItemState {
                    row,
                    loose: true,
                    column: false,
                    filled: true,
                    marker: None,
                    task: None,
                    marked: true,
                }),
            );
            self.ensure_content()?;
            self.pop_frame();
        }
        Ok(())
    }

    fn start_code_block(&mut self, start: usize, source: &str) -> Result<(), EngineError> {
        if self.skipped_blocks > 0 {
            self.separate_suppressed();
            self.code = Some(String::new());
            return Ok(());
        }
        self.ensure_content()?;
        self.flush_inline()?;
        self.open_top_block(start, source)?;
        self.code = Some(String::new());
        Ok(())
    }

    fn finish_code_block(&mut self, range: &Range<usize>) -> Result<(), EngineError> {
        let Some(code) = self.code.take() else {
            return Ok(());
        };
        if self.skipped_blocks == 0 {
            let text = code.strip_suffix('\n').unwrap_or(&code);
            let gap = self.take_gap(false);
            let style = TextStyle::new(self.style.body_size)
                .with_family(self.style.mono_family.clone())
                .with_color(self.style.code_foreground)
                .with_line_height(self.style.line_height)
                .with_alignment(TextAlign::Left)
                .with_wrap(TextWrap::Wrap);
            let node_style = StyleBuilder::new()
                .width_full()
                .padding(CODE_PADDING)
                .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                .build();
            self.build_node(
                NodeBuilder::text(SmolStr::new(text), style)
                    .style(node_style)
                    .paint(
                        PaintBuilder::new()
                            .background(self.style.code_background)
                            .rounded(CODE_RADIUS)
                            .build(),
                    )
                    .id(CODE_ID),
                self.parent(),
            )?;
        }
        self.close_top_block(range);
        Ok(())
    }

    fn start_html_block(&mut self, start: usize, source: &str) -> Result<(), EngineError> {
        let top_level = self.level_depth() == 0;
        if self.skipped_blocks > 0 {
            self.separate_suppressed();
            self.html = Some(HtmlBuf {
                start,
                top_level,
                payload: String::new(),
            });
            return Ok(());
        }
        self.ensure_content()?;
        self.flush_inline()?;
        self.open_top_block(start, source)?;
        self.html = Some(HtmlBuf {
            start,
            top_level,
            payload: String::new(),
        });
        Ok(())
    }

    fn finish_html_block(&mut self, range: &Range<usize>, source: &str) -> Result<(), EngineError> {
        let Some(html) = self.html.take() else {
            return Ok(());
        };
        let payload = if html.top_level {
            source.get(html.start..range.end).unwrap_or_default()
        } else {
            html.payload.as_str()
        };
        if self.skipped_blocks > 0 {
            let text = html::strip_tags(payload);
            if !text.trim().is_empty() {
                self.separate_suppressed();
                self.suppressed.push_str(&text);
            }
            return Ok(());
        }
        if html.top_level {
            self.emit_html_payload(payload)?;
        } else {
            let text = html::strip_tags(payload);
            let text = text.trim();
            if !text.is_empty() {
                let gap = self.take_gap(false);
                let style =
                    self.block_style(self.style.body_size, false, TextAlign::Left, TextWrap::Wrap);
                self.build_node(
                    NodeBuilder::text(SmolStr::new(text), style).style(
                        StyleBuilder::new()
                            .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                            .build(),
                    ),
                    self.parent(),
                )?;
            }
        }
        self.close_top_block(range);
        Ok(())
    }

    fn emit_html_payload(&mut self, payload: &str) -> Result<(), EngineError> {
        if html::is_tag_block(payload) {
            return Ok(());
        }
        if payload.len() > MAX_HTML_BLOCK_BYTES {
            return self.emit_degraded_fragment(payload);
        }
        let tags = html::block_tags(payload);
        let base_depth = self.depth();
        if tags.is_empty() {
            let text = html::strip_tags(payload);
            if !text.trim().is_empty() {
                let kind = self.default_fragment_kind();
                self.build_fragment(&text, kind, base_depth)?;
            }
            return Ok(());
        }
        if tags.len() > MAX_HTML_BLOCK_TAGS || self.html_tags + tags.len() > MAX_HTML_DOCUMENT_TAGS
        {
            return self.emit_degraded_fragment(payload);
        }
        self.html_tags += tags.len();
        let pairs = html::pairs(&tags);
        let segments = html::segments(payload, &tags);
        let mut scopes: Vec<FragmentKind> = Vec::with_capacity(4);
        let mut outside = OutsideText::default();
        let mut index = 0;
        while index < tags.len() {
            let kind = scopes
                .last()
                .copied()
                .unwrap_or_else(|| self.default_fragment_kind());
            self.collect_html_text(&mut outside, &segments[index], payload, kind);
            match tags[index] {
                BlockTag::Rule { .. } => {
                    self.build_html_rule()?;
                    index += 1;
                }
                BlockTag::Close { .. } => {
                    scopes.pop();
                    self.align_windows.pop();
                    index += 1;
                }
                BlockTag::Open { kind, end, .. } => {
                    if let Some(close) = pairs[index] {
                        if let Some(content) = payload.get(end..tags[close].at()) {
                            self.build_fragment(content, kind, base_depth)?;
                        }
                        index = close + 1;
                        continue;
                    }
                    self.emit_outside_text(&mut outside, payload, base_depth)?;
                    if self.fragment.is_none() {
                        self.align_windows.push(kind.align());
                    }
                    scopes.push(match kind {
                        FragmentKind::Heading(_) => FragmentKind::Paragraph(None),
                        kind => kind,
                    });
                    index += 1;
                }
            }
        }
        let kind = scopes
            .last()
            .copied()
            .unwrap_or_else(|| self.default_fragment_kind());
        self.collect_html_text(&mut outside, &segments[tags.len()], payload, kind);
        self.emit_outside_text(&mut outside, payload, base_depth)?;
        Ok(())
    }

    fn default_fragment_kind(&self) -> FragmentKind {
        self.fragment
            .unwrap_or_else(|| FragmentKind::Paragraph(self.current_align()))
    }

    fn collect_html_text(
        &self,
        outside: &mut OutsideText,
        range: &Range<usize>,
        payload: &str,
        kind: FragmentKind,
    ) {
        let Some(segment) = payload.get(range.clone()) else {
            return;
        };
        if segment.trim().is_empty() {
            return;
        }
        if outside.kind.is_none() {
            outside.kind = Some(kind);
        }
        outside.ranges.push(range.clone());
    }

    fn emit_outside_text(
        &mut self,
        outside: &mut OutsideText,
        payload: &str,
        base_depth: usize,
    ) -> Result<(), EngineError> {
        let ranges = std::mem::take(&mut outside.ranges);
        let kind = outside.kind.take().unwrap_or(FragmentKind::Paragraph(None));
        if ranges.is_empty() {
            return Ok(());
        }
        let capacity = ranges.iter().map(|range| range.len() + 2).sum();
        let mut text = String::with_capacity(capacity);
        for range in ranges {
            let piece = html::text_region(payload.get(range).unwrap_or_default());
            if piece.trim().is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&piece);
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        self.build_fragment(&text, kind, base_depth)
    }

    fn build_html_rule(&mut self) -> Result<(), EngineError> {
        let gap = self.take_gap(false);
        self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .height(px(1.0))
                        .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                        .build(),
                )
                .paint(PaintBuilder::new().background(self.style.divider).build())
                .id(RULE_ID),
            self.parent(),
        )?;
        Ok(())
    }

    fn build_fragment(
        &mut self,
        source: &str,
        kind: FragmentKind,
        base_depth: usize,
    ) -> Result<(), EngineError> {
        if !self.budget.admit(source.len()) {
            return self.emit_degraded_fragment(source);
        }
        let saved = self.save_fragment_state();
        self.budget.charge(source.len());
        self.lines = SourceLines::new(source);
        self.base_depth = base_depth;
        self.parse_depth += 1;
        self.fragment = Some(kind);
        self.parse(source)?;
        self.flush_inline()?;
        self.restore_fragment_state(saved);
        Ok(())
    }

    fn emit_degraded_fragment(&mut self, source: &str) -> Result<(), EngineError> {
        if html::strip_tags(source).trim().is_empty() {
            return Ok(());
        }
        let text = plain::plain_text(source);
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let gap = self.take_gap(false);
        let style = self.block_style(self.style.body_size, false, TextAlign::Left, TextWrap::Wrap);
        self.build_node(
            NodeBuilder::text(SmolStr::new(text), style).style(
                StyleBuilder::new()
                    .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                    .build(),
            ),
            self.parent(),
        )?;
        Ok(())
    }

    fn save_fragment_state(&mut self) -> FragmentState {
        FragmentState {
            inline: self.inline.take(),
            cell: self.cell.take(),
            marks: std::mem::take(&mut self.marks),
            clipped_marks: std::mem::take(&mut self.clipped_marks),
            skipped_blocks: std::mem::take(&mut self.skipped_blocks),
            suppressed: std::mem::take(&mut self.suppressed),
            lines: std::mem::take(&mut self.lines),
            last_block: self.last_block.take(),
            open_block: self.open_block.take(),
            code: self.code.take(),
            html: self.html.take(),
            image: self.image.take(),
            table: self.table.take(),
            align_windows: std::mem::take(&mut self.align_windows),
            pending_window_closes: std::mem::take(&mut self.pending_window_closes),
            fragment: self.fragment.take(),
            base_depth: self.base_depth,
            parse_depth: self.parse_depth,
        }
    }

    fn restore_fragment_state(&mut self, saved: FragmentState) {
        self.inline = saved.inline;
        self.cell = saved.cell;
        self.marks = saved.marks;
        self.clipped_marks = saved.clipped_marks;
        self.skipped_blocks = saved.skipped_blocks;
        self.suppressed = saved.suppressed;
        self.lines = saved.lines;
        self.last_block = saved.last_block;
        self.open_block = saved.open_block;
        self.code = saved.code;
        self.html = saved.html;
        self.image = saved.image;
        self.table = saved.table;
        self.align_windows = saved.align_windows;
        self.pending_window_closes = saved.pending_window_closes;
        self.fragment = saved.fragment;
        self.base_depth = saved.base_depth;
        self.parse_depth = saved.parse_depth;
    }

    fn start_table(
        &mut self,
        alignments: Vec<Alignment>,
        start: usize,
        source: &str,
    ) -> Result<(), EngineError> {
        if self.enter_container() {
            return Ok(());
        }
        self.ensure_content()?;
        self.flush_inline()?;
        self.open_top_block(start, source)?;
        self.table = Some(TableBuf::new(alignments));
        Ok(())
    }

    fn begin_table_row(&mut self, head: bool) -> Result<(), EngineError> {
        self.flush_inline()?;
        if let Some(table) = self.table.as_mut() {
            table.begin_row(head);
        }
        Ok(())
    }

    fn start_cell(&mut self) -> Result<(), EngineError> {
        self.flush_inline()?;
        let Some(table) = self.table.as_ref() else {
            return Ok(());
        };
        let align = table.align(table.row_len());
        let header = table.in_head();
        self.cell = Some(InlineBuf::new(InlineKind::Cell { align, header }));
        Ok(())
    }

    fn finish_cell(&mut self) -> Result<(), EngineError> {
        let Some(cell) = self.cell.take() else {
            return Ok(());
        };
        let base = self.base_style(cell.kind);
        let content = cell.finish(&base, self.style).unwrap_or_default();
        let Some(table) = self.table.as_mut() else {
            return Ok(());
        };
        table.push_cell(CellBuf::new(base, content));
        Ok(())
    }

    fn finish_row(&mut self) -> Result<(), EngineError> {
        let Some(head) = self.table.as_ref().map(TableBuf::in_head) else {
            return Ok(());
        };
        let padding = self.base_style(InlineKind::Cell {
            align: Alignment::None,
            header: head,
        });
        if let Some(table) = self.table.as_mut() {
            table.finish_row(padding);
        }
        Ok(())
    }

    fn finish_table(&mut self, range: &Range<usize>) -> Result<(), EngineError> {
        self.flush_inline()?;
        self.close_alignment_windows();
        self.leave_container()?;
        let Some(buf) = self.table.take() else {
            return Ok(());
        };
        let widths = buf.column_widths(self.app);
        let gap = self.take_gap(false);
        let parent = self.parent();
        let table = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .width_full()
                        .min_width_0()
                        .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                        .build(),
                )
                .id(TABLE_ID),
            parent,
        )?;
        let header = buf.header;
        let rows = buf.rows;
        let overflow = buf.overflow;
        if let Some(header) = header {
            self.build_table_row(table, header, widths.as_deref())?;
        }
        for row in rows {
            self.build_table_row(table, row, widths.as_deref())?;
        }
        for line in overflow {
            if line.is_empty() {
                continue;
            }
            let overflow_gap = self.take_gap(false);
            let style =
                self.block_style(self.style.body_size, false, TextAlign::Left, TextWrap::Wrap);
            self.build_node(
                NodeBuilder::text(line, style).style(
                    StyleBuilder::new()
                        .margin_sides(overflow_gap, px(0.0), px(0.0), px(0.0))
                        .build(),
                ),
                parent,
            )?;
        }
        self.close_top_block(range);
        Ok(())
    }

    fn build_table_row(
        &mut self,
        table: NodeId,
        row: Vec<CellBuf>,
        widths: Option<&[LogicalPx]>,
    ) -> Result<(), EngineError> {
        let row_node = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .width_full()
                        .min_width_0()
                        .build(),
                )
                .id(TABLE_ROW_ID),
            table,
        )?;
        for (column, cell) in row.into_iter().enumerate() {
            let width = widths.and_then(|widths| widths.get(column).copied());
            let mut style = StyleBuilder::new()
                .min_width_0()
                .padding(CELL_PADDING)
                .overflow_hidden();
            style = match width {
                Some(width) => style.width(width),
                None => style.flex_1(),
            };
            let cell_node = self.build_node(
                NodeBuilder::boxed().style(style.build()).id(TABLE_CELL_ID),
                row_node,
            )?;
            if !cell.is_empty() {
                self.create_text_node(cell_node, &cell.base, cell.content, LogicalPx::ZERO)?;
            }
        }
        self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .height(TABLE_RULE_HEIGHT)
                        .build(),
                )
                .paint(PaintBuilder::new().background(self.style.divider).build())
                .id(TABLE_RULE_ID),
            table,
        )?;
        Ok(())
    }

    fn start_mark(&mut self, mark: Mark) {
        if self.clipped_marks > 0 || self.marks.len() >= MAX_INLINE_NESTING {
            self.clipped_marks += 1;
            return;
        }
        self.marks.push(mark);
    }

    fn end_mark(&mut self, mark: Mark) {
        if self.clipped_marks > 0 {
            self.clipped_marks -= 1;
            return;
        }
        Mark::pop(&mut self.marks, mark);
    }

    fn start_image(&mut self, dest: &str) {
        if self.skipped_blocks > 0 {
            return;
        }
        self.image = Some(ImageBuf {
            dest: SmolStr::new(dest),
            alt: String::new(),
            depth: 1,
        });
    }

    fn image_event(&mut self, event: Event<'_>) -> Result<(), EngineError> {
        match event {
            Event::Start(Tag::Image { .. }) => {
                if let Some(image) = self.image.as_mut() {
                    image.depth += 1;
                }
                Ok(())
            }
            Event::End(TagEnd::Image) => {
                let depth = self.image.as_mut().map(|image| {
                    image.depth -= 1;
                    image.depth
                });
                if depth == Some(0) {
                    self.finish_image()?;
                }
                Ok(())
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(image) = self.image.as_mut() {
                    image.alt.push_str(&text);
                }
                Ok(())
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(image) = self.image.as_mut() {
                    image.alt.push(' ');
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn finish_image(&mut self) -> Result<(), EngineError> {
        let Some(image) = self.image.take() else {
            return Ok(());
        };
        self.push_image(&image.dest, &image.alt);
        Ok(())
    }

    fn push_image(&mut self, dest: &str, alt: &str) {
        let size = InlineImageSource::parse(dest).map(|source| source.resolve(self.assets));
        match (size, alt.trim().is_empty()) {
            (Some(size), _) => self.push_box(size),
            (None, false) => {
                let marks = Marks::of(&self.marks);
                self.write_inline(alt, marks);
            }
            (None, true) => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        let marks = Marks::of(&self.marks);
        self.write_inline(text, marks);
    }

    fn push_code(&mut self, text: &str) {
        let mut marks = Marks::of(&self.marks);
        marks.code = true;
        self.write_inline(text, marks);
    }

    fn write_inline(&mut self, text: &str, marks: Marks) {
        if text.is_empty() {
            return;
        }
        if self.skipped_blocks > 0 {
            self.suppressed.push_str(text);
            return;
        }
        if let Some(cell) = self.cell.as_mut() {
            cell.push_text(text, marks);
            return;
        }
        self.inline_mut().push_text(text, marks);
    }

    fn push_break(&mut self) -> Result<(), EngineError> {
        if self.skipped_blocks > 0 {
            self.suppressed.push('\n');
            return Ok(());
        }
        let marks = Marks::of(&self.marks);
        if let Some(cell) = self.cell.as_mut() {
            cell.push_break(marks);
            return Ok(());
        }
        self.inline_mut().push_break(marks);
        Ok(())
    }

    fn push_box(&mut self, size: LogicalSize) {
        if let Some(cell) = self.cell.as_mut() {
            cell.push_box(size);
            return;
        }
        self.inline_mut().push_box(size);
    }

    fn inline_mut(&mut self) -> &mut InlineBuf {
        self.inline
            .get_or_insert_with(|| InlineBuf::new(InlineKind::Tight))
    }

    fn push_inline_html(&mut self, tag: &str) -> Result<(), EngineError> {
        match html::inline_tag(tag) {
            InlineTag::Open(mark) => self.start_mark(mark),
            InlineTag::Close(mark) => self.end_mark(mark),
            InlineTag::Break => return self.push_break(),
            InlineTag::Image { src, alt } => {
                self.push_image(
                    src.as_deref().unwrap_or_default(),
                    alt.as_deref().unwrap_or_default(),
                );
            }
            InlineTag::ParagraphClose => {
                if self.level_depth() == 0 && !self.align_windows.is_empty() {
                    self.pending_window_closes += 1;
                }
            }
            InlineTag::Transparent | InlineTag::Other => {}
        }
        Ok(())
    }

    fn set_task(&mut self, checked: bool) {
        if self.skipped_blocks > 0 {
            return;
        }
        if let Some(frame) = self.containers.last_mut()
            && let FrameKind::Item(item) = &mut frame.kind
            && !item.marked
        {
            item.task = Some(checked);
        }
    }

    fn push_rule(&mut self, range: &Range<usize>, source: &str) -> Result<(), EngineError> {
        if self.skipped_blocks > 0 {
            self.separate_suppressed();
            return Ok(());
        }
        self.ensure_content()?;
        self.flush_inline()?;
        self.open_top_block(range.start, source)?;
        self.build_html_rule()?;
        self.close_top_block(range);
        Ok(())
    }

    fn flush_inline(&mut self) -> Result<(), EngineError> {
        self.marks.clear();
        self.clipped_marks = 0;
        let Some(inline) = self.inline.take() else {
            return Ok(());
        };
        if let Some(cell) = self.cell.as_mut() {
            cell.extend(inline);
            return Ok(());
        }
        let kind = inline.kind;
        let base = self.base_style(kind);
        let Some(content) = inline.finish(&base, self.style) else {
            return Ok(());
        };
        if let Some(row) = self.tight_target() {
            self.ensure_marker()?;
            let style = StyleBuilder::new().flex_1().min_width_0().build();
            self.create_styled_text_node(row, &base, content, style)?;
            if let Some(FrameKind::Item(item)) =
                self.containers.last_mut().map(|frame| &mut frame.kind)
            {
                item.filled = true;
            }
            return Ok(());
        }
        self.ensure_content()?;
        let gap = self.take_gap(matches!(kind, InlineKind::Heading(_)));
        self.create_text_node(self.parent(), &base, content, gap)?;
        Ok(())
    }

    fn create_text_node(
        &mut self,
        parent: NodeId,
        base: &TextStyle,
        content: InlineContent,
        gap: LogicalPx,
    ) -> Result<NodeId, EngineError> {
        let style = StyleBuilder::new()
            .margin_sides(gap, px(0.0), px(0.0), px(0.0))
            .build();
        self.create_styled_text_node(parent, base, content, style)
    }

    fn create_styled_text_node(
        &mut self,
        parent: NodeId,
        base: &TextStyle,
        content: InlineContent,
        style: Style,
    ) -> Result<NodeId, EngineError> {
        let mut builder = NodeBuilder::text(content.text, base.clone()).style(style);
        if !content.runs.is_empty() {
            builder = builder.runs(content.runs);
        }
        if !content.boxes.is_empty() {
            builder = builder.inline_boxes(content.boxes);
        }
        self.build_node(builder, parent)
    }

    fn base_style(&self, kind: InlineKind) -> TextStyle {
        let (size, bold) = match kind {
            InlineKind::Heading(level) => (self.style.body_size * heading_scale(level), true),
            InlineKind::Cell { header, .. } => (self.style.body_size, header),
            InlineKind::Paragraph(_) | InlineKind::Tight => (self.style.body_size, false),
        };
        let wrap = match kind {
            InlineKind::Cell { .. } => TextWrap::BreakWord,
            _ => TextWrap::Wrap,
        };
        self.block_style(size, bold, kind.text_align(), wrap)
    }

    fn block_style(
        &self,
        size: LogicalPx,
        bold: bool,
        align: TextAlign,
        wrap: TextWrap,
    ) -> TextStyle {
        TextStyle::new(size)
            .with_color(self.style.foreground)
            .with_weight(if bold { 700 } else { 400 })
            .with_line_height(self.style.line_height)
            .with_alignment(align)
            .with_wrap(wrap)
    }

    fn paragraph_kind(&self) -> InlineKind {
        if self.level_depth() != 0 {
            return InlineKind::Paragraph(None);
        }
        match self.fragment {
            Some(FragmentKind::Paragraph(align)) => InlineKind::Paragraph(align),
            Some(FragmentKind::Heading(level)) => InlineKind::Heading(level),
            None => InlineKind::Paragraph(self.align_windows.last().copied()),
        }
    }

    fn current_align(&self) -> Option<Alignment> {
        if self.level_depth() == 0 {
            self.align_windows.last().copied()
        } else {
            None
        }
    }

    fn depth(&self) -> usize {
        self.containers.len() - 1
    }

    fn level_depth(&self) -> usize {
        self.depth() - self.base_depth
    }

    fn parent(&self) -> NodeId {
        self.containers.last().expect("markdown root frame").node
    }

    fn tight_target(&self) -> Option<NodeId> {
        match self.containers.last().map(|frame| &frame.kind) {
            Some(FrameKind::Item(item)) if !item.loose && !item.column => Some(item.row),
            _ => None,
        }
    }

    fn mark_loose(&mut self) -> Result<(), EngineError> {
        let count = self.containers.len();
        if count >= 1
            && let FrameKind::Item(item) = &mut self.containers[count - 1].kind
        {
            item.loose = true;
        }
        let mut pending: Vec<NodeId> = Vec::new();
        if count >= 2
            && let FrameKind::List {
                loose, unfilled, ..
            } = &mut self.containers[count - 2].kind
        {
            *loose = true;
            pending = std::mem::take(unfilled);
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.fill_loose_columns(pending)
    }

    fn ensure_marker(&mut self) -> Result<(), EngineError> {
        let Some(frame) = self.containers.last_mut() else {
            return Ok(());
        };
        let FrameKind::Item(item) = &mut frame.kind else {
            return Ok(());
        };
        if item.marked {
            return Ok(());
        }
        item.marked = true;
        let row = item.row;
        let task = item.task;
        let marker = item.marker.take();
        match (task, marker) {
            (Some(checked), _) => self.build_task_marker(row, checked),
            (None, Some(marker)) => {
                let style = TextStyle::new(self.style.body_size)
                    .with_color(self.style.muted)
                    .with_alignment(TextAlign::Right)
                    .with_wrap(TextWrap::NoWrap);
                self.build_node(
                    NodeBuilder::text(marker.text, style)
                        .style(StyleBuilder::new().width(marker.width).build())
                        .id(LIST_MARKER_ID),
                    row,
                )?;
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    fn build_task_marker(&mut self, row: NodeId, checked: bool) -> Result<(), EngineError> {
        let mut paint = PaintBuilder::new()
            .border(TASK_BORDER_WIDTH, self.style.task_border)
            .rounded(TASK_RADIUS);
        if checked {
            paint = paint.background(self.style.task_fill);
        }
        let marker = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .size(TASK_SIZE, TASK_SIZE)
                        .items_center()
                        .justify_center()
                        .build(),
                )
                .paint(paint.build())
                .id(LIST_MARKER_ID),
            row,
        )?;
        if checked {
            let style = TextStyle::new(TASK_CHECK_SIZE)
                .with_family(self.style.mono_family.clone())
                .with_color(self.style.foreground)
                .with_alignment(TextAlign::Left)
                .with_wrap(TextWrap::NoWrap);
            self.build_node(NodeBuilder::text(SmolStr::new_static("✓"), style), marker)?;
        }
        Ok(())
    }

    fn ensure_content(&mut self) -> Result<(), EngineError> {
        let Some(FrameKind::Item(item)) = self.containers.last().map(|frame| &frame.kind) else {
            return Ok(());
        };
        if item.column {
            return Ok(());
        }
        let row = item.row;
        let id = if self.current_item_is_loose() {
            LIST_CONTENT_ID
        } else {
            LIST_CONTENT_TIGHT_ID
        };
        self.ensure_marker()?;
        let column = self.build_node(
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .build(),
                )
                .id(id),
            row,
        )?;
        if let Some(frame) = self.containers.last_mut()
            && let FrameKind::Item(item) = &mut frame.kind
        {
            item.column = true;
            frame.node = column;
        }
        Ok(())
    }

    fn current_item_is_loose(&self) -> bool {
        let count = self.containers.len();
        let Some(FrameKind::Item(item)) = self.containers.last().map(|frame| &frame.kind) else {
            return false;
        };
        item.loose
            || (count >= 2
                && matches!(
                    &self.containers[count - 2].kind,
                    FrameKind::List { loose: true, .. }
                ))
    }

    fn item_needs_content_column(&self) -> bool {
        let count = self.containers.len();
        let Some(FrameKind::Item(item)) = self.containers.last().map(|frame| &frame.kind) else {
            return false;
        };
        if item.column {
            return false;
        }
        item.loose
            || (count >= 2
                && matches!(
                    &self.containers[count - 2].kind,
                    FrameKind::List { loose: true, .. }
                ))
    }

    fn push_frame(&mut self, node: NodeId, kind: FrameKind) {
        self.containers.push(Frame {
            node,
            kind,
            block_index: 0,
            previous_blank: false,
        });
    }

    fn pop_frame(&mut self) {
        if self.containers.len() > 1 {
            self.containers.pop();
        }
    }

    fn enter_container(&mut self) -> bool {
        if self.skipped_blocks > 0 || self.depth() + self.parse_depth >= MAX_BLOCK_NESTING {
            self.skipped_blocks += 1;
            self.separate_suppressed();
            return true;
        }
        false
    }

    fn leave_container(&mut self) -> Result<(), EngineError> {
        if self.skipped_blocks == 0 {
            return Ok(());
        }
        self.skipped_blocks -= 1;
        if self.skipped_blocks == 0 {
            self.flush_suppressed()?;
        }
        Ok(())
    }

    fn separate_suppressed(&mut self) {
        if !self.suppressed.is_empty() && !self.suppressed.ends_with('\n') {
            self.suppressed.push('\n');
        }
    }

    fn flush_suppressed(&mut self) -> Result<(), EngineError> {
        if self.suppressed.is_empty() {
            return Ok(());
        }
        let text = self.suppressed.trim_end().to_owned();
        self.suppressed.clear();
        if text.is_empty() {
            return Ok(());
        }
        let gap = self.take_gap(false);
        let style = self.block_style(self.style.body_size, false, TextAlign::Left, TextWrap::Wrap);
        self.build_node(
            NodeBuilder::text(SmolStr::new(text), style).style(
                StyleBuilder::new()
                    .margin_sides(gap, px(0.0), px(0.0), px(0.0))
                    .build(),
            ),
            self.parent(),
        )?;
        Ok(())
    }

    fn take_gap(&mut self, heading: bool) -> LogicalPx {
        let Some(frame) = self.containers.last_mut() else {
            return LogicalPx::ZERO;
        };
        let mut gap = if frame.block_index > 0 && !frame.previous_blank {
            BLOCK_GAP
        } else {
            LogicalPx::ZERO
        };
        if heading && frame.block_index > 0 {
            gap += HEADING_SPACE_ABOVE;
        }
        frame.block_index += 1;
        frame.previous_blank = false;
        gap
    }

    fn open_top_block(&mut self, start: usize, source: &str) -> Result<(), EngineError> {
        if self.level_depth() != 0 {
            return Ok(());
        }
        if self.fragment.is_none() {
            self.emit_blank_spacers(source, start)?;
        }
        self.open_block = Some(start);
        Ok(())
    }

    fn close_top_block(&mut self, range: &Range<usize>) {
        if self.level_depth() != 0 {
            return;
        }
        if let Some(start) = self.open_block.take() {
            self.last_block = Some(start..range.end);
        }
    }

    fn emit_blank_spacers(&mut self, source: &str, next_start: usize) -> Result<(), EngineError> {
        let Some(previous) = self.last_block.take() else {
            return Ok(());
        };
        let count = self.lines.blank_lines_between(source, previous, next_start);
        let height = self.style.line_height();
        let parent = self.parent();
        for _ in 0..count {
            let mut style = StyleBuilder::new().width_full().height(height).build();
            style.flex_shrink(0.0);
            self.build_node(NodeBuilder::boxed().style(style).id(SPACER_ID), parent)?;
            if let Some(frame) = self.containers.last_mut() {
                frame.previous_blank = true;
            }
        }
        Ok(())
    }

    fn build_node(&mut self, builder: NodeBuilder, parent: NodeId) -> Result<NodeId, EngineError> {
        let window = self.window;
        let tree = self
            .app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        builder.build(tree, parent)
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.75,
        2 => 1.5,
        3 => 1.3,
        4 => 1.15,
        5 => 1.05,
        _ => 1.0,
    }
}

fn digits_of(number: u64) -> u32 {
    if number == 0 {
        return 1;
    }
    let mut digits = 0;
    let mut value = number;
    while value > 0 {
        digits += 1;
        value /= 10;
    }
    digits
}

#[cfg(test)]
std::thread_local! {
    static PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn parse_count() -> usize {
    PARSE_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(super) fn reset_parse_count() {
    PARSE_COUNT.with(|count| count.set(0));
}
