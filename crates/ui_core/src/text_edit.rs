use std::{
    ops::Range,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use parley::{
    Affinity as ParleyAffinity, BreakReason as ParleyBreakReason, Cursor as ParleyCursor, Layout,
    Selection as ParleySelection,
};
use smallvec::SmallVec;
use smol_str::{SmolStr, SmolStrBuilder};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

use crate::{
    a11y::{A11yPatch, A11yRole, textrun},
    app::{App, TimerId},
    error::EngineError,
    event::{Action, ActionSource, ImeEvent, KeyCode, Modifiers},
    interaction::{ScrollAxis, drag as drag_util, scroll},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{id::NodeId, tree::NodeTree},
    paint::color::{Color, ColorBrush},
    text::{ShapedLine, TextContent, TextStyle, shape::ShapedText},
    window::{Window, WindowId},
};

const MASK_GLYPH: char = '\u{2022}';

const CARET_BLINK_HALF: Duration = Duration::from_millis(530);

const CARET_REVEAL_MARGIN_X: f32 = 4.0;

fn masked_text(raw: &str) -> SmolStr {
    let mut text = SmolStrBuilder::with_capacity(raw.len());
    for _ in raw.graphemes(true) {
        text.push(MASK_GLYPH);
    }
    text.finish()
}

const UNDO_LIMIT: usize = 100;

pub(crate) struct CaretBlink {
    last_activity: Instant,
    visible: bool,
    timer: Option<TimerId>,
    focused: Option<NodeId>,
    os_focused: bool,
}

impl CaretBlink {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last_activity: now,
            visible: true,
            timer: None,
            focused: None,
            os_focused: true,
        }
    }

    #[must_use]
    pub(crate) fn owner(&self) -> Option<NodeId> {
        self.focused
    }

    pub(crate) fn release_owner(&mut self) {
        self.focused = None;
        self.visible = true;
        self.timer = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RevealKey {
    ancestor: Option<NodeId>,
    caret: LogicalRect,
    viewport: LogicalSize,
    limit: LogicalPoint,
}

struct RevealGeometry {
    key: RevealKey,
    ancestor: Option<(NodeId, crate::interaction::ScrollSlot)>,
    focus: LogicalRect,
    anchor: Option<LogicalRect>,
    viewport: LogicalRect,
    multiline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextAffinity {
    Upstream,
    Downstream,
}

impl TextAffinity {
    fn from_parley(affinity: ParleyAffinity) -> Self {
        match affinity {
            ParleyAffinity::Upstream => Self::Upstream,
            ParleyAffinity::Downstream => Self::Downstream,
        }
    }

    fn to_parley(self) -> ParleyAffinity {
        match self {
            Self::Upstream => ParleyAffinity::Upstream,
            Self::Downstream => ParleyAffinity::Downstream,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextCursor {
    pub byte: usize,
    pub affinity: TextAffinity,
}

impl TextCursor {
    fn at(byte: usize, display_len: usize) -> Self {
        let byte = byte.min(display_len);
        let affinity = if display_len == 0 || byte < display_len {
            TextAffinity::Downstream
        } else {
            TextAffinity::Upstream
        };
        Self { byte, affinity }
    }

    fn from_parley(cursor: ParleyCursor, display_len: usize) -> Self {
        let byte = cursor.index().min(display_len);
        let affinity = if display_len == 0 {
            TextAffinity::Downstream
        } else if byte == display_len {
            TextAffinity::Upstream
        } else {
            TextAffinity::from_parley(cursor.affinity())
        };
        Self { byte, affinity }
    }

    fn to_parley(self, layout: &Layout<ColorBrush>) -> ParleyCursor {
        ParleyCursor::from_byte_index(layout, self.byte, self.affinity.to_parley())
    }

    fn normalized(self, display_len: usize) -> Self {
        if display_len == 0 {
            return Self {
                byte: 0,
                affinity: TextAffinity::Downstream,
            };
        }
        let byte = self.byte.min(display_len);
        if byte == display_len {
            return Self {
                byte,
                affinity: TextAffinity::Upstream,
            };
        }
        if self.byte > display_len {
            return Self {
                byte,
                affinity: TextAffinity::Downstream,
            };
        }
        Self {
            byte,
            affinity: self.affinity,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionBase {
    Cluster,
    Word { start: TextCursor, end: TextCursor },
    Line { start: TextCursor, end: TextCursor },
}

#[derive(Clone, Copy)]
pub struct TextSelection {
    anchor: TextCursor,
    focus: TextCursor,
    h_pos: Option<f32>,
    base: SelectionBase,
}

impl std::fmt::Debug for TextSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSelection")
            .field("anchor", &self.anchor)
            .field("focus", &self.focus)
            .field("h_pos", &self.h_pos)
            .finish()
    }
}

impl PartialEq for TextSelection {
    fn eq(&self, other: &Self) -> bool {
        self.anchor == other.anchor && self.focus == other.focus
    }
}

impl Eq for TextSelection {}

impl TextSelection {
    fn collapsed(cursor: TextCursor) -> Self {
        Self {
            anchor: cursor,
            focus: cursor,
            h_pos: None,
            base: SelectionBase::Cluster,
        }
    }

    fn ranged(anchor: TextCursor, focus: TextCursor) -> Self {
        Self {
            anchor,
            focus,
            h_pos: None,
            base: SelectionBase::Cluster,
        }
    }

    fn ranged_with_h_pos(anchor: TextCursor, focus: TextCursor, h_pos: Option<f32>) -> Self {
        Self {
            anchor,
            focus,
            h_pos,
            base: SelectionBase::Cluster,
        }
    }

    fn from_parley(selection: ParleySelection, display_len: usize) -> Self {
        Self {
            anchor: TextCursor::from_parley(selection.anchor(), display_len),
            focus: TextCursor::from_parley(selection.focus(), display_len),
            h_pos: None,
            base: SelectionBase::Cluster,
        }
    }

    fn word_from_point(layout: &Layout<ColorBrush>, x: f32, y: f32, display_len: usize) -> Self {
        let selection = ParleySelection::word_from_point(layout, x, y);
        let anchor = TextCursor::from_parley(selection.anchor(), display_len);
        let focus = TextCursor::from_parley(selection.focus(), display_len);
        Self {
            anchor,
            focus,
            h_pos: None,
            base: SelectionBase::Word {
                start: anchor,
                end: focus,
            },
        }
    }

    fn hard_line_from_point(
        layout: &Layout<ColorBrush>,
        x: f32,
        y: f32,
        display_len: usize,
    ) -> Self {
        let selection = ParleySelection::hard_line_from_point(layout, x, y);
        let anchor = TextCursor::from_parley(selection.anchor(), display_len);
        let focus = TextCursor::from_parley(selection.focus(), display_len);
        Self {
            anchor,
            focus,
            h_pos: None,
            base: SelectionBase::Line {
                start: anchor,
                end: focus,
            },
        }
    }

    fn to_parley(self, layout: &Layout<ColorBrush>) -> ParleySelection {
        ParleySelection::new(self.anchor.to_parley(layout), self.focus.to_parley(layout))
    }

    fn extend_to_point(self, layout: &Layout<ColorBrush>, x: f32, y: f32) -> Self {
        let display_len = layout.text_len();
        let target = match self.base {
            SelectionBase::Cluster => {
                return Self {
                    anchor: self.anchor,
                    focus: TextCursor::from_parley(
                        ParleyCursor::from_point(layout, x, y),
                        display_len,
                    ),
                    h_pos: None,
                    base: self.base,
                };
            }
            SelectionBase::Word { start, end } => {
                (ParleySelection::word_from_point(layout, x, y), start, end)
            }
            SelectionBase::Line { start, end } => (
                ParleySelection::hard_line_from_point(layout, x, y),
                start,
                end,
            ),
        };
        let (anchor, focus) = extend_selection(target.0, target.1, target.2, layout);
        Self {
            anchor: TextCursor::from_parley(anchor, display_len),
            focus: TextCursor::from_parley(focus, display_len),
            h_pos: None,
            base: self.base,
        }
    }

    fn shift_click_extension(self, layout: &Layout<ColorBrush>, x: f32, y: f32) -> Self {
        let display_len = layout.text_len();
        let target = ParleyCursor::from_point(layout, x, y);
        let anchor = match self.base {
            SelectionBase::Cluster => self.anchor,
            SelectionBase::Word { start, end } | SelectionBase::Line { start, end } => {
                if target.index() < start.byte {
                    end
                } else {
                    start
                }
            }
        };
        Self {
            anchor,
            focus: TextCursor::from_parley(target, display_len),
            h_pos: None,
            base: self.base,
        }
    }

    fn normalized(self, display_len: usize) -> Self {
        Self {
            anchor: self.anchor.normalized(display_len),
            focus: self.focus.normalized(display_len),
            h_pos: self.h_pos,
            base: self.base,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.anchor == other.anchor && self.focus == other.focus && self.base == other.base
    }

    #[must_use]
    pub fn collapsed_at(byte: usize) -> Self {
        Self::collapsed(TextCursor {
            byte,
            affinity: TextAffinity::Downstream,
        })
    }

    pub fn anchor(&self) -> TextCursor {
        self.anchor
    }

    pub fn focus(&self) -> TextCursor {
        self.focus
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor.byte == self.focus.byte
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.byte.min(self.focus.byte)..self.anchor.byte.max(self.focus.byte)
    }
}

fn extend_selection(
    target: ParleySelection,
    start: TextCursor,
    end: TextCursor,
    layout: &Layout<ColorBrush>,
) -> (ParleyCursor, ParleyCursor) {
    let start_cursor = start.to_parley(layout);
    let end_cursor = end.to_parley(layout);
    let extending_right = target.anchor().index() >= start_cursor.index();
    let mut min = target.anchor();
    let mut max = target.anchor();
    for cursor in [target.focus(), start_cursor, end_cursor] {
        if cursor.index() < min.index() {
            min = cursor;
        }
        if cursor.index() > max.index() {
            max = cursor;
        }
    }
    if extending_right {
        (min, max)
    } else {
        (max, min)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextEditSnapshot {
    pub generation: u64,
    pub text: Arc<str>,
    pub selection: TextSelection,
    pub composing: bool,
    pub focused: bool,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextEditMetrics {
    pub line_height: LogicalPx,
    pub first_baseline: Option<LogicalPx>,
    pub line_count: usize,
    pub content_height: LogicalPx,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEditComposition {
    pub preedit: Range<usize>,
    pub caret: usize,
    pub clauses: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditableSpec {
    pub role: A11yRole,
    pub label: SmolStr,
    pub placeholder: SmolStr,
    pub multiline: bool,
    pub masked: bool,
    pub readonly: bool,
    pub disabled: bool,
    pub max_length: Option<usize>,
    pub submit: Option<Action>,
    pub text_style: TextStyle,
    pub caret_color: Color,
    pub selection_color: Color,
    pub placeholder_color: Color,
    pub composition_color: Color,
}

impl EditableSpec {
    pub(crate) fn validate(&self) -> Result<(), SmolStr> {
        if self.masked && self.multiline {
            return Err(SmolStr::new_static(
                "masked editable text cannot be multiline",
            ));
        }
        if !matches!(
            self.role,
            A11yRole::TextInput | A11yRole::MultilineTextInput | A11yRole::PasswordInput
        ) {
            return Err(SmolStr::new_static(
                "editable role must be TextInput, MultilineTextInput or PasswordInput",
            ));
        }
        if self.multiline != (self.role == A11yRole::MultilineTextInput) {
            return Err(SmolStr::new_static(
                "editable role does not match its multiline flag",
            ));
        }
        if self.masked != (self.role == A11yRole::PasswordInput) {
            return Err(SmolStr::new_static(
                "masked editable text must use the password role",
            ));
        }
        if !self.text_style.size.is_finite() || self.text_style.size <= LogicalPx::ZERO {
            return Err(SmolStr::new_static(
                "editable font size must be finite and greater than zero",
            ));
        }
        if self
            .text_style
            .line_height
            .is_some_and(|factor| !factor.is_finite() || factor <= 0.0)
        {
            return Err(SmolStr::new_static(
                "editable line height must be finite and greater than zero",
            ));
        }
        if self.text_style.max_lines != 0 {
            return Err(SmolStr::new_static(
                "editable text style cannot truncate with max_lines",
            ));
        }
        Ok(())
    }
}

pub(crate) enum DisplayText {
    Identity,
    Masked(MaskedDisplay),
    Composed(ComposedDisplay),
}

impl DisplayText {
    fn text<'a>(&'a self, committed: &'a str) -> &'a str {
        match self {
            Self::Identity => committed,
            Self::Masked(masked) => &masked.text,
            Self::Composed(composed) => &composed.text,
        }
    }
}

pub(crate) struct MaskedDisplay {
    text: SmolStr,
    raw_starts: SmallVec<[u32; 8]>,
    display_starts: SmallVec<[u32; 8]>,
}

impl MaskedDisplay {
    fn new(raw: &str) -> Self {
        let mut text = SmolStrBuilder::new();
        let mut raw_starts: SmallVec<[u32; 8]> = SmallVec::new();
        let mut display_starts: SmallVec<[u32; 8]> = SmallVec::new();
        let mut display_byte = 0usize;
        for (raw_byte, _) in raw.grapheme_indices(true) {
            raw_starts.push(u32::try_from(raw_byte).unwrap_or(u32::MAX));
            display_starts.push(u32::try_from(display_byte).unwrap_or(u32::MAX));
            text.push(MASK_GLYPH);
            display_byte += MASK_GLYPH.len_utf8();
        }
        Self {
            text: text.finish(),
            raw_starts,
            display_starts,
        }
    }

    #[cfg(test)]
    fn display_byte_for_raw(&self, raw: &str, raw_byte: usize) -> usize {
        if raw_byte >= raw.len() {
            return self.text.len();
        }
        let cluster = self
            .raw_starts
            .partition_point(|&start| start as usize <= raw_byte)
            - 1;
        self.display_starts[cluster] as usize
    }

    fn display_byte_after_raw(&self, raw: &str, raw_byte: usize) -> usize {
        if raw_byte >= raw.len() {
            return self.text.len();
        }
        let cluster = self
            .raw_starts
            .partition_point(|&start| start as usize <= raw_byte)
            - 1;
        if raw_byte == self.raw_starts[cluster] as usize {
            return self.display_starts[cluster] as usize;
        }
        self.display_starts
            .get(cluster + 1)
            .map_or(self.text.len(), |start| *start as usize)
    }

    fn raw_byte_for_display(&self, raw: &str, display_byte: usize) -> usize {
        if display_byte >= self.text.len() {
            return raw.len();
        }
        let cluster = self
            .display_starts
            .partition_point(|&start| start as usize <= display_byte)
            - 1;
        self.raw_starts[cluster] as usize
    }
}

pub(crate) struct ComposedDisplay {
    text: SmolStr,
    raw: Range<usize>,
    preedit_display: Range<usize>,
    caret_display: usize,
    clauses_display: SmallVec<[Range<usize>; 4]>,
    raw_starts: SmallVec<[u32; 8]>,
    display_starts: SmallVec<[u32; 8]>,
    display_lens: SmallVec<[u32; 8]>,
    committed_len: usize,
}

impl ComposedDisplay {
    fn new(committed: &str, masked: bool, composition: &Composition) -> Self {
        let preedit = composition.text.as_str();
        let raw = composition.raw.clone();
        let prefix = &committed[..raw.start];
        let suffix = &committed[raw.end..];
        let bullet = MASK_GLYPH.len_utf8();
        let prefix_clusters = prefix.graphemes(true).count();
        let preedit_clusters = preedit.graphemes(true).count();
        let suffix_clusters = suffix.graphemes(true).count();
        let prefix_display_len = if masked {
            prefix_clusters * bullet
        } else {
            prefix.len()
        };
        let preedit_display_len = if masked {
            preedit_clusters * bullet
        } else {
            preedit.len()
        };
        let suffix_display_len = if masked {
            suffix_clusters * bullet
        } else {
            suffix.len()
        };
        let preedit_display = prefix_display_len..prefix_display_len + preedit_display_len;
        let caret_byte = composition.caret_utf16.map_or(preedit.len(), |units| {
            byte_offset_for_utf16(preedit, units as usize)
        });
        let caret_in_preedit = if masked {
            preedit[..caret_byte].graphemes(true).count() * bullet
        } else {
            caret_byte.min(preedit.len())
        };
        let caret_display = prefix_display_len + caret_in_preedit;
        let mut clauses_display: SmallVec<[Range<usize>; 4]> = SmallVec::new();
        for clause in &composition.clauses_utf16 {
            let range = utf16_range_to_byte(preedit, clause);
            let start = prefix_display_len + display_len_of(preedit, masked, 0, range.start);
            let end = start + display_len_of(preedit, masked, range.start, range.end);
            if end > start {
                clauses_display.push(start..end);
            }
        }
        let mut text = SmolStrBuilder::with_capacity(
            prefix_display_len + preedit_display_len + suffix_display_len,
        );
        for segment in [prefix, preedit, suffix] {
            if masked {
                for _ in segment.graphemes(true) {
                    text.push(MASK_GLYPH);
                }
            } else {
                text.push_str(segment);
            }
        }
        let text = text.finish();
        let mut raw_starts: SmallVec<[u32; 8]> = SmallVec::new();
        let mut display_starts: SmallVec<[u32; 8]> = SmallVec::new();
        let mut display_lens: SmallVec<[u32; 8]> = SmallVec::new();
        let mut prefix_index = 0usize;
        let mut suffix_index = 0usize;
        for (raw_byte, grapheme) in committed.grapheme_indices(true) {
            if raw_byte >= raw.start && raw_byte < raw.end {
                continue;
            }
            let display_start = if raw_byte < raw.start {
                let start = if masked {
                    prefix_index * bullet
                } else {
                    raw_byte
                };
                prefix_index += 1;
                start
            } else {
                let start = prefix_display_len
                    + preedit_display_len
                    + if masked {
                        suffix_index * bullet
                    } else {
                        raw_byte - raw.end
                    };
                suffix_index += 1;
                start
            };
            let display_len = if masked {
                MASK_GLYPH.len_utf8()
            } else {
                grapheme.len()
            };
            raw_starts.push(u32::try_from(raw_byte).unwrap_or(u32::MAX));
            display_starts.push(u32::try_from(display_start).unwrap_or(u32::MAX));
            display_lens.push(u32::try_from(display_len).unwrap_or(u32::MAX));
        }
        Self {
            text,
            raw,
            preedit_display,
            caret_display,
            clauses_display,
            raw_starts,
            display_starts,
            display_lens,
            committed_len: committed.len(),
        }
    }

    fn cluster_display_end(&self, cluster: usize) -> usize {
        self.display_starts[cluster] as usize + self.display_lens[cluster] as usize
    }

    fn cluster_at_raw(&self, raw_byte: usize) -> Option<usize> {
        if self.raw_starts.is_empty() {
            return None;
        }
        Some(
            self.raw_starts
                .partition_point(|&start| start as usize <= raw_byte)
                .saturating_sub(1),
        )
    }

    fn cluster_at_display(&self, display_byte: usize) -> Option<usize> {
        if self.display_starts.is_empty() {
            return None;
        }
        Some(
            self.display_starts
                .partition_point(|&start| start as usize <= display_byte)
                .saturating_sub(1),
        )
    }

    fn raw_byte_for_display(&self, display_byte: usize) -> usize {
        if display_byte >= self.text.len() {
            return self.committed_len;
        }
        if display_byte >= self.preedit_display.start && display_byte < self.preedit_display.end {
            return self.raw.start;
        }
        match self.cluster_at_display(display_byte) {
            Some(cluster) => self.raw_starts[cluster] as usize,
            None => self.raw.start,
        }
    }

    #[cfg(test)]
    fn display_byte_for_raw(&self, raw_byte: usize) -> usize {
        if raw_byte == self.raw.start || (raw_byte > self.raw.start && raw_byte < self.raw.end) {
            return self.preedit_display.start;
        }
        if raw_byte >= self.committed_len {
            return self.text.len();
        }
        let Some(cluster) = self.cluster_at_raw(raw_byte) else {
            return if raw_byte > self.raw.start {
                self.text.len()
            } else {
                0
            };
        };
        self.display_starts[cluster] as usize
    }

    fn caret_display_byte_for_raw(&self, raw_byte: usize) -> usize {
        if raw_byte == self.raw.start {
            return self.preedit_display.start;
        }
        if raw_byte >= self.committed_len {
            return self.text.len();
        }
        if raw_byte < self.raw.start {
            let Some(cluster) = self.cluster_at_raw(raw_byte) else {
                return 0;
            };
            return if raw_byte == self.raw_starts[cluster] as usize {
                self.display_starts[cluster] as usize
            } else {
                self.cluster_display_end(cluster)
            };
        }
        if raw_byte < self.raw.end {
            return self.preedit_display.end;
        }
        let Some(cluster) = self.cluster_at_raw(raw_byte) else {
            return self.text.len();
        };
        if raw_byte == self.raw_starts[cluster] as usize {
            self.display_starts[cluster] as usize
        } else {
            self.cluster_display_end(cluster)
        }
    }
}

#[derive(Clone, Debug)]
struct Composition {
    raw: Range<usize>,
    text: SmolStr,
    caret_utf16: Option<u32>,
    clauses_utf16: SmallVec<[Range<usize>; 4]>,
    selection_before: TextSelection,
}

struct PlaceholderText {
    content: TextContent,
    shaped: Rc<ShapedText>,
    line: Option<Rc<ShapedLine>>,
    laid_width_bits: u32,
}

impl Composition {
    fn new(raw: Range<usize>, selection_before: TextSelection) -> Self {
        Self {
            raw,
            text: SmolStr::default(),
            caret_utf16: None,
            clauses_utf16: SmallVec::new(),
            selection_before,
        }
    }
}

fn display_len_of(text: &str, masked: bool, start: usize, end: usize) -> usize {
    if masked {
        text[start..end].graphemes(true).count() * MASK_GLYPH.len_utf8()
    } else {
        end.saturating_sub(start)
    }
}

fn byte_offset_for_utf16(text: &str, units: usize) -> usize {
    let mut remaining = units;
    for (byte, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if remaining < width {
            return byte;
        }
        remaining -= width;
        if remaining == 0 {
            return byte + ch.len_utf8();
        }
    }
    text.len()
}

fn utf16_range_to_byte(text: &str, range: &Range<usize>) -> Range<usize> {
    let start = byte_offset_for_utf16(text, range.start);
    let end = byte_offset_for_utf16(text, range.end).max(start);
    start..end
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn splice_preedit(
    current: &str,
    fragment: &str,
    caret: Option<u32>,
    replace: Option<&Range<usize>>,
) -> (SmolStr, Option<u32>) {
    let total = utf16_len(current);
    let (mut start_units, mut end_units) = match replace {
        Some(range) => (range.start.min(total), range.end.min(total)),
        None => (0, total),
    };
    if start_units > end_units {
        std::mem::swap(&mut start_units, &mut end_units);
    }
    let start_byte = byte_offset_for_utf16(current, start_units);
    let end_byte = byte_offset_for_utf16(current, end_units).max(start_byte);
    let caret = caret.map(|caret| {
        let fragment_units = utf16_len(fragment);
        u32::try_from(utf16_len(&current[..start_byte]) + (caret as usize).min(fragment_units))
            .unwrap_or(u32::MAX)
    });
    let mut text =
        SmolStrBuilder::with_capacity(current.len() - (end_byte - start_byte) + fragment.len());
    text.push_str(&current[..start_byte]);
    text.push_str(fragment);
    text.push_str(&current[end_byte..]);
    (text.finish(), caret)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditableColorSlot {
    Caret,
    Selection,
    Composition,
    Placeholder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKind {
    Insert,
    DeleteBackward,
    DeleteForward,
    Replace,
    Paste,
    CompositionCommit,
}

impl EditKind {
    fn coalesces(self) -> bool {
        matches!(
            self,
            Self::Insert | Self::DeleteBackward | Self::DeleteForward
        )
    }
}

struct UndoStep {
    kind: EditKind,
    range: Range<usize>,
    removed: Arc<str>,
    inserted: Arc<str>,
    selection_before: TextSelection,
    selection_after: TextSelection,
}

#[derive(Default)]
struct UndoStack {
    undo: Vec<UndoStep>,
    redo: Vec<UndoStep>,
    merge: bool,
}

impl UndoStack {
    fn record(&mut self, step: UndoStep) {
        self.redo.clear();
        if self.merge
            && let Some(last) = self.undo.last_mut()
            && merge_step(last, &step)
        {
            return;
        }
        self.merge = step.kind.coalesces();
        self.push_undo(step);
    }

    fn push_undo(&mut self, step: UndoStep) {
        if self.undo.len() >= UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(step);
    }

    fn push_redo(&mut self, step: UndoStep) {
        self.redo.push(step);
    }

    fn clear(&mut self) -> bool {
        let changed = !self.undo.is_empty() || !self.redo.is_empty();
        self.undo.clear();
        self.redo.clear();
        self.merge = false;
        changed
    }

    fn undo_len(&self) -> usize {
        self.undo.len()
    }

    fn redo_len(&self) -> usize {
        self.redo.len()
    }
}

fn merge_step(last: &mut UndoStep, step: &UndoStep) -> bool {
    if last.kind != step.kind || !step.kind.coalesces() {
        return false;
    }
    if last.selection_after != step.selection_before {
        return false;
    }
    match step.kind {
        EditKind::Insert => {
            if !last.removed.is_empty()
                || !step.removed.is_empty()
                || last.range.start + last.inserted.len() != step.range.start
            {
                return false;
            }
            last.inserted = concat_text(&last.inserted, &step.inserted);
            last.selection_after = step.selection_after;
            true
        }
        EditKind::DeleteBackward => {
            if !last.inserted.is_empty()
                || !step.inserted.is_empty()
                || step.range.end != last.range.start
            {
                return false;
            }
            last.removed = concat_text(&step.removed, &last.removed);
            last.range.start = step.range.start;
            last.selection_after = step.selection_after;
            true
        }
        EditKind::DeleteForward => {
            if !last.inserted.is_empty()
                || !step.inserted.is_empty()
                || step.range.start != last.range.start
            {
                return false;
            }
            last.removed = concat_text(&last.removed, &step.removed);
            last.range.end = last.range.start + last.removed.len();
            last.selection_after = step.selection_after;
            true
        }
        EditKind::Replace | EditKind::Paste | EditKind::CompositionCommit => false,
    }
}

fn concat_text(left: &Arc<str>, right: &Arc<str>) -> Arc<str> {
    let mut text = String::with_capacity(left.len() + right.len());
    text.push_str(left);
    text.push_str(right);
    Arc::from(text)
}

pub(crate) struct EditableText {
    committed: Arc<str>,
    display: DisplayText,
    spec: EditableSpec,
    selection: TextSelection,
    composition: Option<Composition>,
    placeholder: Option<PlaceholderText>,
    focused: bool,
    invalid: bool,
    caret_visible: bool,
    reveal: Option<RevealKey>,
    a11y_value: SmolStr,
    generation: u64,
    snapshot: TextEditSnapshot,
    graphemes: usize,
    undo: UndoStack,
    #[cfg(test)]
    display_builds: u32,
}

impl EditableText {
    pub(crate) fn new(spec: EditableSpec) -> Self {
        let committed: Arc<str> = Arc::from("");
        let selection = TextSelection::collapsed(TextCursor::at(0, 0));
        let mut editable = Self {
            snapshot: TextEditSnapshot {
                generation: 0,
                text: Arc::clone(&committed),
                selection,
                composing: false,
                focused: false,
                can_undo: false,
                can_redo: false,
            },
            committed,
            display: DisplayText::Identity,
            spec,
            selection,
            composition: None,
            placeholder: None,
            focused: false,
            invalid: false,
            caret_visible: true,
            reveal: None,
            a11y_value: SmolStr::default(),
            generation: 0,
            graphemes: 0,
            undo: UndoStack::default(),
            #[cfg(test)]
            display_builds: 0,
        };
        editable.snapshot = editable.build_snapshot();
        editable
    }

    pub(crate) fn committed(&self) -> &str {
        &self.committed
    }

    pub(crate) fn spec(&self) -> &EditableSpec {
        &self.spec
    }

    pub(crate) fn snapshot(&self) -> &TextEditSnapshot {
        &self.snapshot
    }

    pub(crate) fn selection(&self) -> TextSelection {
        self.selection
    }

    pub(crate) fn graphemes(&self) -> usize {
        self.graphemes
    }

    pub(crate) fn composing(&self) -> bool {
        self.composition.is_some()
    }

    pub(crate) fn display_text(&self) -> &str {
        self.display.text(&self.committed)
    }

    pub(crate) fn display_len(&self) -> usize {
        self.display_text().len()
    }

    pub(crate) fn raw_byte_for_display(&self, display_byte: usize) -> usize {
        match &self.display {
            DisplayText::Identity => display_byte.min(self.committed.len()),
            DisplayText::Masked(masked) => {
                masked.raw_byte_for_display(&self.committed, display_byte)
            }
            DisplayText::Composed(composed) => composed.raw_byte_for_display(display_byte),
        }
    }

    fn raw_range_for_display(&self, display: Range<usize>) -> Range<usize> {
        let start = floor_char_boundary(&self.committed, self.raw_byte_for_display(display.start));
        let end =
            ceil_char_boundary(&self.committed, self.raw_byte_for_display(display.end)).max(start);
        start..end
    }

    #[cfg(test)]
    pub(crate) fn display_byte_for_raw(&self, raw_byte: usize) -> usize {
        match &self.display {
            DisplayText::Identity => raw_byte.min(self.committed.len()),
            DisplayText::Masked(masked) => masked.display_byte_for_raw(&self.committed, raw_byte),
            DisplayText::Composed(composed) => composed.display_byte_for_raw(raw_byte),
        }
    }

    fn caret_display_byte_for_raw(&self, raw_byte: usize) -> usize {
        let raw_byte = raw_byte.min(self.committed.len());
        match &self.display {
            DisplayText::Identity => snap_caret_forward(&self.committed, raw_byte),
            DisplayText::Masked(masked) => masked.display_byte_after_raw(&self.committed, raw_byte),
            DisplayText::Composed(composed) => composed.caret_display_byte_for_raw(raw_byte),
        }
    }

    fn build_snapshot(&self) -> TextEditSnapshot {
        TextEditSnapshot {
            generation: self.generation,
            text: Arc::clone(&self.committed),
            selection: self.selection,
            composing: self.composition.is_some(),
            focused: self.focused,
            can_undo: self.undo.undo_len() > 0,
            can_redo: self.undo.redo_len() > 0,
        }
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.snapshot = self.build_snapshot();
    }

    fn build_display(&self) -> DisplayText {
        match &self.composition {
            Some(composition) => DisplayText::Composed(ComposedDisplay::new(
                &self.committed,
                self.spec.masked,
                composition,
            )),
            None if self.spec.masked => DisplayText::Masked(MaskedDisplay::new(&self.committed)),
            None => DisplayText::Identity,
        }
    }

    fn build_a11y_value(&self) -> SmolStr {
        if self.spec.masked {
            masked_text(&self.committed)
        } else {
            SmolStr::from(Arc::clone(&self.committed))
        }
    }

    fn a11y_value_for(&self, display: &DisplayText) -> SmolStr {
        match display {
            DisplayText::Masked(masked) => masked.text.clone(),
            DisplayText::Composed(_) | DisplayText::Identity => self.build_a11y_value(),
        }
    }

    fn install_committed(&mut self, next: String, text_changed: bool) -> bool {
        self.committed = Arc::from(next);
        let display = self.build_display();
        let a11y_value = self.a11y_value_for(&display);
        self.a11y_value = a11y_value;
        let display_changed = match (&self.display, &display) {
            (DisplayText::Identity, DisplayText::Identity) => text_changed,
            (DisplayText::Masked(previous), DisplayText::Masked(next_display)) => {
                previous.text.len() != next_display.text.len()
            }
            (DisplayText::Identity, DisplayText::Masked(_))
            | (DisplayText::Masked(_), DisplayText::Identity)
            | (DisplayText::Composed(_), _)
            | (_, DisplayText::Composed(_)) => true,
        };
        self.display = display;
        #[cfg(test)]
        {
            self.display_builds += 1;
        }
        display_changed
    }

    fn write_committed(&mut self, start: usize, end: usize, replacement: &str) -> Option<bool> {
        let len = self.committed.len();
        if start > end || end > len {
            return None;
        }
        if !self.committed.is_char_boundary(start) || !self.committed.is_char_boundary(end) {
            return None;
        }
        let removed = self.committed[start..end].graphemes(true).count();
        let inserted = replacement.graphemes(true).count();
        let mut next = String::with_capacity(len - (end - start) + replacement.len());
        next.push_str(&self.committed[..start]);
        next.push_str(replacement);
        next.push_str(&self.committed[end..]);
        self.graphemes = self.graphemes.saturating_sub(removed) + inserted;
        Some(self.install_committed(next, true))
    }

    fn set_selection(&mut self, selection: TextSelection) -> bool {
        let selection = selection.normalized(self.display_len());
        if self.selection.matches(&selection) {
            return false;
        }
        self.selection = selection;
        self.undo.merge = false;
        self.bump_generation();
        true
    }

    fn break_undo_group(&mut self) {
        self.undo.merge = false;
    }

    fn clear_history(&mut self) -> bool {
        self.undo.clear()
    }

    fn composition_start(&mut self) -> Option<bool> {
        if self.composition.is_some() || self.spec.readonly || self.spec.disabled {
            return None;
        }
        let selection_before = self.selection;
        let raw = self.raw_range_for_display(selection_before.range());
        self.composition = Some(Composition::new(raw, selection_before));
        let display_changed = self.refresh_composition_display();
        self.bump_generation();
        Some(display_changed)
    }

    fn composition_update(
        &mut self,
        fragment: &str,
        caret: Option<u32>,
        clauses: &[Range<usize>],
        replace: Option<&Range<usize>>,
    ) -> Option<bool> {
        if self.spec.readonly || self.spec.disabled {
            return None;
        }
        let started = if self.composition.is_none() {
            let selection_before = self.selection;
            let raw = self.raw_range_for_display(selection_before.range());
            self.composition = Some(Composition::new(raw, selection_before));
            true
        } else {
            false
        };
        let sanitized = sanitize(fragment, self.spec.multiline);
        let composition = self.composition.as_mut()?;
        let previous_text = composition.text.clone();
        let previous_caret = composition.caret_utf16;
        let previous_clauses = composition.clauses_utf16.clone();
        let (text, caret_utf16) = splice_preedit(&composition.text, &sanitized, caret, replace);
        let changed = started
            || previous_text != text
            || previous_caret != caret_utf16
            || previous_clauses.as_slice() != clauses;
        if !changed {
            return None;
        }
        composition.text = text;
        composition.caret_utf16 = caret_utf16;
        composition.clauses_utf16.clear();
        composition.clauses_utf16.extend(clauses.iter().cloned());
        let display_changed = self.refresh_composition_display();
        self.bump_generation();
        Some(display_changed)
    }

    fn composition_commit(
        &mut self,
        fragment: &str,
        replace: Option<&Range<usize>>,
    ) -> Option<bool> {
        let composition = self.composition.take()?;
        let sanitized = sanitize(fragment, self.spec.multiline);
        let (text, _) = splice_preedit(&composition.text, &sanitized, None, replace);
        let removed_graphemes = self.committed[composition.raw.clone()]
            .graphemes(true)
            .count();
        let room = match self.spec.max_length {
            Some(max) => max.saturating_sub(self.graphemes.saturating_sub(removed_graphemes)),
            None => usize::MAX,
        };
        let inserted = truncate_graphemes(&text, Some(room));
        if inserted.is_empty() && composition.raw.is_empty() {
            let display_changed = self.refresh_composition_display();
            let display_len = self.display_len();
            let caret = self.caret_display_byte_for_raw(composition.raw.start);
            self.selection = TextSelection::collapsed(TextCursor::at(caret, display_len));
            self.bump_generation();
            return Some(display_changed);
        }
        let removed: Arc<str> = Arc::from(&self.committed[composition.raw.clone()]);
        let inserted_arc: Arc<str> = Arc::from(inserted);
        let selection_before = composition.selection_before;
        let Some(display_changed) =
            self.write_committed(composition.raw.start, composition.raw.end, &inserted_arc)
        else {
            let display_changed = self.refresh_composition_display();
            let display_len = self.display_len();
            let caret = self.caret_display_byte_for_raw(composition.raw.start);
            self.selection = TextSelection::collapsed(TextCursor::at(caret, display_len));
            self.bump_generation();
            return Some(display_changed);
        };
        let display_len = self.display_len();
        let caret_raw = composition.raw.start + inserted_arc.len();
        let caret_display = self.caret_display_byte_for_raw(caret_raw);
        self.selection = TextSelection::collapsed(TextCursor::at(caret_display, display_len));
        let step = UndoStep {
            kind: EditKind::CompositionCommit,
            range: composition.raw,
            removed,
            inserted: inserted_arc,
            selection_before,
            selection_after: self.selection,
        };
        self.undo.record(step);
        self.bump_generation();
        Some(display_changed)
    }

    fn insert_committed(&mut self, text: &str, kind: EditKind) -> Option<bool> {
        let sanitized = sanitize(text, self.spec.multiline);
        let range = self.raw_range_for_display(self.selection.range());
        let removed = grapheme_count(&self.committed[range.clone()]);
        let room = match self.spec.max_length {
            Some(max) => max.saturating_sub(self.graphemes.saturating_sub(removed)),
            None => usize::MAX,
        };
        let inserted = truncate_graphemes(&sanitized, Some(room));
        if inserted.is_empty() {
            return None;
        }
        let caret = range.start + inserted.len();
        self.apply_edit(range, inserted, kind, AfterEdit::CollapsedAtRaw(caret))
    }

    fn cancel_composition(&mut self) -> Option<bool> {
        let display_changed = self.drop_composition()?;
        self.bump_generation();
        Some(display_changed)
    }

    fn drop_composition(&mut self) -> Option<bool> {
        let composition = self.composition.take()?;
        let display_changed = self.refresh_composition_display();
        let display_len = self.display_len();
        let caret = self.caret_display_byte_for_raw(composition.raw.start);
        self.selection = TextSelection::collapsed(TextCursor::at(caret, display_len));
        Some(display_changed)
    }

    fn refresh_composition_display(&mut self) -> bool {
        let display = self.build_display();
        let display_changed = self.display.text(&self.committed) != display.text(&self.committed);
        let display_len = display.text(&self.committed).len();
        let caret = match &display {
            DisplayText::Composed(composed) => composed.caret_display,
            _ => 0,
        };
        let a11y_value = self.a11y_value_for(&display);
        self.display = display;
        self.a11y_value = a11y_value;
        if self.composition.is_some() {
            self.selection = TextSelection::collapsed(TextCursor::at(caret, display_len));
        }
        #[cfg(test)]
        {
            self.display_builds += 1;
        }
        display_changed
    }

    fn set_focused(&mut self, focused: bool) -> Option<bool> {
        if self.focused == focused {
            return None;
        }
        let display_changed = if !focused {
            self.drop_composition().unwrap_or(false)
        } else {
            false
        };
        self.focused = focused;
        self.caret_visible = true;
        self.reveal = None;
        self.bump_generation();
        Some(display_changed)
    }

    fn a11y_patch(&self) -> A11yPatch {
        let selection = self
            .composition
            .is_none()
            .then_some(crate::a11y::A11yTextSelection {
                anchor: self.selection.anchor.byte,
                focus: self.selection.focus.byte,
            });
        A11yPatch {
            role: Some(self.spec.role),
            label: (!self.spec.label.is_empty()).then(|| self.spec.label.clone()),
            placeholder: (!self.spec.placeholder.is_empty()).then(|| self.spec.placeholder.clone()),
            value: Some(self.a11y_value.clone()),
            selection,
            read_only: Some(self.spec.readonly),
            disabled: Some(self.spec.disabled),
            invalid: Some(self.invalid),
            ..A11yPatch::default()
        }
    }

    fn composition_info(&self) -> Option<TextEditComposition> {
        let DisplayText::Composed(composed) = &self.display else {
            return None;
        };
        Some(TextEditComposition {
            preedit: composed.preedit_display.clone(),
            caret: composed.caret_display,
            clauses: composed.clauses_display.iter().cloned().collect(),
        })
    }

    pub(crate) fn focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn caret_visible(&self) -> bool {
        self.caret_visible
    }

    fn set_caret_visible(&mut self, visible: bool) -> bool {
        if self.caret_visible == visible {
            return false;
        }
        self.caret_visible = visible;
        true
    }

    fn reveal(&self) -> Option<RevealKey> {
        self.reveal
    }

    fn set_reveal(&mut self, reveal: Option<RevealKey>) {
        self.reveal = reveal;
    }

    pub(crate) fn composition_ranges(&self) -> Option<&[Range<usize>]> {
        match &self.display {
            DisplayText::Composed(composed) => Some(composed.clauses_display.as_slice()),
            DisplayText::Identity | DisplayText::Masked(_) => None,
        }
    }

    pub(crate) fn composition_caret(&self) -> Option<usize> {
        match &self.display {
            DisplayText::Composed(composed) => Some(composed.caret_display),
            DisplayText::Identity | DisplayText::Masked(_) => None,
        }
    }

    pub(crate) fn placeholder_content(&self) -> Option<TextContent> {
        if !self.committed.is_empty()
            || self.composition.is_some()
            || self.spec.placeholder.is_empty()
        {
            return None;
        }
        let mut style = self.spec.text_style.clone();
        style.color = self.spec.placeholder_color;
        Some(TextContent::new(self.spec.placeholder.clone(), style))
    }

    pub(crate) fn ensure_placeholder(
        &mut self,
        content: TextContent,
        shaped: Rc<ShapedText>,
    ) -> bool {
        let changed = match &self.placeholder {
            Some(placeholder) => {
                !Rc::ptr_eq(&placeholder.shaped, &shaped)
                    || placeholder.content.text != content.text
                    || placeholder.content.style.color != content.style.color
            }
            None => true,
        };
        if changed {
            self.placeholder = Some(PlaceholderText {
                content,
                shaped,
                line: None,
                laid_width_bits: 0,
            });
        }
        changed
    }

    pub(crate) fn placeholder_layout(&self) -> Option<(TextContent, Rc<ShapedText>)> {
        if !self.committed.is_empty()
            || self.composition.is_some()
            || self.spec.placeholder.is_empty()
        {
            return None;
        }
        self.placeholder
            .as_ref()
            .map(|placeholder| (placeholder.content.clone(), Rc::clone(&placeholder.shaped)))
    }

    pub(crate) fn placeholder_line_fresh(&self, generation: u32, width: LogicalPx) -> bool {
        self.placeholder.as_ref().is_some_and(|placeholder| {
            placeholder.laid_width_bits == width.value().to_bits()
                && placeholder
                    .line
                    .as_ref()
                    .is_some_and(|line| line.atlas_generation == generation)
        })
    }

    pub(crate) fn set_placeholder_line(&mut self, line: Rc<ShapedLine>, width: LogicalPx) {
        if let Some(placeholder) = &mut self.placeholder {
            placeholder.line = Some(line);
            placeholder.laid_width_bits = width.value().to_bits();
        }
    }

    pub(crate) fn clear_placeholder_line(&mut self) {
        if let Some(placeholder) = &mut self.placeholder {
            placeholder.line = None;
            placeholder.laid_width_bits = 0;
        }
    }

    fn set_placeholder_text(&mut self, placeholder: SmolStr) -> bool {
        if self.spec.placeholder == placeholder {
            return false;
        }
        self.spec.placeholder = placeholder;
        self.placeholder = None;
        true
    }

    fn set_label(&mut self, label: SmolStr) -> bool {
        if self.spec.label == label {
            return false;
        }
        self.spec.label = label;
        true
    }

    fn set_color(&mut self, slot: EditableColorSlot, color: Color) -> bool {
        let target = match slot {
            EditableColorSlot::Caret => &mut self.spec.caret_color,
            EditableColorSlot::Selection => &mut self.spec.selection_color,
            EditableColorSlot::Composition => &mut self.spec.composition_color,
            EditableColorSlot::Placeholder => &mut self.spec.placeholder_color,
        };
        if *target == color {
            return false;
        }
        *target = color;
        true
    }

    fn set_text_style(&mut self, style: TextStyle) -> bool {
        if self.spec.text_style == style {
            return false;
        }
        self.spec.text_style = style;
        self.placeholder = None;
        true
    }

    fn set_invalid(&mut self, invalid: bool) -> bool {
        if self.invalid == invalid {
            return false;
        }
        self.invalid = invalid;
        true
    }

    pub(crate) fn visible_placeholder_line(&self) -> Option<&Rc<ShapedLine>> {
        if !self.committed.is_empty()
            || self.composition.is_some()
            || self.spec.placeholder.is_empty()
        {
            return None;
        }
        self.placeholder
            .as_ref()
            .and_then(|placeholder| placeholder.line.as_ref())
    }

    fn apply_edit(
        &mut self,
        range: Range<usize>,
        inserted: &str,
        kind: EditKind,
        after: AfterEdit,
    ) -> Option<bool> {
        let start = floor_char_boundary(&self.committed, range.start);
        let end = ceil_char_boundary(&self.committed, range.end).max(start);
        if start == end && inserted.is_empty() {
            return None;
        }
        if start != end && self.committed[start..end] == *inserted {
            return None;
        }
        let removed: Arc<str> = Arc::from(&self.committed[start..end]);
        let inserted: Arc<str> = Arc::from(inserted);
        let selection_before = self.selection;
        let display_changed = self.write_committed(start, end, &inserted)?;
        let display_len = self.display_len();
        self.selection = match after {
            AfterEdit::CollapsedAtRaw(raw_byte) => {
                let display_byte = self.caret_display_byte_for_raw(raw_byte);
                TextSelection::collapsed(TextCursor::at(display_byte, display_len))
            }
        };
        let step = UndoStep {
            kind,
            range: start..end,
            removed,
            inserted,
            selection_before,
            selection_after: self.selection,
        };
        self.undo.record(step);
        self.bump_generation();
        Some(display_changed)
    }

    fn undo(&mut self) -> Option<bool> {
        let step = self.undo.undo.pop()?;
        let start = step.range.start;
        let end = start + step.inserted.len();
        match self.write_committed(start, end, &step.removed) {
            Some(display_changed) => {
                self.selection = step.selection_before.normalized(self.display_len());
                self.undo.push_redo(step);
                self.undo.merge = false;
                self.bump_generation();
                Some(display_changed)
            }
            None => {
                self.undo.undo.push(step);
                None
            }
        }
    }

    fn redo(&mut self) -> Option<bool> {
        let step = self.undo.redo.pop()?;
        let start = step.range.start;
        let end = start + step.removed.len();
        match self.write_committed(start, end, &step.inserted) {
            Some(display_changed) => {
                self.selection = step.selection_after.normalized(self.display_len());
                self.undo.push_undo(step);
                self.undo.merge = false;
                self.bump_generation();
                Some(display_changed)
            }
            None => {
                self.undo.redo.push(step);
                None
            }
        }
    }

    fn assign_committed(&mut self, text: &str) -> (bool, Option<SmolStr>) {
        let changed = self.committed.as_ref() != text;
        let mut display_changed = false;
        if changed {
            display_changed = self
                .write_committed(0, self.committed.len(), text)
                .unwrap_or(false);
        }
        let history_changed = self.clear_history();
        let display_len = self.display_len();
        let end = TextSelection::collapsed(TextCursor::at(display_len, display_len));
        let caret_changed = self.selection != end;
        self.selection = end;
        if changed || history_changed || caret_changed {
            self.bump_generation();
        }
        let display = display_changed.then(|| SmolStr::new(self.display_text()));
        (changed, display)
    }

    #[cfg(test)]
    pub(crate) fn display_builds(&self) -> u32 {
        self.display_builds
    }

    #[cfg(test)]
    pub(crate) fn undo_stats(&self) -> (usize, usize, usize) {
        let bytes = self
            .undo
            .undo
            .iter()
            .chain(self.undo.redo.iter())
            .map(|step| step.removed.len() + step.inserted.len())
            .sum();
        (self.undo.undo_len(), self.undo.redo_len(), bytes)
    }
}

enum AfterEdit {
    CollapsedAtRaw(usize),
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn snap_caret_forward(text: &str, byte: usize) -> usize {
    let byte = ceil_char_boundary(text, byte);
    if byte == 0 || byte == text.len() {
        return byte;
    }
    let mut cursor = GraphemeCursor::new(byte, text.len(), true);
    match cursor.is_boundary(text, 0) {
        Ok(true) => byte,
        Ok(false) | Err(_) => cursor
            .next_boundary(text, 0)
            .ok()
            .flatten()
            .unwrap_or(text.len()),
    }
}

pub(crate) fn value(app: &App, window: WindowId, node: NodeId) -> Result<SmolStr, EngineError> {
    let tree = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree();
    validate_node(tree, window, node)?;
    let editable = tree
        .editable(node)
        .ok_or(EngineError::TextEditNotAvailable { node })?;
    Ok(SmolStr::new(editable.committed()))
}

pub(crate) fn placeholder_color(
    app: &App,
    window: WindowId,
    node: NodeId,
) -> Result<Color, EngineError> {
    let tree = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree();
    validate_node(tree, window, node)?;
    let editable = tree
        .editable(node)
        .ok_or(EngineError::TextEditNotAvailable { node })?;
    Ok(editable.spec().placeholder_color)
}

pub(crate) fn snapshot(
    app: &App,
    window: WindowId,
    node: NodeId,
) -> Result<TextEditSnapshot, EngineError> {
    let tree = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree();
    validate_node(tree, window, node)?;
    let editable = tree
        .editable(node)
        .ok_or(EngineError::TextEditNotAvailable { node })?;
    Ok(editable.snapshot().clone())
}

pub(crate) fn composition(
    app: &App,
    window: WindowId,
    node: NodeId,
) -> Result<Option<TextEditComposition>, EngineError> {
    let tree = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree();
    validate_node(tree, window, node)?;
    let editable = tree
        .editable(node)
        .ok_or(EngineError::TextEditNotAvailable { node })?;
    Ok(editable.composition_info())
}

pub(crate) fn set_value(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    text: &str,
) -> Result<bool, EngineError> {
    let (multiline, max_length, style) = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        (
            editable.spec().multiline,
            editable.spec().max_length,
            editable.spec().text_style.clone(),
        )
    };
    let sanitized = sanitize(text, multiline);
    let truncated = truncate_graphemes(&sanitized, max_length);
    let (changed, display, touched) = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        let cancelled = editable.cancel_composition();
        let generation = editable.generation;
        let (changed, display) = editable.assign_committed(truncated);
        let display = match (display, cancelled) {
            (Some(display), _) => Some(display),
            (None, Some(true)) => Some(SmolStr::new(editable.display_text())),
            _ => None,
        };
        let touched = changed || cancelled.is_some() || editable.generation != generation;
        (changed, display, touched)
    };
    {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        if let Some(display) = display {
            tree.set_text_content(node, TextContent::new(display, style));
            tree.mark_paint(node);
        }
        if touched {
            tree.mark_a11y(node);
            sync_a11y_patch(tree, node);
        }
    }
    if touched {
        app.request_frame();
        note_activity(app, window);
    }
    Ok(changed)
}

pub(crate) fn set_label(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    label: SmolStr,
) -> Result<bool, EngineError> {
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        editable.set_label(label)
    };
    if changed {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        tree.mark_a11y(node);
        sync_a11y_patch(tree, node);
        app.request_frame();
    }
    Ok(changed)
}

pub(crate) fn set_placeholder(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    placeholder: &str,
) -> Result<bool, EngineError> {
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        editable.set_placeholder_text(SmolStr::new(placeholder))
    };
    if changed {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        refresh_display_text(tree, node)?;
        tree.mark_paint(node);
        tree.mark_a11y(node);
        sync_a11y_patch(tree, node);
        app.request_frame();
    }
    Ok(changed)
}

pub(crate) fn set_a11y_invalid(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    invalid: bool,
) -> Result<bool, EngineError> {
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        editable.set_invalid(invalid)
    };
    if changed {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        tree.mark_a11y(node);
        sync_a11y_patch(tree, node);
        app.request_frame();
    }
    Ok(changed)
}

pub(crate) fn set_color(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    slot: EditableColorSlot,
    color: Color,
) -> Result<bool, EngineError> {
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        editable.set_color(slot, color)
    };
    if changed {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        if slot == EditableColorSlot::Placeholder {
            refresh_display_text(tree, node)?;
        }
        tree.mark_paint(node);
        app.request_frame();
    }
    Ok(changed)
}

fn refresh_display_text(tree: &mut NodeTree, node: NodeId) -> Result<(), EngineError> {
    let (display, style) = {
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        (
            SmolStr::new(editable.display_text()),
            editable.spec().text_style.clone(),
        )
    };
    tree.set_text_content(node, TextContent::new(display, style));
    Ok(())
}

pub(crate) fn set_text_style(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    style: TextStyle,
) -> Result<bool, EngineError> {
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        let mut candidate = editable.spec().clone();
        candidate.text_style = style.clone();
        if let Err(detail) = candidate.validate() {
            return Err(EngineError::TextEditSpecInvalid { node, detail });
        }
        editable.set_text_style(style.clone())
    };
    if changed {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        let display = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?
            .display_text()
            .to_owned();
        tree.set_text_content(node, TextContent::new(SmolStr::new(display), style));
        tree.invalidate_text_layout(node);
        tree.mark_paint(node);
        app.request_frame();
    }
    Ok(changed)
}

pub(crate) fn metrics(
    app: &App,
    window: WindowId,
    node: NodeId,
) -> Result<TextEditMetrics, EngineError> {
    let tree = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree();
    validate_node(tree, window, node)?;
    if tree.editable(node).is_none() {
        return Err(EngineError::TextEditNotAvailable { node });
    }
    let width = ready_layout_width(tree, node)?;
    let Some(slot) = tree.text_slot(node) else {
        return Err(layout_error(node));
    };
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let metrics = slot
        .shaped()
        .line_metrics(width, content.style.alignment)
        .ok_or_else(|| EngineError::TextEditLayout {
            node,
            detail: SmolStr::new_static("text layout has no lines"),
        })?;
    Ok(TextEditMetrics {
        line_height: metrics.line_height,
        first_baseline: metrics.first_baseline,
        line_count: metrics.line_count,
        content_height: metrics.content_height,
    })
}

pub(crate) fn cancel_composition(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let result = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        match editable.cancel_composition() {
            Some(display_changed) => CommitResult::changed(
                display_changed.then(|| SmolStr::new(editable.display_text())),
                editable.spec().text_style.clone(),
            ),
            None => CommitResult::none(),
        }
    };
    commit_result(app, window, node, result)
}

pub(crate) fn undo(app: &mut App, window: WindowId, node: NodeId) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let result = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        if editable.spec().readonly || editable.spec().disabled {
            CommitResult::none()
        } else {
            match editable.undo() {
                Some(display_changed) => CommitResult::changed(
                    display_changed.then(|| SmolStr::new(editable.display_text())),
                    editable.spec().text_style.clone(),
                ),
                None => CommitResult::none(),
            }
        }
    };
    commit_result(app, window, node, result)
}

pub(crate) fn redo(app: &mut App, window: WindowId, node: NodeId) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let result = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        if editable.spec().readonly || editable.spec().disabled {
            CommitResult::none()
        } else {
            match editable.redo() {
                Some(display_changed) => CommitResult::changed(
                    display_changed.then(|| SmolStr::new(editable.display_text())),
                    editable.spec().text_style.clone(),
                ),
                None => CommitResult::none(),
            }
        }
    };
    commit_result(app, window, node, result)
}

struct CommitResult {
    changed: bool,
    display: Option<SmolStr>,
    style: Option<TextStyle>,
}

impl CommitResult {
    fn none() -> Self {
        Self {
            changed: false,
            display: None,
            style: None,
        }
    }

    fn changed(display: Option<SmolStr>, style: TextStyle) -> Self {
        Self {
            changed: true,
            display,
            style: Some(style),
        }
    }
}

fn write_commit_effects(tree: &mut NodeTree, node: NodeId, result: CommitResult) -> bool {
    if !result.changed {
        return false;
    }
    if let (Some(display), Some(style)) = (result.display, result.style) {
        tree.set_text_content(node, TextContent::new(display, style));
    }
    tree.mark_paint(node);
    tree.mark_a11y(node);
    sync_a11y_patch(tree, node);
    true
}

fn commit_result(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    result: CommitResult,
) -> Result<bool, EngineError> {
    if !result.changed {
        return Ok(false);
    }
    let Some(target) = app.window_mut(window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    write_commit_effects(target.tree_mut(), node, result);
    app.request_frame();
    note_activity(app, window);
    Ok(true)
}

pub(crate) fn sync_a11y_patch(tree: &mut NodeTree, node: NodeId) {
    let Some(patch) = tree.editable(node).map(EditableText::a11y_patch) else {
        return;
    };
    crate::a11y::tree::write_a11y_patch(tree, node, patch);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextMotion {
    PreviousVisual,
    NextVisual,
    PreviousWord,
    NextWord,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    Lines(isize),
    PageUp,
    PageDown,
}

pub(crate) fn apply_motion(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    motion: TextMotion,
    extend: bool,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let page_height = if matches!(motion, TextMotion::PageUp | TextMotion::PageDown) {
        page_viewport_height(app, window, node)
    } else {
        LogicalPx::ZERO
    };
    let selection = with_editable_layout(app, window, node, |editable, layout| {
        let current = editable.selection();
        let display_len = layout.text_len();
        match motion {
            TextMotion::PreviousVisual => TextSelection::from_parley(
                current.to_parley(layout).previous_visual(layout, extend),
                display_len,
            ),
            TextMotion::NextVisual => TextSelection::from_parley(
                current.to_parley(layout).next_visual(layout, extend),
                display_len,
            ),
            TextMotion::PreviousWord => TextSelection::from_parley(
                current
                    .to_parley(layout)
                    .previous_visual_word(layout, extend),
                display_len,
            ),
            TextMotion::NextWord => TextSelection::from_parley(
                current.to_parley(layout).next_visual_word(layout, extend),
                display_len,
            ),
            TextMotion::LineStart => TextSelection::from_parley(
                current.to_parley(layout).line_start(layout, extend),
                display_len,
            ),
            TextMotion::LineEnd => TextSelection::from_parley(
                current.to_parley(layout).line_end(layout, extend),
                display_len,
            ),
            TextMotion::DocumentStart => {
                let focus = ParleyCursor::from_byte_index(layout, 0, ParleyAffinity::Downstream);
                anchored_selection(current, layout, focus, extend, None, display_len)
            }
            TextMotion::DocumentEnd => {
                let focus =
                    ParleyCursor::from_byte_index(layout, usize::MAX, ParleyAffinity::Upstream);
                anchored_selection(current, layout, focus, extend, None, display_len)
            }
            TextMotion::Lines(delta) => vertical_selection(layout, current, delta, extend),
            TextMotion::PageUp => {
                let lines = page_line_count(layout, page_height);
                vertical_selection(layout, current, -lines, extend)
            }
            TextMotion::PageDown => {
                let lines = page_line_count(layout, page_height);
                vertical_selection(layout, current, lines, extend)
            }
        }
    })?;
    store_selection(app, window, node, selection)
}

pub(crate) fn set_selection(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    anchor_byte: usize,
    focus_byte: usize,
) -> Result<bool, EngineError> {
    let (display_len, composing) = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        (editable.display_len(), editable.composing())
    };
    let anchor = anchor_byte.min(display_len);
    let focus = focus_byte.min(display_len);
    let at_edge = |byte: usize| byte == 0 || byte == display_len;
    let (anchor, focus) = if at_edge(anchor) && at_edge(focus) {
        (anchor, focus)
    } else {
        with_editable_layout(app, window, node, |_, layout| {
            (
                converged_byte(layout, display_len, anchor),
                converged_byte(layout, display_len, focus),
            )
        })?
    };
    if composing {
        let (raw_anchor, raw_focus) = {
            let tree = app
                .window(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree();
            validate_node(tree, window, node)?;
            let editable = tree
                .editable(node)
                .ok_or(EngineError::TextEditNotAvailable { node })?;
            (
                editable.raw_byte_for_display(anchor),
                editable.raw_byte_for_display(focus),
            )
        };
        let _ = cancel_composition(app, window, node)?;
        let (display_len, display_anchor, display_focus) = {
            let tree = app
                .window(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree();
            validate_node(tree, window, node)?;
            let editable = tree
                .editable(node)
                .ok_or(EngineError::TextEditNotAvailable { node })?;
            (
                editable.display_len(),
                editable.caret_display_byte_for_raw(raw_anchor),
                editable.caret_display_byte_for_raw(raw_focus),
            )
        };
        return store_selection(
            app,
            window,
            node,
            TextSelection::ranged(
                TextCursor::at(display_anchor, display_len),
                TextCursor::at(display_focus, display_len),
            ),
        );
    }
    store_selection(
        app,
        window,
        node,
        TextSelection::ranged(
            TextCursor::at(anchor, display_len),
            TextCursor::at(focus, display_len),
        ),
    )
}

fn converged_byte(layout: &Layout<ColorBrush>, display_len: usize, byte: usize) -> usize {
    match parley::Cluster::from_byte_index(layout, byte) {
        Some(cluster) => cluster.text_range().start,
        None => display_len,
    }
}

pub(crate) fn set_selection_from_character_indices(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    anchor_character: usize,
    focus_character: usize,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node)?;
    let lengths = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        textrun::text_metrics(editable.display_text())
            .map(|(lengths, _)| lengths)
            .ok_or(EngineError::TextEditLayout {
                node,
                detail: SmolStr::new_static("a11y character lengths overflow u8"),
            })?
    };
    let anchor = textrun::byte_index(&lengths, anchor_character.min(lengths.len()));
    let focus = textrun::byte_index(&lengths, focus_character.min(lengths.len()));
    set_selection(app, window, node, anchor, focus)
}

pub(crate) fn select_all(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let display_len = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        editable.display_len()
    };
    set_selection(app, window, node, 0, display_len)
}

pub(crate) fn collapse_selection(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let start = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        editable.selection().range().start
    };
    set_selection(app, window, node, start, start)
}

pub(crate) fn insert_text(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    text: &str,
    kind: EditKind,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let prepared = with_editable_layout(app, window, node, |editable, _layout| {
        if editable.spec().readonly || editable.spec().disabled {
            return None;
        }
        let sanitized = sanitize(text, editable.spec().multiline);
        let selection = editable.selection();
        let display_range = selection.range();
        let raw_start = editable.raw_byte_for_display(display_range.start);
        let raw_end = editable.raw_byte_for_display(display_range.end);
        let removed = grapheme_count(&editable.committed()[raw_start..raw_end]);
        let room = match editable.spec().max_length {
            Some(max) => max.saturating_sub(editable.graphemes().saturating_sub(removed)),
            None => usize::MAX,
        };
        let inserted = truncate_graphemes(&sanitized, Some(room));
        if inserted.is_empty() {
            return None;
        }
        Some((raw_start..raw_end, SmolStr::new(inserted)))
    })?;
    let Some((range, inserted)) = prepared else {
        return Ok(false);
    };
    let kind = if range.is_empty() || matches!(kind, EditKind::Paste) {
        kind
    } else {
        EditKind::Replace
    };
    let caret = range.start + inserted.len();
    apply_raw_edit(
        app,
        window,
        node,
        range,
        &inserted,
        kind,
        AfterEdit::CollapsedAtRaw(caret),
    )
}

pub(crate) fn replace_selected_text(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    text: &str,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node)?;
    let prepared = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        if editable.spec().readonly || editable.spec().disabled {
            None
        } else {
            let sanitized = sanitize(text, editable.spec().multiline);
            let display = editable.selection().range();
            let range = editable.raw_byte_for_display(display.start)
                ..editable.raw_byte_for_display(display.end);
            let removed = grapheme_count(&editable.committed()[range.clone()]);
            let room = match editable.spec().max_length {
                Some(max) => max.saturating_sub(editable.graphemes().saturating_sub(removed)),
                None => usize::MAX,
            };
            let inserted = truncate_graphemes(&sanitized, Some(room));
            if range.is_empty() && inserted.is_empty() {
                None
            } else {
                Some((range, SmolStr::new(inserted)))
            }
        }
    };
    let Some((range, inserted)) = prepared else {
        return Ok(false);
    };
    let caret = range.start + inserted.len();
    apply_raw_edit(
        app,
        window,
        node,
        range,
        &inserted,
        EditKind::Replace,
        AfterEdit::CollapsedAtRaw(caret),
    )
}

pub(crate) fn delete_backward(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    word: bool,
) -> Result<bool, EngineError> {
    delete_side(app, window, node, false, word)
}

pub(crate) fn delete_forward(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    word: bool,
) -> Result<bool, EngineError> {
    delete_side(app, window, node, true, word)
}

pub(crate) fn copy_selection(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let text = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        if editable.spec().masked || editable.selection().is_collapsed() {
            return Ok(false);
        }
        let Some(range) = selected_raw_range(editable) else {
            return Ok(false);
        };
        SmolStr::new(&editable.committed()[range])
    };
    if text.is_empty() {
        return Ok(false);
    }
    Ok(app.set_clipboard_text(&text).is_ok())
}

