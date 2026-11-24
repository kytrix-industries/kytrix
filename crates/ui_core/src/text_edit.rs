use std::{ops::Range, sync::Arc};

use parley::{
    Affinity as ParleyAffinity, BreakReason as ParleyBreakReason, Cursor as ParleyCursor, Layout,
    Selection as ParleySelection,
};
use smallvec::SmallVec;
use smol_str::{SmolStr, SmolStrBuilder};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

use crate::{
    a11y::A11yRole,
    app::App,
    error::EngineError,
    event::{Action, ActionSource, KeyCode, Modifiers},
    interaction::scroll,
    math::LogicalPx,
    node::{id::NodeId, tree::NodeTree},
    paint::color::{Color, ColorBrush},
    text::{TextContent, TextStyle},
    window::WindowId,
};

const MASK_GLYPH: char = '\u{2022}';

const UNDO_LIMIT: usize = 100;

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

#[derive(Clone, Copy, Debug)]
pub struct TextSelection {
    anchor: TextCursor,
    focus: TextCursor,
    h_pos: Option<f32>,
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
        }
    }

    fn ranged(anchor: TextCursor, focus: TextCursor) -> Self {
        Self {
            anchor,
            focus,
            h_pos: None,
        }
    }

    fn ranged_with_h_pos(anchor: TextCursor, focus: TextCursor, h_pos: Option<f32>) -> Self {
        Self {
            anchor,
            focus,
            h_pos,
        }
    }

    fn from_parley(selection: ParleySelection, display_len: usize) -> Self {
        Self {
            anchor: TextCursor::from_parley(selection.anchor(), display_len),
            focus: TextCursor::from_parley(selection.focus(), display_len),
            h_pos: None,
        }
    }

    fn to_parley(self, layout: &Layout<ColorBrush>) -> ParleySelection {
        ParleySelection::new(self.anchor.to_parley(layout), self.focus.to_parley(layout))
    }

    fn normalized(self, display_len: usize) -> Self {
        Self {
            anchor: self.anchor.normalized(display_len),
            focus: self.focus.normalized(display_len),
            h_pos: self.h_pos,
        }
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
            A11yRole::TextInput | A11yRole::MultilineTextInput
        ) {
            return Err(SmolStr::new_static(
                "editable role must be TextInput or MultilineTextInput",
            ));
        }
        if self.multiline != (self.role == A11yRole::MultilineTextInput) {
            return Err(SmolStr::new_static(
                "editable role does not match its multiline flag",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKind {
    Insert,
    DeleteBackward,
    DeleteForward,
    Replace,
    Paste,
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
        EditKind::Replace | EditKind::Paste => false,
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

    pub(crate) fn display_text(&self) -> &str {
        match &self.display {
            DisplayText::Identity => &self.committed,
            DisplayText::Masked(masked) => &masked.text,
        }
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
        }
    }

    #[cfg(test)]
    pub(crate) fn display_byte_for_raw(&self, raw_byte: usize) -> usize {
        match &self.display {
            DisplayText::Identity => raw_byte.min(self.committed.len()),
            DisplayText::Masked(masked) => masked.display_byte_for_raw(&self.committed, raw_byte),
        }
    }

    fn caret_display_byte_for_raw(&self, raw_byte: usize) -> usize {
        let raw_byte = raw_byte.min(self.committed.len());
        match &self.display {
            DisplayText::Identity => snap_caret_forward(&self.committed, raw_byte),
            DisplayText::Masked(masked) => masked.display_byte_after_raw(&self.committed, raw_byte),
        }
    }

    fn build_snapshot(&self) -> TextEditSnapshot {
        TextEditSnapshot {
            generation: self.generation,
            text: Arc::clone(&self.committed),
            selection: self.selection,
            composing: false,
            focused: false,
            can_undo: self.undo.undo_len() > 0,
            can_redo: self.undo.redo_len() > 0,
        }
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.snapshot = self.build_snapshot();
    }

    fn install_committed(&mut self, next: String, text_changed: bool) -> bool {
        self.committed = Arc::from(next);
        let display = if self.spec.masked {
            DisplayText::Masked(MaskedDisplay::new(&self.committed))
        } else {
            DisplayText::Identity
        };
        let display_changed = match (&self.display, &display) {
            (DisplayText::Identity, DisplayText::Identity) => text_changed,
            (DisplayText::Masked(previous), DisplayText::Masked(next_display)) => {
                previous.text.len() != next_display.text.len()
            }
            (DisplayText::Identity, DisplayText::Masked(_))
            | (DisplayText::Masked(_), DisplayText::Identity) => true,
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
        if self.selection == selection {
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
        let generation = editable.generation;
        let (changed, display) = editable.assign_committed(truncated);
        let touched = changed || editable.generation != generation;
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
        }
    }
    if touched {
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

pub(crate) fn undo(app: &mut App, window: WindowId, node: NodeId) -> Result<bool, EngineError> {
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

fn commit_result(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    result: CommitResult,
) -> Result<bool, EngineError> {
    if !result.changed {
        return Ok(false);
    }
    {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        if let (Some(display), Some(style)) = (result.display, result.style) {
            tree.set_text_content(node, TextContent::new(display, style));
        }
        tree.mark_paint(node);
        tree.mark_a11y(node);
    }
    app.request_frame();
    Ok(true)
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
        }
        changed
    };
    if changed {
        app.request_frame();
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
    let Some((ancestor, _)) = scroll::scroll_ancestor(tree, node) else {
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

pub(crate) fn handle_text(app: &mut App, window: WindowId, text: &str) -> bool {
    let Some(context) = focused_editable(app, window) else {
        return false;
    };
    if context.readonly {
        return true;
    }
    let _ = insert_text(app, window, context.node, text, EditKind::Insert);
    true
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
    let shift = modifiers.contains(Modifiers::SHIFT);
    let alt_gr = modifiers.contains(Modifiers::CONTROL) && modifiers.contains(Modifiers::ALT);
    let rest = modifiers.difference(Modifiers::SHIFT);
    let plain = rest.is_empty();
    let command = !alt_gr && rest == command_modifier();
    let word = !alt_gr && rest == word_modifier();
    match code {
        KeyCode::Tab => false,
        KeyCode::Escape if modifiers.is_empty() => {
            let _ = collapse_selection(app, window, node);
            true
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
    submit: Option<Action>,
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
        DisplayText, EditableSpec, EditableText, MASK_GLYPH, TextAffinity, TextCursor,
        TextSelection, sanitize, snap_caret_forward, truncate_graphemes,
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
            "editable role must be TextInput or MultilineTextInput"
        );

        let mut role_mismatch = spec();
        role_mismatch.multiline = true;
        assert_eq!(
            role_mismatch.validate().unwrap_err(),
            "editable role does not match its multiline flag"
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
}
