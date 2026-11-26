use std::ops::Range;

use pulldown_cmark::Alignment;
use smol_str::SmolStr;
use ui_core::{
    math::{LogicalSize, px},
    text::{
        InlineAlign, InlineBoxPaint, InlineBoxSpec, TextAlign, TextBackground, TextRun, TextSlant,
        TextStyle,
    },
};

use super::MarkdownStyle;

pub(super) const MONO_FAMILY: &str = "Consolas";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Marks {
    pub strong: bool,
    pub emphasis: bool,
    pub strike: bool,
    pub code: bool,
    pub kbd: bool,
    pub link: bool,
}

impl Marks {
    pub(super) fn of(stack: &[Mark]) -> Self {
        let mut marks = Self::default();
        for mark in stack {
            match mark {
                Mark::Strong => marks.strong = true,
                Mark::Emphasis => marks.emphasis = true,
                Mark::Strike => marks.strike = true,
                Mark::Code => marks.code = true,
                Mark::Kbd => marks.kbd = true,
                Mark::Link => marks.link = true,
            }
        }
        marks
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mark {
    Strong,
    Emphasis,
    Strike,
    Code,
    Kbd,
    Link,
}

impl Mark {
    pub(super) fn pop(stack: &mut Vec<Mark>, mark: Mark) {
        if let Some(index) = stack.iter().rposition(|candidate| *candidate == mark) {
            stack.remove(index);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum InlineKind {
    Paragraph(Option<Alignment>),
    Heading(u8),
    Tight,
    Cell { align: Alignment, header: bool },
}

impl InlineKind {
    pub(super) fn text_align(self) -> TextAlign {
        match self {
            InlineKind::Paragraph(align) => align.map_or(TextAlign::Left, cell_align),
            InlineKind::Cell { align, .. } => cell_align(align),
            InlineKind::Heading(_) | InlineKind::Tight => TextAlign::Left,
        }
    }
}

fn cell_align(align: Alignment) -> TextAlign {
    match align {
        Alignment::Center => TextAlign::Center,
        Alignment::Right => TextAlign::Right,
        Alignment::None | Alignment::Left => TextAlign::Left,
    }
}

#[derive(Clone, Copy, Debug)]
struct InlineBox {
    index: usize,
    size: LogicalSize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct InlineContent {
    pub text: SmolStr,
    pub runs: Vec<TextRun>,
    pub boxes: Vec<InlineBoxSpec>,
}

impl InlineContent {
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty() && self.boxes.is_empty()
    }
}

#[derive(Clone, Debug)]
pub(super) struct InlineBuf {
    text: String,
    spans: Vec<(Range<usize>, Marks)>,
    boxes: Vec<InlineBox>,
    kind: InlineKind,
}

impl InlineBuf {
    pub(super) fn new(kind: InlineKind) -> Self {
        Self {
            text: String::with_capacity(32),
            spans: Vec::with_capacity(2),
            boxes: Vec::new(),
            kind,
        }
    }

    pub(super) fn kind(&self) -> InlineKind {
        self.kind
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty() && self.boxes.is_empty()
    }

    pub(super) fn extend(&mut self, other: InlineBuf) {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        for (range, marks) in other.spans {
            self.push_span(range.start + offset..range.end + offset, marks);
        }
        self.boxes
            .extend(other.boxes.into_iter().map(|mut inline_box| {
                inline_box.index += offset;
                inline_box
            }));
    }

    pub(super) fn push_text(&mut self, text: &str, marks: Marks) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        self.push_span(start..end, marks);
    }

    pub(super) fn push_break(&mut self, marks: Marks) {
        let start = self.text.len();
        self.text.push('\n');
        let end = self.text.len();
        self.push_span(start..end, marks);
    }

    pub(super) fn push_box(&mut self, size: LogicalSize) {
        self.boxes.push(InlineBox {
            index: self.text.len(),
            size,
        });
    }

    fn push_span(&mut self, range: Range<usize>, marks: Marks) {
        if let Some((previous, previous_marks)) = self.spans.last_mut()
            && *previous_marks == marks
        {
            previous.end = range.end;
            return;
        }
        self.spans.push((range, marks));
    }

    pub(super) fn finish(self, base: &TextStyle, style: &MarkdownStyle) -> Option<InlineContent> {
        if self.is_empty() {
            return None;
        }
        let text = if self.text.is_empty() {
            SmolStr::new_static("\u{200B}")
        } else {
            SmolStr::new(&self.text)
        };
        let mut runs: Vec<TextRun> = Vec::with_capacity(self.spans.len());
        for (range, marks) in self.spans {
            let run_style = run_style(base, marks, style);
            if run_style == *base {
                continue;
            }
            if let Some(previous) = runs.last_mut()
                && previous.style == run_style
                && previous.range.end == range.start
            {
                previous.range.end = range.end;
                continue;
            }
            runs.push(TextRun::new(range, run_style));
        }
        let boxes = self
            .boxes
            .into_iter()
            .enumerate()
            .map(|(id, inline_box)| InlineBoxSpec {
                id: id as u64,
                index: inline_box.index,
                size: inline_box.size,
                baseline: None,
                align: InlineAlign::Baseline,
                paint: Some(InlineBoxPaint::Quad {
                    color: style.image_placeholder,
                    corner_radius: [px(2.0); 4],
                }),
            })
            .collect();
        Some(InlineContent { text, runs, boxes })
    }
}

fn run_style(base: &TextStyle, marks: Marks, style: &MarkdownStyle) -> TextStyle {
    let mono = marks.code || marks.kbd;
    let mut run = base.clone();
    if marks.strong {
        run.weight = 700;
    }
    if marks.emphasis {
        run.slant = TextSlant::Italic;
    }
    if mono {
        run = run.with_family(SmolStr::new_static(MONO_FAMILY));
    }
    if marks.link {
        run = run.with_color(style.link);
    } else if mono {
        run = run.with_color(style.code_foreground);
    }
    if marks.link {
        run = run.with_underline(style.decoration_thickness, Some(style.link));
    }
    if marks.kbd {
        run = run.with_underline(style.decoration_thickness, Some(style.code_foreground));
    }
    if marks.strike {
        run = run.with_strikethrough(style.decoration_thickness, Some(style.foreground));
    }
    if mono {
        run = run.with_background(TextBackground {
            color: style.code_background,
            corner_radius: style.background_radius,
            padding_x: style.background_padding_x,
        });
    }
    run
}