pub(crate) fn cut_selection(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let prepared = {
        let tree = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree();
        validate_node(tree, window, node)?;
        let editable = tree
            .editable(node)
            .ok_or(EngineError::TextEditNotAvailable { node })?;
        if editable.spec().masked
            || editable.spec().readonly
            || editable.spec().disabled
            || editable.selection().is_collapsed()
        {
            None
        } else {
            selected_raw_range(editable)
                .map(|range| (range.clone(), SmolStr::new(&editable.committed()[range])))
        }
    };
    let Some((range, text)) = prepared else {
        return Ok(false);
    };
    if text.is_empty() || app.set_clipboard_text(&text).is_err() {
        return Ok(false);
    }
    let caret = range.start;
    apply_raw_edit(
        app,
        window,
        node,
        range,
        "",
        EditKind::Replace,
        AfterEdit::CollapsedAtRaw(caret),
    )
}

pub(crate) fn paste(app: &mut App, window: WindowId, node: NodeId) -> Result<bool, EngineError> {
    let Ok(text) = app.clipboard_text() else {
        return Ok(false);
    };
    if text.is_empty() {
        return Ok(false);
    }
    insert_text(app, window, node, &text, EditKind::Paste)
}

fn delete_side(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    forward: bool,
    word: bool,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let prepared = with_editable_layout(app, window, node, |editable, layout| {
        if editable.spec().readonly || editable.spec().disabled {
            return None;
        }
        let selection = editable.selection();
        let display_range = if selection.is_collapsed() {
            let focus = selection.focus.to_parley(layout);
            if word {
                let target = if forward {
                    focus.next_logical_word(layout)
                } else {
                    focus.previous_logical_word(layout)
                };
                if target.index() == focus.index() {
                    return None;
                }
                target.index().min(focus.index())..target.index().max(focus.index())
            } else {
                let cluster = if forward {
                    focus.downstream_cluster(layout)
                } else {
                    focus.upstream_cluster(layout)
                };
                let cluster = cluster?;
                cluster.text_range()
            }
        } else {
            selection.range()
        };
        if display_range.is_empty() {
            return None;
        }
        let raw = editable.raw_byte_for_display(display_range.start)
            ..editable.raw_byte_for_display(display_range.end);
        if raw.is_empty() {
            return None;
        }
        let caret = raw.start;
        Some((raw, caret))
    })?;
    let Some((range, caret)) = prepared else {
        return Ok(false);
    };
    let kind = if forward {
        EditKind::DeleteForward
    } else {
        EditKind::DeleteBackward
    };
    apply_raw_edit(
        app,
        window,
        node,
        range,
        "",
        kind,
        AfterEdit::CollapsedAtRaw(caret),
    )
}

fn apply_raw_edit(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    range: Range<usize>,
    inserted: &str,
    kind: EditKind,
    after: AfterEdit,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let result = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        if editable.spec().readonly || editable.spec().disabled {
            CommitResult::none()
        } else {
            match editable.apply_edit(range, inserted, kind, after) {
                Some(display_changed) => {
                    let display = display_changed.then(|| SmolStr::new(editable.display_text()));
                    CommitResult::changed(display, editable.spec().text_style.clone())
                }
                None => CommitResult::none(),
            }
        }
    };
    commit_result(app, window, node, result)
}

pub(crate) fn break_undo_group(app: &mut App, window: WindowId, node: NodeId) {
    if let Some(target) = app.window_mut(window)
        && let Some(editable) = target.tree_mut().editable_mut(node)
    {
        editable.break_undo_group();
    }
}

fn store_selection(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    selection: TextSelection,
) -> Result<bool, EngineError> {
    let _ = cancel_composition(app, window, node);
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let Some(editable) = tree.editable_mut(node) else {
            return Err(EngineError::TextEditNotAvailable { node });
        };
        let changed = editable.set_selection(selection);
        if changed {
            tree.mark_paint(node);
            tree.mark_a11y(node);
            sync_a11y_patch(tree, node);
        }
        changed
    };
    if changed {
        app.request_frame();
        note_activity(app, window);
    }
    Ok(changed)
}

fn selected_raw_range(editable: &EditableText) -> Option<Range<usize>> {
    let selection = editable.selection();
    if selection.is_collapsed() {
        return None;
    }
    let range = selection.range();
    Some(editable.raw_byte_for_display(range.start)..editable.raw_byte_for_display(range.end))
}

fn with_editable_layout<R>(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    body: impl FnOnce(&EditableText, &Layout<ColorBrush>) -> R,
) -> Result<R, EngineError> {
    if !layout_matches(app, window, node)? {
        let _ = app.refresh_text(window);
        if !layout_matches(app, window, node)? {
            return Err(layout_error(node));
        }
    }
    let target = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?;
    let tree = target.tree();
    validate_node(tree, window, node)?;
    let editable = tree
        .editable(node)
        .ok_or(EngineError::TextEditNotAvailable { node })?;
    let content = tree
        .text_content(node)
        .ok_or(EngineError::StaleNode { node })?;
    let shaped = tree
        .text_slot(node)
        .ok_or_else(|| layout_error(node))?
        .shaped();
    let width = tree
        .text_layout_width(node)
        .ok_or_else(|| layout_error(node))?;
    shaped
        .with_broken_layout(width, content.style.alignment, |layout| {
            body(editable, layout)
        })
        .ok_or_else(|| EngineError::TextEditLayout {
            node,
            detail: SmolStr::new_static("editable text has no parley layout"),
        })
}

fn layout_matches(app: &App, window: WindowId, node: NodeId) -> Result<bool, EngineError> {
    let tree = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree();
    validate_node(tree, window, node)?;
    let editable = tree
        .editable(node)
        .ok_or(EngineError::TextEditNotAvailable { node })?;
    let Some(slot) = tree.text_slot(node) else {
        return Ok(false);
    };
    let shaped = slot.shaped();
    Ok(shaped.text() == editable.display_text()
        && tree.text_content(node).is_some_and(|content| {
            shaped.same_shaping_inputs(&content.style, content.runs())
                && shaped.inline_boxes() == content.inline_boxes.as_ref()
        }))
}

fn page_viewport_height(app: &App, window: WindowId, node: NodeId) -> LogicalPx {
    let Some(target) = app.window(window) else {
        return LogicalPx::ZERO;
    };
    let tree = target.tree();
    let Some((ancestor, _)) = scroll::scroll_ancestor_on(tree, node, ScrollAxis::Vertical) else {
        return LogicalPx::ZERO;
    };
    scroll::viewport(tree, ancestor).size.height
}

fn page_line_count(layout: &Layout<ColorBrush>, viewport_height: LogicalPx) -> isize {
    let line_height = layout
        .lines()
        .next()
        .map_or(0.0, |line| line.metrics().line_height);
    let count = if viewport_height > LogicalPx::ZERO && line_height > 0.0 {
        (viewport_height.value() / line_height).floor() as usize
    } else {
        layout.len()
    };
    count.max(1) as isize
}

fn vertical_selection(
    layout: &Layout<ColorBrush>,
    current: TextSelection,
    delta: isize,
    extend: bool,
) -> TextSelection {
    let display_len = layout.text_len();
    if delta == 0 || layout.is_empty() {
        return current;
    }
    let parley_selection = current.to_parley(layout);
    let focus = parley_selection.focus();
    let line_limit = layout.len().saturating_sub(1);
    let geometry = focus.geometry(layout, 0.0);
    let y = ((geometry.y0 + geometry.y1) * 0.5) as f32;
    let line_index = line_for_y(layout, y).unwrap_or(line_limit);
    let signed = line_index as isize + delta;
    if signed < 0 {
        let target = ParleyCursor::from_byte_index(layout, 0, ParleyAffinity::Downstream);
        return anchored_selection(current, layout, target, extend, None, display_len);
    }
    if signed > line_limit as isize {
        let target = line_end_cursor(layout, line_limit);
        return anchored_selection(current, layout, target, extend, None, display_len);
    }
    let h_pos = current
        .h_pos
        .unwrap_or_else(|| focus.geometry(layout, 0.0).x0 as f32);
    let Some(line) = layout.get(signed as usize) else {
        return current;
    };
    let metrics = line.metrics();
    let y = (metrics.block_min_coord + metrics.block_max_coord) * 0.5;
    let target = ParleyCursor::from_point(layout, h_pos, y);
    anchored_selection(current, layout, target, extend, Some(h_pos), display_len)
}

fn anchored_selection(
    current: TextSelection,
    layout: &Layout<ColorBrush>,
    focus: ParleyCursor,
    extend: bool,
    h_pos: Option<f32>,
    display_len: usize,
) -> TextSelection {
    let anchor = if extend {
        current.anchor.to_parley(layout)
    } else {
        focus
    };
    TextSelection::ranged_with_h_pos(
        TextCursor::from_parley(anchor, display_len),
        TextCursor::from_parley(focus, display_len),
        h_pos,
    )
}

fn line_for_y(layout: &Layout<ColorBrush>, y: f32) -> Option<usize> {
    layout
        .lines()
        .position(|line| {
            let metrics = line.metrics();
            y >= metrics.block_min_coord && y < metrics.block_max_coord
        })
        .or_else(|| layout.len().checked_sub(1))
}

fn line_end_cursor(layout: &Layout<ColorBrush>, line_index: usize) -> ParleyCursor {
    let Some(line) = layout.get(line_index) else {
        return ParleyCursor::from_byte_index(layout, usize::MAX, ParleyAffinity::Upstream);
    };
    let (index, affinity) = if line.break_reason() == ParleyBreakReason::Explicit {
        line.text_range()
            .end
            .checked_sub(1)
            .and_then(|index| parley::Cluster::from_byte_index(layout, index))
            .map(|cluster| (cluster.text_range().start, ParleyAffinity::Downstream))
            .unwrap_or((layout.text_len(), ParleyAffinity::Upstream))
    } else {
        (line.text_range().end, ParleyAffinity::Upstream)
    };
    ParleyCursor::from_byte_index(layout, index, affinity)
}

fn validate_node(tree: &NodeTree, window: WindowId, node: NodeId) -> Result<(), EngineError> {
    if tree.window() != Some(window) || node.window() != Some(window) {
        return Err(EngineError::StaleNode { node });
    }
    tree.get(node).map(|_| ())
}

fn layout_error(node: NodeId) -> EngineError {
    EngineError::TextEditLayout {
        node,
        detail: SmolStr::new_static("not laid out yet"),
    }
}

fn ready_layout_width(tree: &NodeTree, node: NodeId) -> Result<LogicalPx, EngineError> {
    if tree.text_dirty_pending(node) {
        return Err(layout_error(node));
    }
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let Some(slot) = tree.text_slot(node) else {
        return Err(layout_error(node));
    };
    let shaped = slot.shaped();
    if shaped.text() != content.text.as_str()
        || !shaped.same_shaping_inputs(&content.style, content.runs())
        || shaped.inline_boxes() != content.inline_boxes.as_ref()
    {
        return Err(layout_error(node));
    }
    if tree.text_line(node).is_none() {
        return Err(layout_error(node));
    }
    tree.text_laid_width(node).ok_or_else(|| layout_error(node))
}

#[must_use]
pub fn sanitize_text(text: &str, multiline: bool) -> SmolStr {
    sanitize(text, multiline)
}

fn sanitize(text: &str, multiline: bool) -> SmolStr {
    let mut builder = SmolStrBuilder::new();
    for ch in text.chars() {
        if ch.is_control() {
            if ch == '\n' && multiline {
                builder.push(ch);
            }
            continue;
        }
        builder.push(ch);
    }
    builder.finish()
}

fn truncate_graphemes(text: &str, max_length: Option<usize>) -> &str {
    let Some(max_length) = max_length else {
        return text;
    };
    match text.grapheme_indices(true).nth(max_length) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

pub(crate) fn handle_ime(app: &mut App, window: WindowId, event: &ImeEvent) -> bool {
    let Some(context) = focused_editable(app, window) else {
        return false;
    };
    if context.readonly {
        return true;
    }
    if ime_edit(app, window, context.node, event) {
        app.request_frame();
        note_activity(app, window);
    }
    true
}

fn ime_edit(app: &mut App, window: WindowId, node: NodeId, event: &ImeEvent) -> bool {
    let Some(target) = app.window_mut(window) else {
        return false;
    };
    let tree = target.tree_mut();
    let Some(editable) = tree.editable_mut(node) else {
        return false;
    };
    let edit = match event {
        ImeEvent::Start => editable.composition_start(),
        ImeEvent::Preedit {
            text,
            caret,
            clauses,
            replace,
        } => editable.composition_update(text, *caret, clauses, replace.as_ref()),
        ImeEvent::End => editable.cancel_composition(),
        ImeEvent::Commit { text, replace } => {
            if editable.composing() {
                editable.composition_commit(text, replace.as_ref())
            } else {
                editable.insert_committed(text, EditKind::CompositionCommit)
            }
        }
    };
    let result = match edit {
        Some(display_changed) => CommitResult::changed(
            display_changed.then(|| SmolStr::new(editable.display_text())),
            editable.spec().text_style.clone(),
        ),
        None => CommitResult::none(),
    };
    write_commit_effects(tree, node, result)
}

pub(crate) fn apply_focus(tree: &mut NodeTree, node: NodeId, focused: bool) -> bool {
    let display_changed = {
        let Some(editable) = tree.editable_mut(node) else {
            return false;
        };
        editable.set_focused(focused)
    };
    let Some(display_changed) = display_changed else {
        return false;
    };
    if display_changed {
        let (text, style) = {
            let editable = tree.editable(node).expect("刚检查过");
            (
                SmolStr::new(editable.display_text()),
                editable.spec().text_style.clone(),
            )
        };
        tree.set_text_content(node, TextContent::new(text, style));
    }
    tree.mark_paint(node);
    tree.mark_a11y(node);
    sync_a11y_patch(tree, node);
    true
}

pub(crate) fn sync_ime_cursor_area(app: &mut App, window: WindowId) {
    if !app.ime_enabled(window) {
        clear_managed_ime_cursor_area(app, window);
        return;
    }
    let focused = app.focused(window);
    let editable_focused = focused.and_then(|node| {
        let tree = app.window(window)?.tree();
        (tree.editable(node).is_some() && tree.is_effectively_visible(node)).then_some(node)
    });
    let request = editable_focused.and_then(|node| ime_cursor_request(app, window, node));
    let Some(target) = app.window_mut(window) else {
        return;
    };
    if let Some((node, byte_index)) = request {
        target.set_ime_cursor_request(node, byte_index);
        return;
    }
    if let Some(node) = editable_focused {
        if target.ime_cursor_node() != Some(node) {
            target.clear_ime_cursor_request();
        }
        return;
    }
    if managed_ime_request(target) {
        target.clear_ime_cursor_request();
    }
}

pub(crate) fn clear_managed_ime_cursor_area(app: &mut App, window: WindowId) {
    let Some(target) = app.window_mut(window) else {
        return;
    };
    if managed_ime_request(target) {
        target.clear_ime_cursor_request();
    }
}

fn managed_ime_request(window: &Window) -> bool {
    window.ime_cursor_node().is_some_and(|node| {
        window.tree().get(node).is_err() || window.tree().editable(node).is_some()
    })
}

fn ime_cursor_request(app: &App, window: WindowId, node: NodeId) -> Option<(NodeId, usize)> {
    let tree = app.window(window)?.tree();
    let editable = tree.editable(node)?;
    if editable.spec().readonly || editable.spec().disabled {
        return None;
    }
    let byte_index = editable
        .composition_caret()
        .unwrap_or_else(|| editable.selection().focus().byte);
    let display = editable.display_text();
    if byte_index > display.len() || !display.is_char_boundary(byte_index) {
        return None;
    }
    tree.text_caret_rect(node, byte_index).ok()?;
    Some((node, byte_index))
}

pub(crate) fn cancel_composition_on_focus_loss(app: &mut App, window: WindowId) {
    let Some(node) = app.focused(window) else {
        return;
    };
    let _ = cancel_composition(app, window, node);
}

pub(crate) fn handle_text(app: &mut App, window: WindowId, text: &str) -> bool {
    let Some(context) = focused_editable(app, window) else {
        return false;
    };
    if context.composing || context.readonly {
        return true;
    }
    let _ = insert_text(app, window, context.node, text, EditKind::Insert);
    true
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextPointerPress {
    pub node: NodeId,
    pub origin: LogicalPoint,
    pub dragging: bool,
}

#[must_use]
pub(crate) fn text_pointer_press(app: &App, window: WindowId) -> Option<TextPointerPress> {
    app.window(window)?.pointer().text_edit
}

pub(crate) fn pointer_press(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    position: LogicalPoint,
    local_point: LogicalPoint,
    click_count: u8,
    modifiers: Modifiers,
) -> bool {
    let content_offset = {
        let Some(target) = app.window(window) else {
            return false;
        };
        let tree = target.tree();
        let Some(editable) = tree.editable(node) else {
            return false;
        };
        if editable.spec().disabled {
            return false;
        }
        tree.content_offset(node)
    };
    let point = LogicalPoint::new(
        local_point.x - content_offset.x,
        local_point.y - content_offset.y,
    );
    let _ = cancel_composition(app, window, node);
    let selection = with_editable_layout(app, window, node, |editable, layout| {
        let x = point.x.value();
        let y = point.y.value();
        let display_len = layout.text_len();
        match click_count {
            _ if modifiers.contains(Modifiers::SHIFT) => {
                editable.selection().shift_click_extension(layout, x, y)
            }
            2 => TextSelection::word_from_point(layout, x, y, display_len),
            count if count >= 3 => TextSelection::hard_line_from_point(layout, x, y, display_len),
            _ => TextSelection::from_parley(ParleySelection::from_point(layout, x, y), display_len),
        }
    });
    let Ok(selection) = selection else {
        return false;
    };
    if store_selection(app, window, node, selection).is_err() {
        return false;
    }
    note_activity(app, window);
    mark_revealed(app, window, node);
    let Some(target) = app.window_mut(window) else {
        return false;
    };
    target.pointer_mut().text_edit = Some(TextPointerPress {
        node,
        origin: position,
        dragging: false,
    });
    true
}

pub(crate) fn pointer_move(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
    drag_distance: f32,
) -> bool {
    let Some(press) = text_pointer_press(app, window) else {
        return false;
    };
    let node = press.node;
    if app.focused(window) != Some(node) || !editable_accepts_pointer(app, window, node) {
        cancel_pointer_press(app, window);
        return false;
    }
    if !press.dragging {
        let delta = LogicalPoint::new(position.x - press.origin.x, position.y - press.origin.y);
        if !drag_util::crosses_threshold(delta, drag_distance) {
            return true;
        }
        if let Some(target) = app.window_mut(window)
            && let Some(press) = target.pointer_mut().text_edit.as_mut()
        {
            press.dragging = true;
        }
    }
    auto_scroll_text(app, window, node, position);
    extend_pointer_selection(app, window, node, position);
    note_activity(app, window);
    true
}

pub(crate) fn pointer_release(app: &mut App, window: WindowId, position: LogicalPoint) -> bool {
    let Some(press) = text_pointer_press(app, window) else {
        return false;
    };
    if app.focused(window) != Some(press.node) {
        cancel_pointer_press(app, window);
        return false;
    }
    if press.dragging && editable_accepts_pointer(app, window, press.node) {
        extend_pointer_selection(app, window, press.node, position);
    }
    cancel_pointer_press(app, window);
    note_activity(app, window);
    mark_revealed(app, window, press.node);
    true
}

pub(crate) fn cancel_pointer_press(app: &mut App, window: WindowId) -> bool {
    let had = app
        .window_mut(window)
        .is_some_and(|target| target.pointer_mut().text_edit.take().is_some());
    if had {
        let _ = app.release_pointer(window);
    }
    had
}

fn editable_accepts_pointer(app: &App, window: WindowId, node: NodeId) -> bool {
    app.window(window)
        .and_then(|target| target.tree().editable(node))
        .is_some_and(|editable| !editable.spec().disabled)
}

fn extend_pointer_selection(app: &mut App, window: WindowId, node: NodeId, position: LogicalPoint) {
    let Some(point) = text_layout_point(app, window, node, position) else {
        return;
    };
    let selection = with_editable_layout(app, window, node, |editable, layout| {
        let height = layout.height();
        let (x, y) = if point.y.value() < 0.0 {
            (-1.0, -1.0)
        } else if point.y.value() > height {
            (layout.width() + 1.0, (height - 1.0).max(0.0))
        } else {
            (point.x.value(), point.y.value())
        };
        editable.selection().extend_to_point(layout, x, y)
    });
    if let Ok(selection) = selection {
        let _ = store_selection(app, window, node, selection);
    }
}

fn text_layout_point(
    app: &App,
    window: WindowId,
    node: NodeId,
    position: LogicalPoint,
) -> Option<LogicalPoint> {
    let tree = app.window(window)?.tree();
    let content = tree.content_offset(node);
    let local = match tree.hit_test(position) {
        Some(hit) if hit.node == node => hit.local_point,
        _ => {
            let bounds = tree.get(node).ok()?.layout().bounds;
            let offset = scroll::ancestor_offset(tree, node);
            LogicalPoint::new(
                position.x + offset.x - bounds.origin.x,
                position.y + offset.y - bounds.origin.y,
            )
        }
    };
    Some(LogicalPoint::new(local.x - content.x, local.y - content.y))
}

fn auto_scroll_text(app: &mut App, window: WindowId, node: NodeId, position: LogicalPoint) {
    let Some((handle, goal)) = auto_scroll_goal(app, window, node, position) else {
        return;
    };
    let _ = app.scroll_to(&handle, goal);
}

fn auto_scroll_goal(
    app: &App,
    window: WindowId,
    node: NodeId,
    position: LogicalPoint,
) -> Option<(crate::interaction::ScrollHandle, LogicalPoint)> {
    let target = app.window(window)?;
    let tree = target.tree();
    let bounds = tree.get(node).ok()?.layout().bounds;
    let node_screen = scroll::to_screen(tree, node, bounds);
    for (ancestor, slot) in scroll::scroll_ancestors(tree, node) {
        let viewport_screen = scroll::to_screen(tree, ancestor, scroll::viewport(tree, ancestor));
        let visible = node_screen.intersect(viewport_screen);
        let visible =
            if visible.size.width > LogicalPx::ZERO && visible.size.height > LogicalPx::ZERO {
                visible
            } else {
                viewport_screen
            };
        let (distance, direction) = match slot.axis {
            ScrollAxis::Vertical => {
                if position.y < visible.origin.y {
                    (visible.origin.y - position.y, -1.0_f32)
                } else if position.y > visible.max().y {
                    (position.y - visible.max().y, 1.0_f32)
                } else {
                    (LogicalPx::ZERO, 0.0_f32)
                }
            }
            ScrollAxis::Horizontal => {
                if position.x < visible.origin.x {
                    (visible.origin.x - position.x, -1.0_f32)
                } else if position.x > visible.max().x {
                    (position.x - visible.max().x, 1.0_f32)
                } else {
                    (LogicalPx::ZERO, 0.0_f32)
                }
            }
        };
        if direction == 0.0 {
            continue;
        }
        let max = scroll::max_offset(&slot, scroll::viewport(tree, ancestor).size);
        let max = match slot.axis {
            ScrollAxis::Vertical => max.y,
            ScrollAxis::Horizontal => max.x,
        };
        if max <= LogicalPx::ZERO {
            continue;
        }
        let step = px(distance
            .value()
            .clamp(1.0, (slot.line_height.value() * 2.0).max(1.0))
            * direction);
        let goal = match slot.axis {
            ScrollAxis::Vertical => LogicalPoint::new(slot.offset.x, slot.offset.y + step),
            ScrollAxis::Horizontal => LogicalPoint::new(slot.offset.x + step, slot.offset.y),
        };
        return Some((
            crate::interaction::ScrollHandle::new(window, ancestor),
            goal,
        ));
    }
    None
}

pub(crate) fn handle_key(
    app: &mut App,
    window: WindowId,
    code: KeyCode,
    repeat: bool,
    modifiers: Modifiers,
) -> bool {
    let Some(context) = focused_editable(app, window) else {
        return false;
    };
    let node = context.node;
    if context.composing {
        return match code {
            KeyCode::Escape if modifiers.is_empty() => {
                let _ = cancel_composition(app, window, node);
                true
            }
            KeyCode::Enter | KeyCode::NumpadEnter => true,
            _ => false,
        };
    }
    let shift = modifiers.contains(Modifiers::SHIFT);
    let alt_gr = modifiers.contains(Modifiers::CONTROL) && modifiers.contains(Modifiers::ALT);
    let rest = modifiers.difference(Modifiers::SHIFT);
    let plain = rest.is_empty();
    let command = !alt_gr && rest == command_modifier();
    let word = !alt_gr && rest == word_modifier();
    match code {
        KeyCode::Tab => false,
        KeyCode::Escape if modifiers.is_empty() => {
            collapse_selection(app, window, node).is_ok_and(|changed| changed)
        }
        KeyCode::ArrowLeft => {
            if word {
                motion(app, window, node, TextMotion::PreviousWord, shift)
            } else if plain {
                motion(app, window, node, TextMotion::PreviousVisual, shift)
            } else if command && !word {
                motion(app, window, node, TextMotion::LineStart, shift)
            } else {
                false
            }
        }
        KeyCode::ArrowRight => {
            if word {
                motion(app, window, node, TextMotion::NextWord, shift)
            } else if plain {
                motion(app, window, node, TextMotion::NextVisual, shift)
            } else if command && !word {
                motion(app, window, node, TextMotion::LineEnd, shift)
            } else {
                false
            }
        }
        KeyCode::ArrowUp => {
            if plain {
                motion(app, window, node, TextMotion::Lines(-1), shift)
            } else if command && !word {
                motion(app, window, node, TextMotion::DocumentStart, shift)
            } else {
                false
            }
        }
        KeyCode::ArrowDown => {
            if plain {
                motion(app, window, node, TextMotion::Lines(1), shift)
            } else if command && !word {
                motion(app, window, node, TextMotion::DocumentEnd, shift)
            } else {
                false
            }
        }
        KeyCode::PageUp => {
            if plain {
                motion(app, window, node, TextMotion::PageUp, shift)
            } else {
                false
            }
        }
        KeyCode::PageDown => {
            if plain {
                motion(app, window, node, TextMotion::PageDown, shift)
            } else {
                false
            }
        }
        KeyCode::Home => {
            if command {
                motion(app, window, node, TextMotion::DocumentStart, shift)
            } else if plain {
                motion(app, window, node, TextMotion::LineStart, shift)
            } else {
                false
            }
        }
        KeyCode::End => {
            if command {
                motion(app, window, node, TextMotion::DocumentEnd, shift)
            } else if plain {
                motion(app, window, node, TextMotion::LineEnd, shift)
            } else {
                false
            }
        }
        KeyCode::Backspace => {
            if context.readonly {
                word || plain
            } else if word {
                let _ = delete_backward(app, window, node, true);
                true
            } else if plain {
                let _ = delete_backward(app, window, node, false);
                true
            } else {
                false
            }
        }
        KeyCode::Delete => {
            if context.readonly {
                word || plain
            } else if word {
                let _ = delete_forward(app, window, node, true);
                true
            } else if plain {
                let _ = delete_forward(app, window, node, false);
                true
            } else {
                false
            }
        }
        KeyCode::Enter | KeyCode::NumpadEnter => handle_enter(app, window, &context, repeat),
        KeyCode::KeyA if command => {
            let _ = select_all(app, window, node);
            true
        }
        KeyCode::KeyC if command => {
            let _ = copy_selection(app, window, node);
            true
        }
        KeyCode::KeyX if command => {
            let _ = cut_selection(app, window, node);
            true
        }
        KeyCode::KeyV if command => {
            let _ = paste(app, window, node);
            true
        }
        KeyCode::KeyZ if command => {
            if shift {
                let _ = redo(app, window, node);
            } else {
                let _ = undo(app, window, node);
            }
            true
        }
        KeyCode::KeyY if command => {
            let _ = redo(app, window, node);
            true
        }
        _ => (plain || alt_gr) && is_printable_key(code),
    }
}

fn motion(app: &mut App, window: WindowId, node: NodeId, motion: TextMotion, extend: bool) -> bool {
    let _ = apply_motion(app, window, node, motion, extend);
    true
}

fn handle_enter(app: &mut App, window: WindowId, context: &KeyContext, repeat: bool) -> bool {
    if context.multiline {
        if context.readonly {
            return true;
        }
        break_undo_group(app, window, context.node);
        if insert_text(app, window, context.node, "\n", EditKind::Insert).unwrap_or(false) {
            break_undo_group(app, window, context.node);
        }
        return true;
    }
    if repeat {
        return true;
    }
    if let Some(action) = context.submit.clone() {
        let _ = app.dispatch_action(window, context.node, action, ActionSource::Key);
    }
    true
}

struct KeyContext {
    node: NodeId,
    multiline: bool,
    readonly: bool,
    composing: bool,
    submit: Option<Action>,
}

pub(crate) fn focused_node(app: &App, window: WindowId) -> Option<NodeId> {
    let node = app.focused(window)?;
    let tree = app.window(window)?.tree();
    let editable = tree.editable(node)?;
    (!editable.spec().disabled).then_some(node)
}

#[must_use]
pub(crate) fn caret_visible(app: &App, window: WindowId) -> Option<bool> {
    let node = focused_node(app, window)?;
    app.window(window)?
        .tree()
        .editable(node)
        .map(EditableText::caret_visible)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlinkTrigger {
    Frame,
    Timer,
    Activity,
    Focus,
}

pub(crate) fn sync_blink(app: &mut App, window: WindowId) {
    apply_blink(app, window, BlinkTrigger::Frame);
}

pub(crate) fn sync_window_focus(app: &mut App, window: WindowId, focused: bool) {
    app.caret_blink_mut().os_focused = focused;
    if focused {
        apply_blink(app, window, BlinkTrigger::Focus);
        return;
    }
    let owner = app.caret_blink().focused.and_then(NodeId::window);
    if owner == Some(window) {
        {
            let blink = app.caret_blink_mut();
            blink.focused = None;
            blink.visible = true;
        }
        cancel_blink_timer(app);
    }
    if let Some(node) = focused_node(app, window) {
        sync_caret_visible(app, window, node, false, true);
    }
}

pub(crate) fn resume_after_owner_release(app: &mut App) {
    let windows: Vec<WindowId> = app.windows().map(|(window, _)| window).collect();
    let mut marked = false;
    for window in windows {
        let Some(focused) = app.focused(window) else {
            continue;
        };
        if let Some(target) = app.window_mut(window) {
            target.tree_mut().mark_paint(focused);
            marked = true;
        }
    }
    if marked {
        app.request_frame();
    }
}

pub(crate) fn note_activity(app: &mut App, window: WindowId) {
    apply_blink(app, window, BlinkTrigger::Activity);
}

pub(crate) fn note_focus_change(app: &mut App, window: WindowId) {
    apply_blink(app, window, BlinkTrigger::Focus);
}

fn blink_tick(app: &mut App, window: WindowId) {
    app.caret_blink_mut().timer = None;
    if app.caret_blink().focused.and_then(NodeId::window) != Some(window) {
        return;
    }
    apply_blink(app, window, BlinkTrigger::Timer);
}

fn take_blink_ownership(app: &mut App, focused: Option<NodeId>, now: Instant) {
    let previous = app.caret_blink().focused;
    {
        let blink = app.caret_blink_mut();
        blink.focused = focused;
        blink.last_activity = now;
        blink.visible = true;
    }
    cancel_blink_timer(app);
    if let Some(previous) = previous
        && let Some(previous_window) = previous.window()
    {
        sync_caret_visible(app, previous_window, previous, false, true);
    }
}

fn apply_blink(app: &mut App, window: WindowId, trigger: BlinkTrigger) {
    let now = app.now();
    let focused = focused_node(app, window);
    let owner = app.caret_blink().focused.and_then(NodeId::window);
    if owner != Some(window) {
        let can_take = focused.is_some()
            && (trigger == BlinkTrigger::Focus
                || (owner.is_none()
                    && app.caret_blink().os_focused
                    && app
                        .focused_window()
                        .is_none_or(|focused_window| focused_window == window)));
        if can_take {
            take_blink_ownership(app, focused, now);
        } else {
            if let Some(node) = focused {
                sync_caret_visible(app, window, node, false, true);
            }
            return;
        }
    }
    if trigger == BlinkTrigger::Activity {
        app.caret_blink_mut().last_activity = now;
    }
    let previous = app.caret_blink().focused;
    if previous != focused {
        {
            let blink = app.caret_blink_mut();
            blink.focused = focused;
            blink.last_activity = now;
            blink.visible = true;
        }
        cancel_blink_timer(app);
        if let Some(previous) = previous
            && let Some(previous_window) = previous.window()
        {
            sync_caret_visible(app, previous_window, previous, false, false);
        }
    }
    let Some(node) = focused else {
        cancel_blink_timer(app);
        app.caret_blink_mut().visible = true;
        return;
    };
    let composing = app
        .window(window)
        .and_then(|target| target.tree().editable(node))
        .is_some_and(EditableText::composing);
    if composing {
        app.caret_blink_mut().visible = true;
        cancel_blink_timer(app);
        sync_caret_visible(app, window, node, true, trigger == BlinkTrigger::Activity);
        return;
    }
    if trigger == BlinkTrigger::Activity {
        app.caret_blink_mut().visible = true;
        sync_caret_visible(app, window, node, true, true);
        ensure_blink_timer(app, window, node);
        return;
    }
    let deadline = app.caret_blink().last_activity + CARET_BLINK_HALF;
    if now >= deadline {
        let next = !app.caret_blink().visible;
        {
            let blink = app.caret_blink_mut();
            blink.visible = next;
            blink.last_activity = now;
        }
        sync_caret_visible(app, window, node, next, trigger == BlinkTrigger::Timer);
    } else {
        let visible = app.caret_blink().visible;
        sync_caret_visible(app, window, node, visible, false);
    }
    ensure_blink_timer(app, window, node);
}

fn sync_caret_visible(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    visible: bool,
    request_frame: bool,
) -> bool {
    let Some(target) = app.window_mut(window) else {
        return false;
    };
    let tree = target.tree_mut();
    let changed = tree
        .editable_mut(node)
        .is_some_and(|editable| editable.set_caret_visible(visible));
    if changed {
        tree.mark_paint(node);
    }
    if changed && request_frame {
        app.request_frame();
    }
    changed
}

fn ensure_blink_timer(app: &mut App, window: WindowId, node: NodeId) {
    let timer = app.caret_blink().timer;
    if let Some(id) = timer {
        if app.timer_live(&id) {
            return;
        }
        app.caret_blink_mut().timer = None;
    }
    let deadline = app.caret_blink().last_activity + CARET_BLINK_HALF;
    let delay = deadline
        .saturating_duration_since(app.now())
        .max(Duration::from_millis(1));
    app.caret_blink_mut().timer = app
        .after_scoped(window, node, delay, move |app| blink_tick(app, window))
        .ok();
}

fn cancel_blink_timer(app: &mut App) {
    if let Some(id) = app.caret_blink_mut().timer.take() {
        let _ = app.cancel_timer(&id);
    }
}
pub(crate) fn ensure_visible(app: &mut App, window: WindowId) {
    let Some(node) = focused_node(app, window) else {
        return;
    };
    if text_pointer_press(app, window).is_some() {
        return;
    }
    let Some(geometry) = reveal_geometry(app, window, node) else {
        return;
    };
    let current = app
        .window(window)
        .and_then(|target| target.tree().editable(node))
        .and_then(EditableText::reveal);
    if current == Some(geometry.key) {
        return;
    }
    let Some((ancestor, slot)) = geometry.ancestor else {
        set_reveal(app, window, node, Some(geometry.key));
        return;
    };
    let line_height = metrics(app, window, node).map_or_else(
        |_| geometry.focus.size.height.max(px(1.0)),
        |metrics| metrics.line_height.max(geometry.focus.size.height),
    );
    let focus_wanted = reveal_offset(
        &slot,
        geometry.viewport,
        geometry.focus,
        geometry.multiline,
        line_height,
    );
    let wanted = focus_wanted.or_else(|| {
        geometry.anchor.and_then(|anchor| {
            let candidate = reveal_offset(
                &slot,
                geometry.viewport,
                anchor,
                geometry.multiline,
                line_height,
            )?;
            let target = clamp_reveal_target(&slot, geometry.viewport, candidate);
            reveal_keeps_focus_visible(&slot, geometry.viewport, geometry.focus, target)
                .then_some(target)
        })
    });
    let Some(wanted) = wanted else {
        set_reveal(app, window, node, Some(geometry.key));
        return;
    };
    let target = clamp_reveal_target(&slot, geometry.viewport, wanted);
    if target == slot.offset {
        set_reveal(app, window, node, Some(geometry.key));
        return;
    }
    let handle = crate::interaction::ScrollHandle::new(window, ancestor);
    if app.scroll_to(&handle, target).is_err() {
        return;
    }
    let reached = app
        .scroll_state(&handle)
        .is_some_and(|state| state.offset == target || state.offset == slot.offset);
    if reached {
        set_reveal(app, window, node, Some(geometry.key));
    }
}

#[must_use]
pub(crate) fn reveal_pending(app: &App, window: WindowId) -> bool {
    let Some(node) = focused_node(app, window) else {
        return false;
    };
    if text_pointer_press(app, window).is_some() {
        return false;
    }
    let Some(geometry) = reveal_geometry(app, window, node) else {
        return false;
    };
    let current = app
        .window(window)
        .and_then(|target| target.tree().editable(node))
        .and_then(EditableText::reveal);
    current != Some(geometry.key)
}

fn set_reveal(app: &mut App, window: WindowId, node: NodeId, reveal: Option<RevealKey>) {
    if let Some(target) = app.window_mut(window)
        && let Some(editable) = target.tree_mut().editable_mut(node)
    {
        editable.set_reveal(reveal);
    }
}

fn mark_revealed(app: &mut App, window: WindowId, node: NodeId) {
    if let Some(geometry) = reveal_geometry(app, window, node) {
        set_reveal(app, window, node, Some(geometry.key));
    }
}

fn caret_content_rect(tree: &NodeTree, node: NodeId, byte_index: usize) -> Option<LogicalRect> {
    let local = tree.text_caret_rect(node, byte_index).ok()?;
    let bounds = tree.get(node).ok()?.layout().bounds;
    let content_offset = tree.content_offset(node);
    Some(local.translate(LogicalPoint::new(
        bounds.origin.x + content_offset.x,
        bounds.origin.y + content_offset.y,
    )))
}

fn reveal_geometry(app: &App, window: WindowId, node: NodeId) -> Option<RevealGeometry> {
    let tree = app.window(window)?.tree();
    let editable = tree.editable(node)?;
    let multiline = editable.spec().multiline;
    let selection = editable.selection();
    let focus = caret_content_rect(tree, node, selection.focus().byte)?;
    let anchor = (!selection.is_collapsed())
        .then(|| caret_content_rect(tree, node, selection.anchor().byte))
        .flatten();
    let caret = anchor.map_or(focus, |anchor| focus.union(anchor));
    let ancestor = scroll::scroll_ancestor(tree, node);
    let viewport = ancestor.map_or_else(LogicalRect::default, |(container, _)| {
        scroll::viewport(tree, container)
    });
    let limit = ancestor.map_or_else(LogicalPoint::default, |(_, slot)| {
        scroll::max_offset(&slot, viewport.size)
    });
    Some(RevealGeometry {
        key: RevealKey {
            ancestor: ancestor.map(|(container, _)| container),
            caret,
            viewport: viewport.size,
            limit,
        },
        ancestor,
        focus,
        anchor,
        viewport,
        multiline,
    })
}

fn reveal_offset(
    slot: &crate::interaction::ScrollSlot,
    viewport: LogicalRect,
    caret: LogicalRect,
    multiline: bool,
    line_height: LogicalPx,
) -> Option<LogicalPoint> {
    match slot.axis {
        ScrollAxis::Vertical => {
            let available = viewport.size.height;
            let margin = line_height.max(px(1.0));
            let margin = margin.min(((available - margin) * 0.5).max(LogicalPx::ZERO));
            let top = slot.offset.y + viewport.origin.y + margin;
            let bottom = slot.offset.y + viewport.origin.y + available - margin;
            if caret.origin.y < top {
                Some(LogicalPoint::new(
                    slot.offset.x,
                    slot.offset.y + (caret.origin.y - top),
                ))
            } else if caret.max().y > bottom {
                Some(LogicalPoint::new(
                    slot.offset.x,
                    slot.offset.y + (caret.max().y - bottom),
                ))
            } else {
                None
            }
        }
        ScrollAxis::Horizontal if !multiline => {
            let available = viewport.size.width;
            let margin = px(CARET_REVEAL_MARGIN_X).min(available * 0.25);
            let left = slot.offset.x + viewport.origin.x + margin;
            let right = slot.offset.x + viewport.origin.x + available - margin;
            if caret.origin.x < left {
                Some(LogicalPoint::new(
                    slot.offset.x + (caret.origin.x - left),
                    slot.offset.y,
                ))
            } else if caret.max().x > right {
                Some(LogicalPoint::new(
                    slot.offset.x + (caret.max().x - right),
                    slot.offset.y,
                ))
            } else {
                None
            }
        }
        ScrollAxis::Horizontal => None,
    }
}

fn clamp_reveal_target(
    slot: &crate::interaction::ScrollSlot,
    viewport: LogicalRect,
    target: LogicalPoint,
) -> LogicalPoint {
    let mut probe = *slot;
    probe.offset = target;
    scroll::clamp_offset(&probe, viewport.size)
}

fn reveal_keeps_focus_visible(
    slot: &crate::interaction::ScrollSlot,
    viewport: LogicalRect,
    focus: LogicalRect,
    target: LogicalPoint,
) -> bool {
    match slot.axis {
        ScrollAxis::Vertical => {
            let start = target.y + viewport.origin.y;
            let end = start + viewport.size.height;
            focus.origin.y >= start && focus.max().y <= end
        }
        ScrollAxis::Horizontal => {
            let start = target.x + viewport.origin.x;
            let end = start + viewport.size.width;
            focus.origin.x >= start && focus.max().x <= end
        }
    }
}

fn focused_editable(app: &App, window: WindowId) -> Option<KeyContext> {
    let node = app.focused(window)?;
    let tree = app.window(window)?.tree();
    let editable = tree.editable(node)?;
    if editable.spec().disabled {
        return None;
    }
    Some(KeyContext {
        node,
        multiline: editable.spec().multiline,
        readonly: editable.spec().readonly,
        composing: editable.composing(),
        submit: editable.spec().submit.clone(),
    })
}

fn command_modifier() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::META
    } else {
        Modifiers::CONTROL
    }
}

fn word_modifier() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::ALT
    } else {
        Modifiers::CONTROL
    }
}

fn is_printable_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Backquote
            | KeyCode::Backslash
            | KeyCode::BracketLeft
            | KeyCode::BracketRight
            | KeyCode::Comma
            | KeyCode::Equal
            | KeyCode::IntlBackslash
            | KeyCode::IntlRo
            | KeyCode::IntlYen
            | KeyCode::Minus
            | KeyCode::Period
            | KeyCode::Quote
            | KeyCode::Semicolon
            | KeyCode::Slash
            | KeyCode::KeyA
            | KeyCode::KeyB
            | KeyCode::KeyC
            | KeyCode::KeyD
            | KeyCode::KeyE
            | KeyCode::KeyF
            | KeyCode::KeyG
            | KeyCode::KeyH
            | KeyCode::KeyI
            | KeyCode::KeyJ
            | KeyCode::KeyK
            | KeyCode::KeyL
            | KeyCode::KeyM
            | KeyCode::KeyN
            | KeyCode::KeyO
            | KeyCode::KeyP
            | KeyCode::KeyQ
            | KeyCode::KeyR
            | KeyCode::KeyS
            | KeyCode::KeyT
            | KeyCode::KeyU
            | KeyCode::KeyV
            | KeyCode::KeyW
            | KeyCode::KeyX
            | KeyCode::KeyY
            | KeyCode::KeyZ
            | KeyCode::Digit0
            | KeyCode::Digit1
            | KeyCode::Digit2
            | KeyCode::Digit3
            | KeyCode::Digit4
            | KeyCode::Digit5
            | KeyCode::Digit6
            | KeyCode::Digit7
            | KeyCode::Digit8
            | KeyCode::Digit9
            | KeyCode::Numpad0
            | KeyCode::Numpad1
            | KeyCode::Numpad2
            | KeyCode::Numpad3
            | KeyCode::Numpad4
            | KeyCode::Numpad5
            | KeyCode::Numpad6
            | KeyCode::Numpad7
            | KeyCode::Numpad8
            | KeyCode::Numpad9
            | KeyCode::NumpadAdd
            | KeyCode::NumpadComma
            | KeyCode::NumpadDecimal
            | KeyCode::NumpadDivide
            | KeyCode::NumpadEqual
            | KeyCode::NumpadMultiply
            | KeyCode::NumpadSubtract
            | KeyCode::Space
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smol_str::SmolStr;
    use unicode_segmentation::UnicodeSegmentation;

    use super::{
        DisplayText, EditableSpec, EditableText, MASK_GLYPH, SelectionBase, TextAffinity,
        TextCursor, TextSelection, byte_offset_for_utf16, sanitize, sanitize_text,
        snap_caret_forward, splice_preedit, truncate_graphemes,
    };
    use crate::{a11y::A11yRole, event::Action, math::px, paint::color::Color, text::TextStyle};

    fn spec() -> EditableSpec {
        EditableSpec {
            role: A11yRole::TextInput,
            label: SmolStr::new_static(""),
            placeholder: SmolStr::new_static(""),
            multiline: false,
            masked: false,
            readonly: false,
            disabled: false,
            max_length: None,
            submit: None,
            text_style: TextStyle::new(px(14.0)),
            caret_color: Color::BLACK,
            selection_color: Color::BLACK,
            placeholder_color: Color::BLACK,
            composition_color: Color::BLACK,
        }
    }

    fn masked_spec(max_length: Option<usize>) -> EditableSpec {
        EditableSpec {
            role: A11yRole::PasswordInput,
            masked: true,
            max_length,
            ..spec()
        }
    }

    fn masked_editable(text: &str) -> EditableText {
        let mut editable = EditableText::new(masked_spec(None));
        editable.assign_committed(text);
        editable
    }

    #[test]
    fn collapsed_at_builds_a_downstream_caret_selection() {
        let selection = TextSelection::collapsed_at(7);
        assert!(selection.is_collapsed());
        assert_eq!(selection.anchor().byte, 7);
        assert_eq!(selection.focus().byte, 7);
        assert_eq!(selection.anchor().affinity, TextAffinity::Downstream);
        assert_eq!(selection.range(), 7..7);
    }

    #[test]
    fn sanitize_text_exposes_the_input_cleaning_rule() {
        assert_eq!(sanitize_text("a\u{8}b\u{1b}c", false), "abc");
        assert_eq!(sanitize_text("a\tb\nc\r\nd", false), "abcd");
        assert_eq!(sanitize_text("a\tb\nc\r\nd", true), "ab\nc\nd");
        assert_eq!(sanitize_text("e\u{301}👍🏽", true), "e\u{301}👍🏽");
    }

    #[test]
    fn label_setter_updates_the_spec_and_patch() {
        let mut editable = EditableText::new(spec());
        assert_eq!(editable.a11y_patch().label, None);
        assert!(editable.set_label(SmolStr::new_static("Search")));
        assert_eq!(editable.spec().label, "Search");
        assert_eq!(
            editable.a11y_patch().label,
            Some(SmolStr::new_static("Search"))
        );
        assert!(!editable.set_label(SmolStr::new_static("Search")));
        assert!(editable.set_label(SmolStr::new_static("")));
        assert_eq!(editable.a11y_patch().label, None);
    }

    #[test]
    fn invalid_flag_reaches_the_a11y_patch() {
        let mut editable = EditableText::new(spec());
        assert_eq!(editable.a11y_patch().invalid, Some(false));
        assert!(editable.set_invalid(true));
        assert_eq!(editable.a11y_patch().invalid, Some(true));
        assert!(!editable.set_invalid(true));
        assert!(editable.set_invalid(false));
        assert_eq!(editable.a11y_patch().invalid, Some(false));
    }

    #[test]
    fn sanitize_drops_control_characters_and_keeps_multiline_newlines() {
        assert_eq!(sanitize("a\u{8}b\u{1b}c", false), "abc");
        assert_eq!(sanitize("a\tb\nc\r\nd", false), "abcd");
        assert_eq!(sanitize("a\tb\nc\r\nd", true), "ab\nc\nd");
        assert_eq!(sanitize("e\u{301}👍🏽", true), "e\u{301}👍🏽");
    }

    #[test]
    fn validate_accepts_a_plain_single_line_spec() {
        assert!(spec().validate().is_ok());
        let mut multiline = spec();
        multiline.role = A11yRole::MultilineTextInput;
        multiline.multiline = true;
        assert!(multiline.validate().is_ok());
        assert!(masked_spec(None).validate().is_ok());
    }

    #[test]
    fn validate_rejects_specs_that_break_the_layout_contract() {
        let mut masked_multiline = spec();
        masked_multiline.masked = true;
        masked_multiline.multiline = true;
        masked_multiline.role = A11yRole::MultilineTextInput;
        assert_eq!(
            masked_multiline.validate().unwrap_err(),
            "masked editable text cannot be multiline"
        );

        let mut wrong_role = spec();
        wrong_role.role = A11yRole::Button;
        assert_eq!(
            wrong_role.validate().unwrap_err(),
            "editable role must be TextInput, MultilineTextInput or PasswordInput"
        );

        let mut role_mismatch = spec();
        role_mismatch.multiline = true;
        assert_eq!(
            role_mismatch.validate().unwrap_err(),
            "editable role does not match its multiline flag"
        );

        let mut masked_text_input = spec();
        masked_text_input.masked = true;
        assert_eq!(
            masked_text_input.validate().unwrap_err(),
            "masked editable text must use the password role"
        );

        let mut plain_password = spec();
        plain_password.role = A11yRole::PasswordInput;
        assert_eq!(
            plain_password.validate().unwrap_err(),
            "masked editable text must use the password role"
        );

        let mut multiline_password = spec();
        multiline_password.role = A11yRole::PasswordInput;
        multiline_password.masked = true;
        multiline_password.multiline = true;
        assert_eq!(
            multiline_password.validate().unwrap_err(),
            "masked editable text cannot be multiline"
        );

        let mut bad_size = spec();
        bad_size.text_style.size = px(0.0);
        assert_eq!(
            bad_size.validate().unwrap_err(),
            "editable font size must be finite and greater than zero"
        );

        let mut bad_line_height = spec();
        bad_line_height.text_style.line_height = Some(0.0);
        assert_eq!(
            bad_line_height.validate().unwrap_err(),
            "editable line height must be finite and greater than zero"
        );

        let mut truncating = spec();
        truncating.text_style.max_lines = 2;
        assert_eq!(
            truncating.validate().unwrap_err(),
            "editable text style cannot truncate with max_lines"
        );
    }

    #[test]
    fn submit_action_stays_optional() {
        let mut editable = spec();
        assert_eq!(editable.submit, None);
        editable.submit = Some(Action::custom("submit"));
        assert!(editable.submit.is_some());
    }

    #[test]
    fn masked_display_replaces_each_grapheme_with_one_bullet() {
        for (text, expected) in [
            ("abc", "•••"),
            ("e\u{301}", "•"),
            ("👨‍👩‍👧", "•"),
            ("🇨🇳", "•"),
            ("👍🏽", "•"),
            ("a👍🏽🇨🇳e\u{301}", "••••"),
        ] {
            let editable = masked_editable(text);
            assert_eq!(editable.committed(), text);
            assert_eq!(editable.display_text(), expected, "字素簇 {text:?}");
        }
    }

    #[test]
    fn masked_display_maps_every_grapheme_boundary_in_both_directions() {
        let text = "a👍🏽🇨🇳e\u{301}";
        let editable = masked_editable(text);
        let raw_starts: Vec<usize> = text
            .grapheme_indices(true)
            .map(|(raw_start, _)| raw_start)
            .collect();
        assert_eq!(raw_starts, vec![0, 1, 9, 17]);
        assert_eq!(editable.display_text(), "••••");

        let bullet = MASK_GLYPH.len_utf8();
        for (cluster, raw_start) in raw_starts.iter().enumerate() {
            let display_start = cluster * bullet;
            assert_eq!(
                editable.display_byte_for_raw(*raw_start),
                display_start,
                "簇 {cluster} 的 raw→display 边界"
            );
            assert_eq!(
                editable.raw_byte_for_display(display_start),
                *raw_start,
                "簇 {cluster} 的 display→raw 边界"
            );
        }

        assert_eq!(editable.display_byte_for_raw(text.len()), 4 * bullet);
        assert_eq!(editable.raw_byte_for_display(4 * bullet), text.len());
        assert_eq!(editable.display_byte_for_raw(4), bullet);
        assert_eq!(editable.display_byte_for_raw(12), 2 * bullet);
        assert_eq!(editable.display_byte_for_raw(18), 3 * bullet);
        assert_eq!(editable.raw_byte_for_display(4), 1);
        assert_eq!(editable.raw_byte_for_display(7), 9);
        assert_eq!(editable.raw_byte_for_display(10), 17);
        assert_eq!(editable.raw_byte_for_display(usize::MAX), text.len());
        assert_eq!(editable.display_byte_for_raw(usize::MAX), 4 * bullet);

        let empty = masked_editable("");
        assert_eq!(empty.display_text(), "");
        assert_eq!(empty.display_byte_for_raw(0), 0);
        assert_eq!(empty.raw_byte_for_display(0), 0);
        assert_eq!(empty.display_byte_for_raw(5), 0);
        assert_eq!(empty.raw_byte_for_display(5), 0);
    }

    #[test]
    fn masked_mapping_handles_long_text_at_every_boundary() {
        let text = "👍🏽".repeat(1000);
        let editable = masked_editable(&text);
        assert_eq!(editable.display_text().len(), 3000);
        for cluster in [0usize, 1, 2, 499, 500, 998, 999] {
            let raw_start = cluster * 8;
            let display_start = cluster * 3;
            assert_eq!(editable.display_byte_for_raw(raw_start), display_start);
            assert_eq!(editable.raw_byte_for_display(display_start), raw_start);
        }
        assert_eq!(editable.raw_byte_for_display(2999), 999 * 8);
        assert_eq!(editable.display_byte_for_raw(7999), 2997);
        assert_eq!(editable.display_byte_for_raw(text.len()), 3000);
        assert_eq!(editable.raw_byte_for_display(3000), text.len());
    }

    #[test]
    fn unmasked_display_is_the_committed_text_without_a_mapping() {
        let mut editable = EditableText::new(spec());
        assert!(matches!(editable.display, DisplayText::Identity));
        editable.assign_committed("a👍🏽");
        assert!(matches!(editable.display, DisplayText::Identity));
        assert_eq!(editable.display_text(), "a👍🏽");
        assert_eq!(editable.display_byte_for_raw(0), 0);
        assert_eq!(editable.display_byte_for_raw(5), 5);
        assert_eq!(editable.raw_byte_for_display(5), 5);
        assert_eq!(editable.display_byte_for_raw(usize::MAX), 9);
        assert_eq!(editable.raw_byte_for_display(usize::MAX), 9);
    }

    #[test]
    fn snapshots_share_the_text_allocation_until_the_content_changes() {
        let mut editable = masked_editable("abc");
        assert_eq!(editable.display_builds(), 1);
        let first = editable.snapshot().clone();
        let second = editable.snapshot().clone();
        assert!(Arc::ptr_eq(&first.text, &second.text));
        assert_eq!(second.generation, first.generation);
        assert_eq!(editable.display_builds(), 1, "读取快照不得重建映射");

        assert!(editable.assign_committed("abcd").0);
        let third = editable.snapshot().clone();
        assert_eq!(editable.display_builds(), 2);
        assert!(!Arc::ptr_eq(&first.text, &third.text));
        assert_eq!(third.generation, first.generation + 1);

        assert!(editable.assign_committed("abcde").0);
        assert_eq!(editable.display_builds(), 3);
    }

    #[test]
    fn max_length_truncation_only_lands_on_grapheme_boundaries() {
        assert_eq!(truncate_graphemes("a👍🏽b", None), "a👍🏽b");
        assert_eq!(truncate_graphemes("a👍🏽b", Some(2)), "a👍🏽");
        assert_eq!(truncate_graphemes("e\u{301}x", Some(1)), "e\u{301}");
        assert_eq!(truncate_graphemes("abc", Some(3)), "abc");
        assert_eq!(truncate_graphemes("abc", Some(0)), "");
        assert_eq!(truncate_graphemes("", Some(0)), "");
        assert_eq!(truncate_graphemes("", Some(3)), "");
    }

    #[test]
    fn selection_collapses_by_byte_and_normalizes_the_display_edges() {
        let empty = TextSelection::collapsed(TextCursor::at(0, 0));
        assert!(empty.is_collapsed());
        assert_eq!(empty.focus().affinity, TextAffinity::Downstream);

        let end = TextCursor::at(3, 3);
        assert_eq!(end.affinity, TextAffinity::Upstream);
        assert_eq!(end.normalized(3).byte, 3);

        let out_of_range = TextCursor {
            byte: 99,
            affinity: TextAffinity::Upstream,
        };
        assert_eq!(out_of_range.normalized(3).byte, 3);
        assert!(
            TextSelection::ranged(TextCursor::at(0, 3), TextCursor::at(2, 3)).range() == (0..2)
        );

        let same_byte = TextSelection::ranged(
            TextCursor {
                byte: 1,
                affinity: TextAffinity::Downstream,
            },
            TextCursor {
                byte: 1,
                affinity: TextAffinity::Upstream,
            },
        );
        assert!(
            same_byte.is_collapsed(),
            "parley 语义：is_collapsed 只看 byte"
        );
    }

    #[test]
    fn masked_caret_mapping_snaps_inside_cluster_to_the_cluster_end() {
        let editable = masked_editable("e\u{301}x");
        assert_eq!(editable.display_text(), "••");
        let DisplayText::Masked(masked) = &editable.display else {
            panic!("masked 文本必须使用掩码映射");
        };
        let raw = "e\u{301}x";
        assert_eq!(
            masked.display_byte_after_raw(raw, 0),
            0,
            "簇起点保持 display 簇起点"
        );
        assert_eq!(
            masked.display_byte_after_raw(raw, 1),
            3,
            "簇内 byte 吸附到簇尾"
        );
        assert_eq!(masked.display_byte_after_raw(raw, 2), 3);
        assert_eq!(masked.display_byte_after_raw(raw, 3), 3, "下一簇起点保持");
        assert_eq!(masked.display_byte_after_raw(raw, 4), 6);
        assert_eq!(masked.display_byte_after_raw(raw, usize::MAX), 6);
        assert_eq!(editable.caret_display_byte_for_raw(1), 3);
        assert_eq!(editable.caret_display_byte_for_raw(4), 6);
    }

    #[test]
    fn identity_caret_mapping_snaps_forward_inside_a_grapheme() {
        assert_eq!(snap_caret_forward("e\u{301}x", 0), 0);
        assert_eq!(snap_caret_forward("e\u{301}x", 1), 3);
        assert_eq!(snap_caret_forward("e\u{301}x", 2), 3);
        assert_eq!(snap_caret_forward("e\u{301}x", 3), 3);
        assert_eq!(snap_caret_forward("e\u{301}x", 4), 4);
        assert_eq!(snap_caret_forward("👨‍👩‍👧", 4), "👨‍👩‍👧".len());
        assert_eq!(snap_caret_forward("", 0), 0);
        assert_eq!(snap_caret_forward("abc", 99), 3);
    }

    fn editable_with(text: &str) -> EditableText {
        let mut editable = EditableText::new(spec());
        editable.assign_committed(text);
        editable
    }

    fn select_at(editable: &mut EditableText, start: usize, end: usize) {
        let display_len = editable.display_len();
        editable.selection = TextSelection::ranged(
            TextCursor::at(start, display_len),
            TextCursor::at(end, display_len),
        );
    }

    fn start_composition(editable: &mut EditableText) {
        assert!(editable.composition_start().is_some(), "组合必须启动");
    }

    #[test]
    fn composed_identity_display_maps_display_and_raw_around_the_preedit() {
        let mut editable = editable_with("abcd");
        select_at(&mut editable, 2, 2);
        start_composition(&mut editable);
        assert!(
            editable
                .composition_update("ni", Some(2), &[], None)
                .is_some()
        );
        assert_eq!(editable.display_text(), "abnicd");
        assert_eq!(editable.committed(), "abcd");
        let info = editable.composition_info().expect("组合信息");
        assert_eq!(info.preedit, 2..4);
        assert_eq!(info.caret, 4);
        assert!(info.clauses.is_empty());

        for (display, raw) in [(0, 0), (1, 1), (2, 2), (3, 2), (4, 2), (5, 3), (6, 4)] {
            assert_eq!(
                editable.raw_byte_for_display(display),
                raw,
                "display {display} → raw"
            );
        }
        assert_eq!(editable.display_byte_for_raw(0), 0);
        assert_eq!(editable.display_byte_for_raw(1), 1);
        assert_eq!(editable.display_byte_for_raw(2), 2);
        assert_eq!(
            editable.display_byte_for_raw(3),
            5,
            "suffix 之后的 raw 起点映射到 shift 后的 display"
        );
        assert_eq!(editable.display_byte_for_raw(4), 6);
        assert_eq!(
            editable.caret_display_byte_for_raw(2),
            2,
            "组合起点 caret 在 preedit 前"
        );
        assert_eq!(editable.caret_display_byte_for_raw(3), 5);
        assert_eq!(editable.caret_display_byte_for_raw(4), 6);
    }

    #[test]
    fn composed_display_handles_a_replaced_selection() {
        let mut editable = editable_with("abcd");
        select_at(&mut editable, 1, 3);
        start_composition(&mut editable);
        assert!(
            editable
                .composition_update("xy", Some(1), &[], None)
                .is_some()
        );
        assert_eq!(editable.display_text(), "axyd");
        assert_eq!(editable.committed(), "abcd");
        let info = editable.composition_info().expect("组合信息");
        assert_eq!(info.preedit, 1..3);
        assert_eq!(info.caret, 2);
        assert_eq!(editable.raw_byte_for_display(0), 0);
        assert_eq!(editable.raw_byte_for_display(1), 1);
        assert_eq!(
            editable.raw_byte_for_display(2),
            1,
            "preedit 内映射到组合起点"
        );
        assert_eq!(editable.raw_byte_for_display(3), 3);
        assert_eq!(editable.raw_byte_for_display(4), 4);
        assert_eq!(editable.caret_display_byte_for_raw(1), 1);
        assert_eq!(
            editable.caret_display_byte_for_raw(2),
            3,
            "组合终点 caret 在 preedit 后"
        );
        assert_eq!(editable.caret_display_byte_for_raw(3), 3);
        assert_eq!(editable.caret_display_byte_for_raw(4), 4);

        assert!(editable.cancel_composition().is_some());
        assert_eq!(editable.display_text(), "abcd");
        assert_eq!(editable.caret_display_byte_for_raw(2), 2);
    }

    #[test]
    fn composed_masked_display_uses_one_bullet_per_preedit_grapheme() {
        let mut editable = EditableText::new(masked_spec(None));
        editable.assign_committed("ab");
        editable.selection = TextSelection::collapsed(TextCursor::at(6, 6));
        start_composition(&mut editable);
        assert!(
            editable
                .composition_update("👨‍👩‍👧x", None, &[], None)
                .is_some()
        );
        assert_eq!(editable.display_text(), "••••");
        assert_eq!(editable.committed(), "ab");
        let info = editable.composition_info().expect("组合信息");
        assert_eq!(info.preedit, 6..12);
        assert_eq!(info.caret, 12, "caret=None 落在 preedit 末尾");
        let patch = editable.a11y_patch();
        assert_eq!(
            patch.value.as_deref(),
            Some("••"),
            "a11y value 不含 preedit"
        );
        assert!(
            patch.selection.is_none(),
            "组合期不暴露 preedit：选区一并不暴露"
        );
        assert_eq!(editable.raw_byte_for_display(6), 2);
        assert_eq!(editable.raw_byte_for_display(9), 2);
        assert_eq!(editable.raw_byte_for_display(12), 2);
        assert_eq!(editable.raw_byte_for_display(11), 2);
        assert_eq!(editable.display_byte_for_raw(1), 3);
        assert_eq!(editable.display_byte_for_raw(2), 6);
        assert_eq!(editable.caret_display_byte_for_raw(2), 6);

        assert!(editable.composition_update("👨‍👩‍👧", None, &[], None).is_some());
        assert_eq!(editable.display_text(), "•••");
        assert_eq!(
            editable.raw_byte_for_display(7),
            2,
            "preedit 内映射到组合起点"
        );
        assert_eq!(
            editable.caret_display_byte_for_raw(2),
            6,
            "组合起点 caret 在 preedit 前"
        );

        assert!(
            editable
                .composition_update("e\u{301}", None, &[], None)
                .is_some()
        );
        assert_eq!(
            editable.display_text(),
            "•••",
            "组合字符簇在掩码 display 里仍是一个 •"
        );
        let info = editable.composition_info().expect("组合信息");
        assert_eq!(info.preedit, 6..9);
        assert_eq!(info.caret, 9);
    }

    #[test]
    fn composition_commit_replaces_the_raw_range_and_records_one_undo_step() {
        let mut editable = editable_with("abcd");
        select_at(&mut editable, 1, 3);
        start_composition(&mut editable);
        assert!(
            editable
                .composition_update("ni", Some(2), &[], None)
                .is_some()
        );
        assert!(editable.composition_commit("你", None).is_some());
        assert_eq!(editable.committed(), "a你d");
        assert_eq!(editable.display_text(), "a你d");
        assert!(!editable.composing());
        assert_eq!(editable.undo_stats().0, 1, "一次组合提交 = 一步 undo");
        assert!(editable.undo().is_some());
        assert_eq!(editable.committed(), "abcd");
        assert_eq!(editable.selection().range(), 1..3, "撤销恢复组合前选区");
        assert!(editable.redo().is_some());
        assert_eq!(editable.committed(), "a你d");
    }

    #[test]
    fn composition_commit_clips_to_grapheme_max_length() {
        let mut editable = EditableText::new(masked_spec(Some(2)));
        editable.assign_committed("a");
        editable.selection = TextSelection::collapsed(TextCursor::at(3, 3));
        start_composition(&mut editable);
        assert!(editable.composition_update("👨‍👩‍👧", None, &[], None).is_some());
        assert!(editable.composition_commit("🇨🇳x", None).is_some());
        assert_eq!(editable.committed(), "a🇨🇳");
        assert_eq!(editable.display_text(), "••");
    }

    #[test]
    fn composition_end_is_idempotent_and_restores_the_caret() {
        let mut editable = editable_with("abcd");
        select_at(&mut editable, 2, 2);
        start_composition(&mut editable);
        assert!(editable.composition_update("ni", None, &[], None).is_some());
        assert!(editable.cancel_composition().is_some());
        assert_eq!(editable.display_text(), "abcd");
        assert!(!editable.composing());
        assert_eq!(editable.selection().range(), 2..2);
        assert!(editable.cancel_composition().is_none(), "重复取消幂等");
    }

    #[test]
    fn splice_preedit_applies_utf16_replace_ranges_and_caret_offsets() {
        let (text, caret) = splice_preedit("ni", "hao", Some(3), Some(&(0..1)));
        assert_eq!(text, "haoi");
        assert_eq!(caret, Some(3));

        let (text, caret) = splice_preedit("你好", "a", Some(0), Some(&(1..2)));
        assert_eq!(text, "你a");
        assert_eq!(
            caret,
            Some(1),
            "替换起点前的 UTF-16 单位数 + fragment caret"
        );

        let (text, caret) = splice_preedit("ni", "你", Some(1), None);
        assert_eq!(text, "你");
        assert_eq!(caret, Some(1));

        let (text, caret) = splice_preedit("", "🌍a", Some(1), None);
        assert_eq!(text, "🌍a");
        assert_eq!(caret, Some(1), "fragment caret 按 UTF-16 单位");
    }

    #[test]
    fn byte_offset_for_utf16_lands_on_char_boundaries() {
        assert_eq!(byte_offset_for_utf16("ni你好", 0), 0);
        assert_eq!(byte_offset_for_utf16("ni你好", 2), 2);
        assert_eq!(byte_offset_for_utf16("ni你好", 3), 5);
        assert_eq!(byte_offset_for_utf16("ni你好", 4), 8);
        assert_eq!(byte_offset_for_utf16("ni你好", 99), 8);
        assert_eq!(byte_offset_for_utf16("🌍a", 1), 0, "代理对中间落到字符起点");
        assert_eq!(byte_offset_for_utf16("🌍a", 2), 4);
        assert_eq!(byte_offset_for_utf16("🌍a", 3), 5);
    }

    #[test]
    fn equal_range_with_cluster_base_refreshes_a_word_selection_base() {
        let mut editable = editable_with("hello world");
        let start = TextCursor::at(6, 11);
        let end = TextCursor::at(11, 11);
        editable.selection = TextSelection {
            anchor: start,
            focus: end,
            h_pos: None,
            base: SelectionBase::Word { start, end },
        };
        assert!(
            editable.set_selection(TextSelection::ranged(start, end)),
            "同范围但 base 不同必须视为选择变化（否则拖拽粒度残留）"
        );
        assert_eq!(editable.selection.base, SelectionBase::Cluster);
        assert!(
            !editable.set_selection(TextSelection::ranged(start, end)),
            "范围与 base 都相同才是 no-op"
        );
    }
}
