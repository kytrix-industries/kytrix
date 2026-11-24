use std::{
    cell::{Cell, RefCell},
    fmt,
    ops::Range,
    sync::{Arc, OnceLock},
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::{Display, LengthPercentageAuto};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    event::{
        self, Action, ActionSource, Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers,
    },
    interaction::{ScrollAxis, ScrollConfig, ScrollHandle},
    layout::hit_test::HitResult,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::Signal,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, Quad, QuadPaint},
        style::{Cursor, StylePatch},
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextAlign, TextOverflow, TextStyle, TextWrap},
    window::{Window, WindowId},
};

use crate::{
    kit::{button::Button, icon::IconKind, size::UnifiedSizable},
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};

const BULLET: char = '•';
const CORNER_RADIUS: f32 = 4.0;
const UNDO_HISTORY_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct InputSpec {
    pub name: SmolStr,
    pub placeholder: SmolStr,
    pub masked: bool,
    pub multiline: bool,
    pub readonly: bool,
    pub disabled: bool,
    pub max_length: Option<usize>,
    pub size: ControlSize,
    pub submit: Option<Action>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    Empty,
    TooLong { max: usize },
    Invalid { i18n_key: SmolStr },
}

impl InputError {
    #[must_use]
    pub fn i18n_key(&self) -> &str {
        match self {
            Self::Empty => "input.error.empty",
            Self::TooLong { .. } => "input.error.tooLong",
            Self::Invalid { i18n_key } => i18n_key.as_str(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaretMotion {
    CharLeft,
    CharRight,
    Home,
    End,
    DocStart,
    DocEnd,
}

#[derive(Clone, Debug, PartialEq)]
struct Composition {
    start: usize,
    text: String,
    caret_utf16: Option<u32>,
    before: EditSnapshot,
}

#[derive(Clone, Debug, PartialEq)]
struct EditSnapshot {
    text: String,
    caret: usize,
    anchor: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputState {
    text: String,
    caret: usize,
    anchor: usize,
    composition: Option<Composition>,
    focused: bool,
    error: Option<InputError>,
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
    dragging: bool,
    drag_origin: Option<LogicalPoint>,
}

impl InputState {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            caret: end,
            anchor: end,
            composition: None,
            focused: false,
            error: None,
            undo: Vec::new(),
            redo: Vec::new(),
            dragging: false,
            drag_origin: None,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn display_text(&self, masked: bool) -> SmolStr {
        let unmasked = self.display_unmasked();
        if !masked {
            return unmasked;
        }
        let chars = unmasked.chars().count();
        let mut bullets = String::with_capacity(chars * BULLET.len_utf8());
        bullets.extend(std::iter::repeat_n(BULLET, chars));
        SmolStr::new(bullets)
    }

    #[must_use]
    pub fn caret(&self) -> usize {
        self.display_caret_byte(false)
    }

    #[must_use]
    pub fn selection(&self) -> Range<usize> {
        if self.composition.is_some() {
            let start = self.composition.as_ref().map_or(0, |comp| comp.start);
            return start..start;
        }
        self.selection_range()
    }

    #[must_use]
    pub fn selection_anchor(&self) -> usize {
        match &self.composition {
            Some(comp) => comp.start,
            None => self.anchor,
        }
    }

    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    #[must_use]
    pub fn error(&self) -> Option<&InputError> {
        self.error.as_ref()
    }

    pub fn insert(&mut self, spec: &InputSpec, text: &str) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let before = self
            .composition
            .as_ref()
            .map_or_else(|| self.snapshot(), |composition| composition.before.clone());
        let mut changed = self.commit_composition(spec);
        changed |= self.replace_selection_with(spec, text);
        self.finish_edit(before, changed);
        changed
    }

    pub fn delete_backward(&mut self, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        if self.cancel_composition() {
            return true;
        }
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start != end {
            let before = self.snapshot();
            let changed = self.delete_selection();
            self.finish_edit(before, changed);
            return changed;
        }
        let caret = snap_char_boundary(&self.text, self.caret);
        self.caret = caret;
        self.anchor = caret;
        let Some(prev) = prev_char_boundary(&self.text, caret) else {
            return false;
        };
        let before = self.snapshot();
        self.text.replace_range(prev..caret, "");
        self.caret = prev;
        self.anchor = prev;
        self.error = None;
        self.finish_edit(before, true);
        true
    }

    pub fn delete_forward(&mut self, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        if self.cancel_composition() {
            return true;
        }
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start != end {
            let before = self.snapshot();
            let changed = self.delete_selection();
            self.finish_edit(before, changed);
            return changed;
        }
        let caret = snap_char_boundary(&self.text, self.caret);
        self.caret = caret;
        self.anchor = caret;
        let Some(next) = next_char_boundary(&self.text, caret) else {
            return false;
        };
        let before = self.snapshot();
        self.text.replace_range(caret..next, "");
        self.error = None;
        self.finish_edit(before, true);
        true
    }

    pub fn insert_newline(&mut self, spec: &InputSpec) -> bool {
        if !spec.multiline {
            return false;
        }
        self.insert(spec, "\n")
    }

    pub fn select_all(&mut self) {
        self.cancel_composition();
        self.anchor = 0;
        self.caret = self.text.len();
    }

    pub fn set_caret(&mut self, byte: usize, extend: bool) {
        self.cancel_composition();
        let byte = snap_char_boundary(&self.text, byte);
        if extend {
            self.caret = byte;
        } else {
            self.anchor = byte;
            self.caret = byte;
        }
    }

    pub fn move_caret(&mut self, motion: CaretMotion, extend: bool) {
        self.cancel_composition();
        let caret = snap_char_boundary(&self.text, self.caret);
        let target = match motion {
            CaretMotion::CharLeft => prev_char_boundary(&self.text, caret).unwrap_or(0),
            CaretMotion::CharRight => {
                next_char_boundary(&self.text, caret).unwrap_or(self.text.len())
            }
            CaretMotion::Home => self.text[..caret].rfind('\n').map_or(0, |index| index + 1),
            CaretMotion::End => self.text[caret..]
                .find('\n')
                .map_or(self.text.len(), |index| caret + index),
            CaretMotion::DocStart => 0,
            CaretMotion::DocEnd => self.text.len(),
        };
        if extend {
            self.caret = target;
        } else {
            self.anchor = target;
            self.caret = target;
        }
    }

    pub fn ime_preedit(&mut self, spec: &InputSpec, text: &str, caret_utf16: Option<u32>) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let started = if self.composition.is_none() {
            if !self.ime_start_inner(spec) {
                return false;
            }
            true
        } else {
            false
        };
        let Some(composition) = self.composition.as_mut() else {
            return false;
        };
        let previous_text = composition.text.clone();
        let previous_caret = composition.caret_utf16;
        let units = utf16_len(text) as u32;
        composition.text.clear();
        composition.text.push_str(text);
        composition.caret_utf16 = caret_utf16.map(|caret| caret.min(units));
        started || previous_text != composition.text || previous_caret != composition.caret_utf16
    }

    pub fn ime_commit(&mut self, spec: &InputSpec, text: &str) -> bool {
        if spec.disabled || spec.readonly {
            self.ime_cancel();
            return false;
        }
        let Some(composition) = self.composition.take() else {
            return self.insert(spec, text);
        };
        let before = composition.before;
        self.caret = composition.start;
        self.anchor = composition.start;
        let changed = self.replace_selection_with(spec, text);
        self.finish_edit(before, changed);
        true
    }

    pub fn ime_cancel(&mut self) {
        self.cancel_composition();
    }

    pub fn copy(&self, app: &App, spec: &InputSpec) -> bool {
        if spec.disabled || spec.masked {
            return false;
        }
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start == end {
            return false;
        }
        app.set_clipboard_text(&self.text[start..end]).is_ok()
    }

    pub fn cut(&mut self, app: &App, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly || spec.masked {
            return false;
        }
        if !self.copy(app, spec) {
            return false;
        }
        let before = self.snapshot();
        let changed = self.delete_selection();
        self.finish_edit(before, changed);
        changed
    }

    pub fn paste(&mut self, app: &App, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let Ok(text) = app.clipboard_text() else {
            return false;
        };
        self.insert(spec, text.as_str())
    }

    pub fn set_focused(&mut self, focused: bool) {
        if !focused {
            self.cancel_composition();
        }
        self.focused = focused;
    }

    pub fn validate(&mut self, spec: &InputSpec) -> Option<InputError> {
        let error = if self.text.is_empty() {
            Some(InputError::Empty)
        } else {
            spec.max_length
                .filter(|max| self.text.chars().count() > *max)
                .map(|max| InputError::TooLong { max })
        };
        self.error = error.clone();
        error
    }

    pub fn set_error(&mut self, error: Option<InputError>) {
        self.error = error;
    }

    pub fn set_text(&mut self, spec: &InputSpec, text: &str) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        self.replace_text(spec, text)
    }

    fn replace_text(&mut self, spec: &InputSpec, text: &str) -> bool {
        let before = self.snapshot();
        let sanitized = clean_single_line(text, spec.multiline);
        let total = sanitized.chars().count();
        let clipped: String = match spec.max_length {
            Some(max) if total > max => sanitized.chars().take(max).collect(),
            _ => sanitized,
        };
        let too_long = clipped.chars().count() < total;
        let error = spec
            .max_length
            .filter(|_| too_long)
            .map(|max| InputError::TooLong { max });
        let changed = self.text != clipped
            || self.caret != clipped.len()
            || self.anchor != clipped.len()
            || self.composition.is_some()
            || (self.error.is_some() && error.is_none());
        if changed {
            self.text = clipped;
            self.caret = self.text.len();
            self.anchor = self.caret;
            self.composition = None;
            self.error = error;
            self.finish_edit(before, true);
        }
        changed
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        self.cancel_composition();
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.redo.push(current);
        self.restore(previous);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.cancel_composition();
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.undo.push(current);
        self.restore(next);
        true
    }

    pub fn select_word(&mut self) -> bool {
        self.cancel_composition();
        let caret = snap_char_boundary(&self.text, self.caret);
        let Some(range) = word_range_at(&self.text, caret) else {
            return false;
        };
        let (start, end) = (range.start, range.end);
        if self.anchor == start && self.caret == end {
            return false;
        }
        self.anchor = start;
        self.caret = end;
        true
    }

    pub fn select_line(&mut self) -> bool {
        self.cancel_composition();
        let caret = snap_char_boundary(&self.text, self.caret);
        let start = self.text[..caret].rfind('\n').map_or(0, |index| index + 1);
        let end = self.text[caret..]
            .find('\n')
            .map_or(self.text.len(), |index| caret + index);
        if self.anchor == start && self.caret == end {
            return false;
        }
        self.anchor = start;
        self.caret = end;
        true
    }

    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn begin_drag(&mut self, origin_caret: usize, origin: LogicalPoint) -> bool {
        self.cancel_composition();
        let origin_caret = snap_char_boundary(&self.text, origin_caret);
        self.dragging = true;
        self.drag_origin = Some(origin);
        self.anchor = origin_caret;
        self.caret = origin_caret;
        self.error = None;
        true
    }

    pub fn end_drag(&mut self) -> bool {
        self.drag_origin = None;
        std::mem::take(&mut self.dragging)
    }

    pub fn drag_to(&mut self, caret: usize, extend: bool) -> bool {
        if !self.dragging {
            return false;
        }
        let before = self.caret;
        self.set_caret(caret, extend);
        self.caret != before
    }

    pub fn set_caret_from_point(
        &mut self,
        window: &Window,
        text_node: NodeId,
        local_point: LogicalPoint,
        extend: bool,
    ) -> bool {
        let Ok(display_byte) = window.text_byte_index_at(text_node, local_point) else {
            return false;
        };
        let Ok(node) = window.tree().get(text_node) else {
            return false;
        };
        let NodeKind::Text(content) = node.kind() else {
            return false;
        };
        let display = content.text.as_str();
        let display_byte = snap_char_boundary(display, display_byte);
        let char_index = display[..display_byte].chars().count();
        let target = self.committed_offset_for_display_char(char_index);
        let composition_dropped = self.composition.take().is_some();
        let moved = if extend {
            if self.caret == target {
                false
            } else {
                self.caret = target;
                true
            }
        } else if self.anchor == target && self.caret == target {
            false
        } else {
            self.anchor = target;
            self.caret = target;
            true
        };
        let changed = composition_dropped || moved;
        if changed {
            self.error = None;
        }
        changed
    }

    pub fn handle_key(
        &mut self,
        app: &mut App,
        window: WindowId,
        spec: &InputSpec,
        code: KeyCode,
        modifiers: Modifiers,
        repeat: bool,
    ) -> bool {
        if spec.disabled || !self.focused {
            return false;
        }
        if self.composition.is_some() {
            if matches!(code, KeyCode::Escape) {
                self.ime_cancel();
                return true;
            }
            return false;
        }
        let command = modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::META);
        let extend = modifiers.contains(Modifiers::SHIFT);
        if repeat && (command || matches!(code, KeyCode::Enter | KeyCode::NumpadEnter)) {
            return false;
        }
        match code {
            KeyCode::KeyA if command => {
                self.select_all();
                true
            }
            KeyCode::KeyC if command => {
                self.copy(app, spec);
                true
            }
            KeyCode::KeyX if command => {
                self.cut(app, spec);
                true
            }
            KeyCode::KeyV if command => {
                self.paste(app, spec);
                true
            }
            KeyCode::KeyZ if command => {
                if extend {
                    self.redo();
                } else {
                    self.undo();
                }
                true
            }
            KeyCode::KeyY if command => {
                self.redo();
                true
            }
            KeyCode::Backspace if !command => {
                self.delete_backward(spec);
                true
            }
            KeyCode::Delete if !command => {
                self.delete_forward(spec);
                true
            }
            KeyCode::ArrowLeft if !command => {
                self.move_caret(CaretMotion::CharLeft, extend);
                true
            }
            KeyCode::ArrowRight if !command => {
                self.move_caret(CaretMotion::CharRight, extend);
                true
            }
            KeyCode::Home => {
                let motion = if command {
                    CaretMotion::DocStart
                } else {
                    CaretMotion::Home
                };
                self.move_caret(motion, extend);
                true
            }
            KeyCode::End => {
                let motion = if command {
                    CaretMotion::DocEnd
                } else {
                    CaretMotion::End
                };
                self.move_caret(motion, extend);
                true
            }
            KeyCode::Escape => {
                let selection = self.selection_range();
                if selection.is_empty() {
                    return false;
                }
                self.set_caret(selection.start, false);
                true
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if spec.multiline {
                    self.insert_newline(spec);
                } else if let Some(submit) = &spec.submit {
                    app.dispatch_action_focused(window, submit.clone(), ActionSource::Key);
                }
                true
            }
            _ => false,
        }
    }

    pub fn handle_ime(
        &mut self,
        app: &mut App,
        window: WindowId,
        spec: &InputSpec,
        event: &ImeEvent,
    ) -> bool {
        if spec.disabled || spec.readonly {
            return false;
        }
        let changed = match event {
            ImeEvent::Start => self.ime_start_inner(spec),
            ImeEvent::Preedit {
                text,
                caret,
                replace,
                ..
            } => {
                let (full, caret_full) = self.preedit_replaced(text, *caret, replace.as_ref());
                self.ime_preedit(spec, &full, caret_full)
            }
            ImeEvent::Commit { text, replace } => match replace {
                Some(range) if self.composition.is_some() => {
                    let (full, _) = self.preedit_replaced(text, None, Some(range));
                    self.ime_commit(spec, &full)
                }
                _ => self.ime_commit(spec, text),
            },
            ImeEvent::End => {
                let composing = self.composition.is_some();
                if composing {
                    self.ime_cancel();
                }
                composing
            }
        };
        if changed {
            let _ = app.request_repaint(window);
        }
        true
    }

    fn display_unmasked(&self) -> SmolStr {
        match &self.composition {
            Some(composition) => format_smolstr!(
                "{}{}{}",
                &self.text[..composition.start],
                composition.text,
                &self.text[composition.start..]
            ),
            None => SmolStr::new(self.text.as_str()),
        }
    }

    fn preedit_caret_utf8(&self) -> usize {
        let Some(composition) = &self.composition else {
            return 0;
        };
        let units = composition.caret_utf16.map_or_else(
            || utf16_len(&composition.text) as u16,
            |caret| u16::try_from(caret).unwrap_or(u16::MAX),
        );
        utf8_from_utf16_offset(&composition.text, units)
    }

    fn display_offset(&self, committed: usize) -> usize {
        match &self.composition {
            Some(composition) if committed > composition.start => {
                committed + composition.text.len()
            }
            _ => committed,
        }
    }

    fn display_caret_byte(&self, masked: bool) -> usize {
        let unmasked = match &self.composition {
            Some(composition) => composition.start + self.preedit_caret_utf8(),
            None => self.caret,
        };
        if masked {
            masked_offset(&self.display_unmasked(), unmasked)
        } else {
            unmasked
        }
    }

    fn display_selection(&self, masked: bool) -> Range<usize> {
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        let display_start = self.display_offset(start);
        let display_end = self.display_offset(end);
        if masked {
            let display = self.display_unmasked();
            masked_offset(&display, display_start)..masked_offset(&display, display_end)
        } else {
            display_start..display_end
        }
    }

    fn selection_range(&self) -> Range<usize> {
        if self.anchor <= self.caret {
            self.anchor..self.caret
        } else {
            self.caret..self.anchor
        }
    }

    fn snapshot(&self) -> EditSnapshot {
        match &self.composition {
            Some(composition) => EditSnapshot {
                text: self.text.clone(),
                caret: composition.start,
                anchor: composition.start,
            },
            None => EditSnapshot {
                text: self.text.clone(),
                caret: snap_char_boundary(&self.text, self.caret),
                anchor: snap_char_boundary(&self.text, self.anchor),
            },
        }
    }

    fn restore(&mut self, snapshot: EditSnapshot) {
        self.text = snapshot.text;
        self.caret = snap_char_boundary(&self.text, snapshot.caret);
        self.anchor = snap_char_boundary(&self.text, snapshot.anchor);
        self.composition = None;
        self.error = None;
    }

    fn finish_edit(&mut self, before: EditSnapshot, changed: bool) {
        if !changed {
            return;
        }
        if self.undo.len() >= UNDO_HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(before);
        self.redo.clear();
    }

    fn delete_selection(&mut self) -> bool {
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        if start == end {
            return false;
        }
        self.text.replace_range(start..end, "");
        self.caret = start;
        self.anchor = start;
        self.error = None;
        true
    }

    fn replace_selection_with(&mut self, spec: &InputSpec, text: &str) -> bool {
        let selection = self.selection_range();
        let (start, end) = (selection.start, selection.end);
        let selected = self.text[start..end].chars().count();
        let total = self.text.chars().count();
        let budget = spec
            .max_length
            .map_or(usize::MAX, |max| max.saturating_sub(total - selected));
        let cleaned = clean_single_line(text, spec.multiline);
        let clipped: String = cleaned.chars().take(budget).collect();
        if clipped.is_empty() && start == end {
            return false;
        }
        self.text.replace_range(start..end, &clipped);
        let caret = start + clipped.len();
        self.caret = caret;
        self.anchor = caret;
        self.error = None;
        true
    }

    fn commit_composition(&mut self, spec: &InputSpec) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        self.caret = composition.start;
        self.anchor = composition.start;
        self.replace_selection_with(spec, &composition.text);
        true
    }

    fn ime_start_inner(&mut self, spec: &InputSpec) -> bool {
        if spec.disabled || spec.readonly || self.composition.is_some() {
            return false;
        }
        let before = self.snapshot();
        self.delete_selection();
        let start = snap_char_boundary(&self.text, self.caret);
        self.caret = start;
        self.anchor = start;
        self.composition = Some(Composition {
            start,
            text: String::new(),
            caret_utf16: None,
            before,
        });
        true
    }

    fn cancel_composition(&mut self) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        self.caret = composition.start;
        self.anchor = composition.start;
        true
    }

    fn preedit_replaced(
        &self,
        fragment: &str,
        caret_fragment: Option<u32>,
        replace: Option<&Range<usize>>,
    ) -> (String, Option<u32>) {
        let current = self
            .composition
            .as_ref()
            .map_or("", |composition| composition.text.as_str());
        let total = utf16_len(current);
        let (start_units, end_units) = match replace {
            Some(range) => (range.start.min(total), range.end.min(total)),
            None => (0, total),
        };
        let start_units = start_units.min(end_units);
        let end_units = end_units.max(start_units);
        let start_byte = utf8_from_utf16_units(current, start_units);
        let end_byte = utf8_from_utf16_units(current, end_units);
        let start_byte = start_byte.min(end_byte);
        let end_byte = end_byte.max(start_byte);
        let mut full =
            String::with_capacity(start_byte + fragment.len() + current.len() - end_byte);
        full.push_str(&current[..start_byte]);
        full.push_str(fragment);
        full.push_str(&current[end_byte..]);
        let caret_full = caret_fragment.map(|caret| {
            let fragment_units = utf16_len(fragment) as u32;
            let local = caret.min(fragment_units) as usize;
            (utf16_len(&current[..start_byte]) + local) as u32
        });
        (full, caret_full)
    }

    fn committed_offset_for_display_char(&self, char_index: usize) -> usize {
        let Some(composition) = &self.composition else {
            return byte_at_char(&self.text, char_index);
        };
        let prefix = &self.text[..composition.start];
        let prefix_chars = prefix.chars().count();
        if char_index <= prefix_chars {
            return byte_at_char(prefix, char_index);
        }
        let preedit_chars = composition.text.chars().count();
        if char_index < prefix_chars + preedit_chars {
            return composition.start;
        }
        byte_at_char(&self.text, char_index - preedit_chars)
    }
}

#[derive(Clone)]
pub struct InputNode {
    pub root: NodeId,
    pub viewport: Option<NodeId>,
    pub text: NodeId,
    pub placeholder: NodeId,
    pub caret: NodeId,
    pub state: Signal<InputState>,
    pub placeholder_state: Signal<SmolStr>,
    pub spec: Arc<InputSpec>,
    scroll: Arc<OnceLock<ScrollHandle>>,
    theme: ThemeHandle,
    clear: RefCell<Option<Button>>,
    submit_on_blur: Cell<bool>,
}

impl fmt::Debug for InputNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputNode")
            .field("name", &self.spec.name)
            .field("root", &self.root)
            .field("text", &self.text)
            .field("placeholder", &self.placeholder)
            .field("caret", &self.caret)
            .field("clearable", &self.clear.borrow().is_some())
            .field("submit_on_blur", &self.submit_on_blur.get())
            .finish()
    }
}

#[must_use]
pub fn build(
    tree: &mut NodeTree,
    parent: NodeId,
    spec: Arc<InputSpec>,
    theme: &ThemeHandle,
    state: &Signal<InputState>,
    placeholder_text: &Signal<SmolStr>,
) -> InputNode {
    let tokens = theme.tokens();
    let size = tokens.for_size(spec.size);
    let border_width = tokens.border_width;
    let disabled = spec.disabled;
    let readonly = spec.readonly;
    let masked = spec.masked;
    let multiline = spec.multiline;
    let name = spec.name.clone();
    let padding_x = size.padding_x;
    let padding_y = if multiline {
        size.padding_x
    } else {
        LogicalPx::ZERO
    };

    let mut root_style = StyleBuilder::new()
        .flex_row()
        .items_center()
        .relative()
        .width_full()
        .padding_x(padding_x)
        .padding_y(padding_y)
        .border(border_width)
        .overflow_hidden()
        .build();
    if multiline {
        root_style
            .min_height(LengthPercentageAuto::length(size.height.value()))
            .align_self(taffy::style::AlignSelf::START);
    } else {
        root_style.height_length(size.height);
    }
    let root_paint = PaintBuilder::new()
        .border(border_width, Color::TRANSPARENT)
        .rounded(px(CORNER_RADIUS))
        .build();

    let style_state = *state;
    let a11y_state = *state;
    let a11y_placeholder = *placeholder_text;
    let style_theme = theme.clone();
    let mut root_builder = NodeBuilder::boxed()
        .style(root_style)
        .paint(root_paint)
        .id(format_smolstr!("{name}.root"))
        .tab_stop(true)
        .a11y(A11yOverride {
            role: Some(if multiline {
                A11yRole::MultilineTextInput
            } else {
                A11yRole::TextInput
            }),
            label: Some(name.clone()),
            disabled,
            ..A11yOverride::default()
        })
        .bind_style(move |cx| {
            let state = cx.read(&style_state);
            let colors = style_theme.components(cx).input;
            let border_color = if disabled {
                colors.border.disabled
            } else if state.error().is_some() {
                style_theme.role(cx, Role::StatusDangerBorder)
            } else if state.is_focused() {
                colors.border.focused
            } else {
                colors.border.normal
            };
            StylePatch {
                background: Some(if disabled {
                    colors.background.disabled
                } else {
                    colors.background.enabled
                }),
                border: Some(Border {
                    width: border_width,
                    color: border_color,
                }),
                interactive: Some(!disabled),
                cursor: Some(if disabled {
                    Cursor::NotAllowed
                } else {
                    Cursor::Text
                }),
                ..StylePatch::default()
            }
        })
        .bind_a11y(move |cx| {
            let state = cx.read(&a11y_state);
            let placeholder = cx.read(&a11y_placeholder).clone();
            A11yPatch {
                value: Some(state.display_text(masked)),
                disabled: Some(disabled),
                read_only: Some(readonly),
                invalid: Some(state.error().is_some()),
                placeholder: Some(placeholder),
                ..A11yPatch::default()
            }
        });
    if !disabled {
        root_builder = root_builder
            .on_click(click_action(&spec))
            .on_drag(drag_action(&spec));
    }
    let root = root_builder
        .build(tree, parent)
        .expect("input: root node mounts under a live parent");

    let viewport = if multiline {
        None
    } else {
        let viewport = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .height_full()
                    .flex_1()
                    .min_width_0()
                    .relative()
                    .overflow_hidden()
                    .build(),
            )
            .id(format_smolstr!("{name}.viewport"))
            .tab_stop(false)
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, root)
            .expect("input: viewport mounts under the fresh root");
        Some(viewport)
    };

    let text_state = *state;
    let text_style = TextStyle::new(size.font_size)
        .with_wrap(if multiline {
            TextWrap::Wrap
        } else {
            TextWrap::NoWrap
        })
        .with_alignment(TextAlign::Start)
        .with_overflow(TextOverflow::Clip, 0);
    let text_theme = theme.clone();
    let text_parent = viewport.unwrap_or(root);
    let mut text_builder = NodeBuilder::text(SmolStr::new_static(""), text_style)
        .bind_text(move |cx| cx.read(&text_state).display_text(masked))
        .bind_style(move |cx| {
            let foreground = text_theme.components(cx).input.foreground;
            StylePatch {
                foreground: Some(if disabled {
                    foreground.disabled
                } else {
                    foreground.enabled
                }),
                ..StylePatch::default()
            }
        })
        .id(format_smolstr!("{name}.text"))
        .tab_stop(false)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        });
    text_builder = if multiline {
        text_builder.style(StyleBuilder::new().flex_1().min_width_0().build())
    } else {
        text_builder.style(StyleBuilder::new().flex_none().build())
    };
    let text = text_builder
        .build(tree, text_parent)
        .expect("input: text node mounts under the fresh parent");

    let placeholder_state = *placeholder_text;
    let placeholder_style_state = *state;
    let placeholder_theme = theme.clone();
    let placeholder_text_style = TextStyle::new(size.font_size)
        .with_wrap(if multiline {
            TextWrap::Wrap
        } else {
            TextWrap::NoWrap
        })
        .with_alignment(TextAlign::Start)
        .with_overflow(TextOverflow::Clip, 0);
    let placeholder = NodeBuilder::text(SmolStr::new_static(""), placeholder_text_style)
        .style(
            StyleBuilder::new()
                .absolute()
                .top(padding_y)
                .left(padding_x)
                .right(padding_x)
                .build(),
        )
        .bind_text(move |cx| cx.read(&placeholder_state).clone())
        .bind_style(move |cx| {
            let state = cx.read(&placeholder_style_state);
            let empty = state.text().is_empty() && !state.is_composing();
            StylePatch {
                foreground: Some(placeholder_theme.components(cx).input.placeholder),
                opacity: Some(if empty { 1.0 } else { 0.0 }),
                interactive: Some(false),
                ..StylePatch::default()
            }
        })
        .id(format_smolstr!("{name}.placeholder"))
        .tab_stop(false)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, root)
        .expect("input: placeholder node mounts under the fresh root");

    let caret_theme = theme.clone();
    let caret = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
        .style(
            StyleBuilder::new()
                .absolute()
                .top(padding_y)
                .left(padding_x)
                .right(padding_x)
                .bottom(padding_y)
                .build(),
        )
        .bind_style(move |cx| {
            let _ = caret_theme.appearance(cx);
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            }
        })
        .id(format_smolstr!("{name}.caret"))
        .tab_stop(false)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, root)
        .expect("input: caret canvas mounts under the fresh root");

    InputNode {
        root,
        viewport,
        text,
        placeholder,
        caret,
        state: *state,
        placeholder_state: *placeholder_text,
        spec,
        scroll: Arc::new(OnceLock::new()),
        theme: theme.clone(),
        clear: RefCell::new(None),
        submit_on_blur: Cell::new(false),
    }
}

impl InputNode {
    pub fn install(&self, app: &mut App) {
        if self.spec.disabled {
            return;
        }
        if let (Some(viewport), Some(window)) = (self.viewport, self.root.window())
            && let Ok(handle) = app.make_scroll(
                window,
                viewport,
                ScrollConfig {
                    axis: ScrollAxis::Horizontal,
                    line_height: px(1.0),
                },
            )
        {
            let _ = self.scroll.set(handle);
        }
        let state = self.state;
        let spec = Arc::clone(&self.spec);
        let root = self.root;
        let text = self.text;
        app.on_action(click_action(&spec), move |ctx| {
            let Some(hit) = ctx.hit() else {
                return;
            };
            let window = ctx.window();
            let extend = ctx.modifiers().contains(Modifiers::SHIFT);
            let click_count = match ctx.source() {
                ActionSource::Click { click_count, .. } => click_count,
                _ => 1,
            };
            let app = ctx.app_mut();
            let Some(window_ref) = app.window(window) else {
                return;
            };
            let Some(local) = text_local_point(window_ref.tree(), hit, text) else {
                return;
            };
            if app.focused(window) != Some(root) {
                app.set_focus(window, Some(root));
            }
            let before = app.read(&state).clone();
            let mut next = before.clone();
            next.set_focused(true);
            let Some(window_ref) = app.window(window) else {
                return;
            };
            next.set_caret_from_point(window_ref, text, local, extend);
            if click_count >= 3 {
                next.select_line();
            } else if click_count == 2 {
                next.select_word();
            }
            if next != before {
                app.write(&state, next);
            }
            let _ = app.request_repaint(window);
        });

        let state = self.state;
        let spec = Arc::clone(&self.spec);
        let text = self.text;
        let scroll_handle = self.scroll.get().copied();
        app.on_action(drag_action(&spec), move |ctx| {
            let window = ctx.window();
            let hit = ctx.hit();
            let (phase, delta) = match ctx.source() {
                ActionSource::Drag { phase, delta } => (phase, delta),
                _ => return,
            };
            let app = ctx.app_mut();
            let scroll = scroll_handle
                .and_then(|handle| app.scroll_state(&handle))
                .map_or(0.0, |state| f32::from(state.offset.x));
            let text_origin = {
                let Some(window_ref) = app.window(window) else {
                    return;
                };
                let Ok(node) = window_ref.tree().get(text) else {
                    return;
                };
                node.layout().bounds.origin
            };
            let to_text_local = |point: LogicalPoint| {
                LogicalPoint::new(point.x - text_origin.x + scroll, point.y - text_origin.y)
            };
            let before = app.read(&state).clone();
            let mut next = before.clone();
            if phase == ui_core::interaction::DragPhase::Cancel {
                next.end_drag();
                if next != before {
                    app.write(&state, next);
                }
                return;
            }
            let info = app.drag_state(window).and_then(|drag| app.drag(&drag));
            if let Some(info) = info {
                match phase {
                    ui_core::interaction::DragPhase::Start => {
                        let down = to_text_local(info.origin);
                        next.set_focused(true);
                        let Some(window_ref) = app.window(window) else {
                            return;
                        };
                        if !next.set_caret_from_point(window_ref, text, down, false) {
                            return;
                        }
                        let anchor = next.caret();
                        next.begin_drag(anchor, down);
                        let Some(window_ref) = app.window(window) else {
                            return;
                        };
                        next.set_caret_from_point(
                            window_ref,
                            text,
                            to_text_local(info.current),
                            true,
                        );
                    }
                    ui_core::interaction::DragPhase::Move => {
                        if !next.is_dragging() {
                            return;
                        }
                        let Some(window_ref) = app.window(window) else {
                            return;
                        };
                        next.set_caret_from_point(
                            window_ref,
                            text,
                            to_text_local(info.current),
                            true,
                        );
                    }
                    ui_core::interaction::DragPhase::End => {}
                    ui_core::interaction::DragPhase::Cancel => unreachable!("Cancel 已提前返回"),
                }
            } else {
                if phase != ui_core::interaction::DragPhase::End || !next.is_dragging() {
                    return;
                }
                let current = hit
                    .and_then(|hit| {
                        let window_ref = app.window(window)?;
                        text_local_point(window_ref.tree(), hit, text)
                    })
                    .or_else(|| {
                        next.drag_origin
                            .map(|origin| LogicalPoint::new(origin.x + delta.x, origin.y + delta.y))
                    });
                if let Some(current) = current
                    && let Some(window_ref) = app.window(window)
                {
                    next.set_caret_from_point(window_ref, text, current, true);
                }
                next.end_drag();
            }
            if next != before {
                app.write(&state, next);
            }
        });
    }

    pub fn release(&self, app: &mut App) -> usize {
        let mut released = [click_action(&self.spec), drag_action(&self.spec)]
            .iter()
            .filter(|action| app.off_action(action))
            .count();
        if let Some(button) = self.clear.borrow_mut().take() {
            released += button.release(app);
        }
        released
    }

    pub fn update_value(&self, app: &mut App, value: &str) -> bool {
        let before = app.read(&self.state).clone();
        let mut next = before.clone();
        let changed = next.replace_text(&self.spec, value);
        next.undo.clear();
        next.redo.clear();
        if next == before {
            return false;
        }
        app.write(&self.state, next);
        if let Some(window) = self.root.window() {
            let _ = app.request_repaint(window);
        }
        changed
    }

    pub fn update_placeholder(&self, app: &mut App, placeholder: impl Into<SmolStr>) -> bool {
        let changed = app.write(&self.placeholder_state, placeholder.into());
        if changed && let Some(window) = self.root.window() {
            let _ = app.request_repaint(window);
        }
        changed
    }

    pub fn clear(&self, app: &mut App) -> bool {
        self.update_value(app, "")
    }

    pub fn set_clearable(&mut self, app: &mut App, clearable: bool) -> bool {
        if self.clear.borrow().is_some() == clearable {
            return false;
        }
        let Some(window) = self.root.window() else {
            return false;
        };
        if !clearable {
            if let Some(button) = self.clear.borrow_mut().take() {
                if let Some(node) = button.node()
                    && app
                        .window(window)
                        .is_some_and(|target| target.tree().get(node).is_ok())
                {
                    let _ = app.remove_node(window, node);
                }
                button.release(app);
            }
            return true;
        }
        let node = self.clone();
        let key = format_smolstr!("{}.clear", self.spec.name);
        let button = Button::new(app, self.theme.clone(), key)
            .icon(IconKind::X)
            .a11y_label(SmolStr::new_static("Clear"))
            .ghost()
            .small()
            .on_click(move |ctx| {
                node.clear(ctx.app_mut());
            });
        if button.build(app, window, self.root).is_err() {
            return false;
        }
        *self.clear.borrow_mut() = Some(button);
        self.sync_clear_button(app);
        true
    }

    pub fn set_submit_on_blur(&self, submit_on_blur: bool) -> bool {
        self.submit_on_blur.replace(submit_on_blur) != submit_on_blur
    }

    fn sync_clear_button(&self, app: &mut App) -> bool {
        let Some(button) = self.clear.borrow().clone() else {
            return false;
        };
        let Some(node) = button.node() else {
            return false;
        };
        let mut changed = button.set_disabled(app, self.spec.disabled);
        let display = if app.read(&self.state).text().is_empty() {
            Display::None
        } else {
            Display::Flex
        };
        let Some(window) = node.window() else {
            return changed;
        };
        let Some(target) = app.window_mut(window) else {
            return changed;
        };
        let tree = target.tree_mut();
        let Ok(current) = tree.get(node) else {
            return changed;
        };
        let mut style = current.style().clone();
        if style.display_mode() != display {
            style.display(display);
            let _ = tree.set_style(node, style);
            changed = true;
        }
        changed
    }

    pub fn sync(&self, app: &mut App) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        let Some(window_ref) = app.window(window) else {
            return false;
        };
        let focused = window_ref.focus().is_focused(self.root);
        let mut next = app.read(&self.state).clone();
        let mut state_changed = false;
        let was_focused = next.is_focused();
        if was_focused != focused {
            next.set_focused(focused);
            state_changed = true;
        }
        let blur_submit = was_focused
            && !focused
            && self.submit_on_blur.get()
            && !self.spec.multiline
            && self.spec.submit.is_some();
        let ime_target = focused && !self.spec.disabled && !self.spec.readonly;
        if ime_target {
            app.set_ime_enabled(window, true);
            let byte = next.display_caret_byte(self.spec.masked);
            if app.set_ime_cursor_area(window, self.text, byte).is_err() {}
        } else {
            app.clear_ime_cursor_area(window);
            if was_focused {
                app.set_ime_enabled(window, false);
            }
        }
        let scroll_changed = self.sync_horizontal_scroll(app, window, &next);
        let appearance = self.theme.current(app);
        let theme = self.theme.theme_for(appearance);
        let caret_color = theme.role(Role::FgDefault);
        let selection_color = theme.components().input.selection;
        let canvas = app.window(window).and_then(|window_ref| {
            self.caret_canvas(window_ref, &next, caret_color, selection_color)
        });
        if state_changed {
            app.write(&self.state, next);
        }
        let submitted = if blur_submit {
            if let Some(submit) = &self.spec.submit {
                app.dispatch_action(
                    window,
                    self.root,
                    submit.clone(),
                    ActionSource::Programmatic,
                )
            } else {
                false
            }
        } else {
            false
        };
        let canvas_changed = canvas.is_some_and(|canvas| self.apply_canvas(app, window, canvas));
        let clear_changed = self.sync_clear_button(app);
        state_changed || scroll_changed || canvas_changed || clear_changed || submitted
    }

    fn sync_horizontal_scroll(&self, app: &mut App, window: WindowId, state: &InputState) -> bool {
        let Some(handle) = self.scroll.get().copied() else {
            return false;
        };
        let Some(viewport) = self.viewport else {
            return false;
        };
        let byte = state.display_caret_byte(self.spec.masked);
        let geometry = {
            let Some(window_ref) = app.window(window) else {
                return false;
            };
            let tree = window_ref.tree();
            let (Ok(viewport_node), Ok(text_node)) = (tree.get(viewport), tree.get(self.text))
            else {
                return false;
            };
            let viewport_bounds = viewport_node.layout().bounds;
            let text_bounds = text_node.layout().bounds;
            let Ok(caret) = window_ref.text_caret_rect(self.text, byte) else {
                return false;
            };
            (
                text_bounds.size.width,
                text_bounds.origin.x,
                viewport_bounds.size.width,
                viewport_bounds.size.height,
                caret,
            )
        };
        let (content_width, text_origin_x, viewport_width, viewport_height, caret) = geometry;
        if viewport_width.value() <= 0.0 || !content_width.is_finite() {
            return false;
        }
        let _ = app.set_scroll_content(
            &handle,
            LogicalSize::new(
                content_width.max(viewport_width),
                viewport_height.max(px(1.0)),
            ),
        );
        let Some(current) = app.scroll_state(&handle) else {
            return false;
        };
        let max_offset = (content_width - viewport_width).max(px(0.0));
        let caret_left = caret.origin.x - text_origin_x;
        let caret_right = caret_left + caret.size.width.max(px(1.0));
        let mut next = current.offset.x;
        if caret_left < next {
            next = caret_left;
        }
        if caret_right > next + viewport_width {
            next = caret_right - viewport_width;
        }
        let next = next.value().clamp(0.0, max_offset.value());
        if (next - current.offset.x.value()).abs() <= f32::EPSILON {
            return false;
        }
        app.scroll_to(&handle, LogicalPoint::new(px(next), px(0.0)))
            .is_ok()
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        if app.focused(window) != Some(self.root) {
            return false;
        }
        let before = app.read(&self.state).clone();
        let mut next = before.clone();
        let consumed = match event {
            Event::Input(InputEvent::Text { text, .. }) => {
                if event::should_emit_text(KeyState::Down, false, false, next.is_composing()) {
                    next.insert(&self.spec, text);
                }
                true
            }
            Event::Input(InputEvent::Key {
                code,
                state: KeyState::Down,
                repeat,
                modifiers,
                ..
            }) => {
                next.set_focused(true);
                next.handle_key(app, window, &self.spec, *code, *modifiers, *repeat)
            }
            Event::Input(InputEvent::Key { .. }) => false,
            Event::Ime(ime) => {
                next.set_focused(true);
                next.handle_ime(app, window, &self.spec, ime)
            }
            _ => false,
        };
        if consumed {
            let current = app.read(&self.state).clone();
            if current == before && next != before {
                app.write(&self.state, next);
            }
        }
        consumed
    }

    fn caret_canvas(
        &self,
        window: &Window,
        state: &InputState,
        caret_color: Color,
        selection_color: Color,
    ) -> Option<Canvas> {
        if !window.focus().is_focused(self.root) {
            return Some(Canvas::new(LogicalRect::default()));
        }
        let masked = self.spec.masked;
        let caret_byte = state.display_caret_byte(masked);
        let caret_rect = window.text_caret_rect(self.text, caret_byte).ok()?;
        let local_zero = window.tree().text_caret_rect(self.text, 0).ok()?;
        let absolute_zero = window.text_caret_rect(self.text, 0).ok()?;
        let origin = LogicalPoint::new(
            absolute_zero.origin.x - local_zero.origin.x,
            absolute_zero.origin.y - local_zero.origin.y,
        );
        let selection = state.display_selection(masked);
        let mut quads: Vec<Quad> = Vec::with_capacity(4);
        if !selection.is_empty() {
            let rects = window
                .tree()
                .text_selection_rects(self.text, selection.start, selection.end)
                .ok()?;
            for rect in rects {
                quads.push(Quad {
                    bounds: rect.translate(origin),
                    corner_radius: [px(0.0); 4],
                    paint: QuadPaint::Solid {
                        fill: selection_color,
                        border: None,
                    },
                });
            }
        }
        quads.push(Quad {
            bounds: caret_rect,
            corner_radius: [px(0.0); 4],
            paint: QuadPaint::Solid {
                fill: caret_color,
                border: None,
            },
        });
        let bounds = quads.iter().fold(LogicalRect::default(), |bounds, quad| {
            bounds.union(quad.bounds)
        });
        let mut canvas = Canvas::new(bounds);
        for quad in quads {
            canvas.push_quad(quad);
        }
        Some(canvas)
    }

    fn apply_canvas(&self, app: &mut App, window: WindowId, canvas: Canvas) -> bool {
        let unchanged = app.window(window).is_some_and(|window_ref| {
            match window_ref.tree().get(self.caret) {
                Ok(node) => {
                    matches!(node.kind(), NodeKind::Canvas(existing) if existing.as_ref() == &canvas)
                }
                Err(_) => false,
            }
        });
        if unchanged {
            return false;
        }
        let applied = app
            .window_mut(window)
            .is_some_and(|window_ref| window_ref.set_canvas(self.caret, canvas).unwrap_or(false));
        if applied {
            let _ = app.request_repaint(window);
        }
        applied
    }
}

fn click_action(spec: &InputSpec) -> Action {
    Action::Custom(format_smolstr!("ui_kit.input.{}.click", spec.name))
}

fn drag_action(spec: &InputSpec) -> Action {
    Action::Custom(format_smolstr!("ui_kit.input.{}.drag", spec.name))
}

fn text_local_point(tree: &NodeTree, hit: HitResult, text: NodeId) -> Option<LogicalPoint> {
    let hit_node = tree.get(hit.node).ok()?;
    let text_node = tree.get(text).ok()?;
    let hit_bounds = hit_node.layout().bounds;
    let text_bounds = text_node.layout().bounds;
    Some(LogicalPoint::new(
        hit.local_point.x + hit_bounds.origin.x - text_bounds.origin.x,
        hit.local_point.y + hit_bounds.origin.y - text_bounds.origin.y,
    ))
}

fn masked_offset(unmasked_display: &str, byte: usize) -> usize {
    let byte = snap_char_boundary(unmasked_display, byte);
    unmasked_display[..byte].chars().count() * BULLET.len_utf8()
}

fn clean_single_line(text: &str, multiline: bool) -> String {
    if multiline {
        return text.to_string();
    }
    text.chars()
        .filter(|ch| !matches!(ch, '\r' | '\n' | '\t'))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordClass {
    Word,
    Space,
    Other,
}

fn word_class(ch: char) -> WordClass {
    if ch.is_alphanumeric() || ch == '_' {
        WordClass::Word
    } else if ch.is_whitespace() {
        WordClass::Space
    } else {
        WordClass::Other
    }
}

fn word_range_at(text: &str, caret: usize) -> Option<Range<usize>> {
    let anchor = if caret == text.len() && caret > 0 {
        prev_char_boundary(text, caret)?
    } else {
        caret
    };
    let anchor_char = text[anchor..].chars().next()?;
    let class = word_class(anchor_char);
    let mut start = anchor;
    while let Some(previous) = prev_char_boundary(text, start) {
        let Some(ch) = text[previous..].chars().next() else {
            break;
        };
        if word_class(ch) != class {
            break;
        }
        start = previous;
    }
    let mut end = anchor + anchor_char.len_utf8();
    while let Some(next) = next_char_boundary(text, end) {
        let Some(ch) = text[end..].chars().next() else {
            break;
        };
        if word_class(ch) != class {
            break;
        }
        end = next;
    }
    Some(start..end)
}

fn snap_char_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn prev_char_boundary(text: &str, byte: usize) -> Option<usize> {
    text[..byte]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
}

fn next_char_boundary(text: &str, byte: usize) -> Option<usize> {
    text[byte..].chars().next().map(|ch| byte + ch.len_utf8())
}

fn byte_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf8_from_utf16_offset(text: &str, offset: u16) -> usize {
    utf8_from_utf16_units(text, usize::from(offset))
}

fn utf8_from_utf16_units(text: &str, offset: usize) -> usize {
    let mut remaining = offset;
    for (byte, ch) in text.char_indices() {
        if remaining == 0 {
            return byte;
        }
        let units = ch.len_utf16();
        if remaining < units {
            return byte;
        }
        remaining -= units;
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, sync::Arc, time::Instant};

    use smol_str::SmolStr;
    use ui_core::{
        event::{
            Action, ActionSource, Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers,
            PointerButton,
        },
        interaction::ScrollHandle,
        math::{LogicalPoint, LogicalRect, px},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeKind},
        paint::{
            color::{Color, ColorResolver},
            primitive::Primitive,
            style::PaintSpec,
            style_builder::StyleBuilder,
        },
        test_support::{app::TestApp, record::PrimitiveKind},
        window::WindowId,
    };

    use super::{
        CaretMotion, InputError, InputNode, InputSpec, InputState, build, click_action,
        drag_action, utf8_from_utf16_offset, utf16_len,
    };
    use crate::theme::{
        appearance::Appearance, components::ControlSize, handle::ThemeHandle, resolved::Theme,
        role::Role,
    };

    fn spec() -> InputSpec {
        InputSpec {
            name: SmolStr::new_static("test"),
            placeholder: SmolStr::new_static("请输入"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Medium,
            submit: None,
        }
    }

    fn state(text: &str) -> InputState {
        InputState::new(text)
    }

    fn focused_state(text: &str) -> InputState {
        let mut state = InputState::new(text);
        state.set_focused(true);
        state
    }

    struct Harness {
        app: TestApp,
        node: InputNode,
        state: Signal<InputState>,
        placeholder: Signal<SmolStr>,
        theme: ThemeHandle,
    }

    fn harness(spec: InputSpec, initial: InputState) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let state = app.app_mut().signal(initial);
        let placeholder = app.app_mut().signal(SmolStr::new_static("请输入"));
        let spec = Arc::new(spec);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let node = build(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut(),
            root,
            Arc::clone(&spec),
            &theme,
            &state,
            &placeholder,
        );
        node.install(app.app_mut());
        app.drain().expect("首帧");
        Harness {
            app,
            node,
            state,
            placeholder,
            theme,
        }
    }

    impl Harness {
        fn window(&self) -> WindowId {
            self.app.window_id()
        }

        fn drain(&mut self) {
            self.app.drain().expect("帧");
        }

        fn focus(&mut self) {
            let window = self.window();
            let root = self.node.root;
            assert!(
                self.app.app_mut().set_focus(window, Some(root)),
                "根节点可聚焦"
            );
            let _ = self.node.sync(self.app.app_mut());
            self.drain();
        }

        fn sync(&mut self) {
            let _ = self.node.sync(self.app.app_mut());
        }

        fn pump(&mut self, event: Event) {
            self.app.dispatch_event(event);
            let window = self.window();
            let events = self.app.app_mut().drain_events();
            for (target, event) in events {
                assert_eq!(target, window);
                self.node.dispatch(self.app.app_mut(), &event);
            }
        }

        fn type_text(&mut self, text: &str) {
            self.pump(Event::Input(InputEvent::Text {
                text: SmolStr::new(text),
                timestamp: Instant::now(),
            }));
        }

        fn key(&mut self, code: KeyCode, modifiers: Modifiers) {
            self.pump(Event::Input(InputEvent::Key {
                code,
                state: KeyState::Down,
                repeat: false,
                modifiers,
                timestamp: Instant::now(),
            }));
        }

        fn key_repeat(&mut self, code: KeyCode, modifiers: Modifiers) {
            self.pump(Event::Input(InputEvent::Key {
                code,
                state: KeyState::Down,
                repeat: true,
                modifiers,
                timestamp: Instant::now(),
            }));
        }

        fn ime(&mut self, event: ImeEvent) {
            self.pump(Event::Ime(event));
        }

        fn state(&self) -> InputState {
            self.app.app().read(&self.state).clone()
        }

        fn write_state(&mut self, state: InputState) {
            self.app.app_mut().write(&self.state, state);
        }

        fn scroll_handle(&self) -> Option<ScrollHandle> {
            self.node.scroll.get().copied()
        }

        fn node_text(&self, node: NodeId) -> SmolStr {
            let window = self.window();
            let tree = self.app.app().window(window).expect("窗口存在").tree();
            match tree.get(node).expect("节点存在").kind() {
                NodeKind::Text(content) => content.text.clone(),
                other => panic!("expected text node, got {other:?}"),
            }
        }

        fn root_quad_count(&self) -> usize {
            let Some(frame) = self.app.frame_record().frame.as_ref() else {
                return 0;
            };
            frame
                .quads
                .iter()
                .filter(|quad| quad.node == self.node.root)
                .count()
        }

        fn caret_quads(&self) -> Vec<LogicalRect> {
            let window = self.window();
            let tree = self.app.app().window(window).expect("窗口存在").tree();
            let NodeKind::Canvas(canvas) = tree.get(self.node.caret).expect("节点存在").kind()
            else {
                panic!("caret 节点必须是 Canvas");
            };
            canvas
                .primitives()
                .iter()
                .filter_map(|primitive| match primitive {
                    Primitive::Quad(quad) => Some(quad.bounds),
                    _ => None,
                })
                .collect()
        }

        fn caret_fills(&self) -> Vec<Color> {
            let window = self.window();
            let tree = self.app.app().window(window).expect("窗口存在").tree();
            let NodeKind::Canvas(canvas) = tree.get(self.node.caret).expect("节点存在").kind()
            else {
                panic!("caret 节点必须是 Canvas");
            };
            canvas
                .primitives()
                .iter()
                .filter_map(|primitive| match primitive {
                    Primitive::Quad(quad) => match quad.paint {
                        ui_core::paint::primitive::QuadPaint::Solid { fill, .. } => Some(fill),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        }

        fn window_ref(&self) -> &ui_core::window::Window {
            self.app.app().window(self.window()).expect("窗口存在")
        }

        fn find(&self, debug_id: &str) -> Option<NodeId> {
            let tree = self
                .app
                .app()
                .window(self.window())
                .expect("窗口存在")
                .tree();
            let mut stack = vec![self.node.root];
            while let Some(node) = stack.pop() {
                if tree.debug_id(node) == Some(debug_id) {
                    return Some(node);
                }
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            None
        }

        fn display_of(&self, node: NodeId) -> taffy::style::Display {
            self.app
                .app()
                .window(self.window())
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("节点存在")
                .style()
                .inner()
                .display
        }

        fn click_node(&mut self, node: NodeId) {
            let bounds = self.app.probe().bounds(node).expect("节点已布局");
            let point = LogicalPoint::new(
                bounds.origin.x + bounds.size.width * 0.5,
                bounds.origin.y + bounds.size.height * 0.5,
            );
            let _ = self.app.inject(InputEvent::pointer_down(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
        }
    }

    #[test]
    fn insert_delete_backward_and_forward_edit_ascii() {
        let spec = spec();
        let mut state = state("");
        assert!(state.insert(&spec, "abc"));
        assert_eq!(state.text(), "abc");
        assert_eq!(state.caret(), 3);
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "ab");
        assert_eq!(state.caret(), 2);
        state.set_caret(0, false);
        assert!(state.delete_forward(&spec));
        assert_eq!(state.text(), "b");
        assert_eq!(state.caret(), 0);
        assert!(!state.delete_backward(&spec), "行首没有可删字符");
        assert!(state.delete_forward(&spec), "光标后仍有字符");
        assert_eq!(state.text(), "");
        assert!(!state.delete_forward(&spec), "空文本没有可删字符");
    }

    #[test]
    fn insert_replaces_the_selection() {
        let spec = spec();
        let mut state = state("hello");
        state.set_caret(1, false);
        state.set_caret(4, true);
        assert_eq!(state.selection(), 1..4);
        assert!(state.insert(&spec, "i"));
        assert_eq!(state.text(), "hio");
        assert_eq!(state.caret(), 2);
        assert_eq!(state.selection(), 2..2);
    }

    #[test]
    fn multi_byte_cjk_edits_never_split_bytes() {
        let spec = spec();
        let mut state = state("你好世界");
        assert_eq!(state.caret(), 12);
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "你好世");
        assert_eq!(state.caret(), 9);
        assert!(state.text().is_char_boundary(state.caret()));
        state.set_caret(6, false);
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "你世");
        state.set_caret(0, false);
        assert!(state.delete_forward(&spec));
        assert_eq!(state.text(), "世");
        assert!(state.text().is_char_boundary(state.caret()));
    }

    #[test]
    fn set_caret_snaps_inside_emoji_and_clamps() {
        let mut state = state("a🌍b");
        state.set_caret(2, false);
        assert_eq!(state.caret(), 1, "落在 emoji 中间必须向下吸附到 char 起点");
        state.set_caret(999, false);
        assert_eq!(state.caret(), "a🌍b".len());
        state.set_caret(9, false);
        assert!(state.text().is_char_boundary(state.caret()));
    }

    #[test]
    fn arrows_and_shift_select_characters() {
        let mut state = focused_state("ab中");
        state.set_caret(0, false);
        state.move_caret(CaretMotion::CharRight, false);
        assert_eq!(state.caret(), 1);
        state.move_caret(CaretMotion::CharRight, false);
        assert_eq!(state.caret(), 2);
        state.move_caret(CaretMotion::CharRight, true);
        assert_eq!(state.selection(), 2..5, "Shift 扩选一个 CJK 字符");
        state.move_caret(CaretMotion::CharLeft, false);
        assert_eq!(state.selection(), 2..2, "无 Shift 取消锚点");
        assert_eq!(state.caret(), 2);
        state.move_caret(CaretMotion::DocEnd, false);
        assert_eq!(state.caret(), 5);
        state.move_caret(CaretMotion::CharLeft, false);
        assert_eq!(state.caret(), 2);
    }

    #[test]
    fn home_and_end_stay_on_the_current_line() {
        let mut state = state("one\ntwo\nthree");
        state.set_caret(6, false);
        state.move_caret(CaretMotion::Home, false);
        assert_eq!(state.caret(), 4);
        state.move_caret(CaretMotion::End, false);
        assert_eq!(state.caret(), 7);
        state.move_caret(CaretMotion::Home, true);
        assert_eq!(state.selection(), 4..7);
        state.move_caret(CaretMotion::DocStart, false);
        assert_eq!(state.caret(), 0);
        state.move_caret(CaretMotion::DocEnd, false);
        assert_eq!(state.caret(), 13);
    }

    #[test]
    fn select_all_and_replace() {
        let spec = spec();
        let mut state = state("你好");
        state.select_all();
        assert_eq!(state.selection(), 0..6);
        assert!(state.insert(&spec, "hi"));
        assert_eq!(state.text(), "hi");
        assert_eq!(state.caret(), 2);
    }

    #[test]
    fn insert_newline_is_multiline_only() {
        let mut single = focused_state("");
        assert!(!single.insert_newline(&spec()));
        let multiline = InputSpec {
            multiline: true,
            ..spec()
        };
        let mut multi = focused_state("");
        assert!(multi.insert_newline(&multiline));
        assert_eq!(multi.text(), "\n");
        assert_eq!(multi.caret(), 1);
    }

    #[test]
    fn max_length_clips_ascii_and_cjk_by_chars() {
        let ascii_limit = InputSpec {
            max_length: Some(3),
            ..spec()
        };
        let mut ascii = state("");
        assert!(ascii.insert(&ascii_limit, "abcdef"));
        assert_eq!(ascii.text(), "abc");
        assert!(
            !ascii.insert(&ascii_limit, "汉字"),
            "已满且无选区时插入是 no-op"
        );
        assert_eq!(ascii.text(), "abc");

        let cjk_limit = InputSpec {
            max_length: Some(2),
            ..spec()
        };
        let mut cjk = state("");
        assert!(cjk.insert(&cjk_limit, "你好世界"));
        assert_eq!(cjk.text(), "你好");
        cjk.select_all();
        let smaller = InputSpec {
            max_length: Some(1),
            ..spec()
        };
        assert!(cjk.insert(&smaller, "🌍🌍"));
        assert_eq!(cjk.text(), "🌍");
        assert!(cjk.text().is_char_boundary(cjk.text().len()));
    }

    #[test]
    fn masked_display_uses_one_bullet_per_character() {
        let mut state = state("");
        state.insert(&spec(), "a你🌍");
        assert_eq!(state.display_text(true), "•••");
        assert_eq!(state.display_text(true).chars().count(), 3);
        assert_eq!(state.display_text(false), "a你🌍");
        assert_eq!(state.caret(), 1 + 3 + 4);
        assert_eq!(
            state.display_caret_byte(true),
            '•'.len_utf8() * 3,
            "脱敏 caret 按字符数换算"
        );
    }

    #[test]
    fn readonly_rejects_every_mutation() {
        let spec = InputSpec {
            readonly: true,
            ..spec()
        };
        let mut state = state("abc");
        assert!(!state.insert(&spec, "x"));
        assert!(!state.delete_backward(&spec));
        assert!(!state.delete_forward(&spec));
        assert!(!state.insert_newline(&spec));
        assert!(!state.ime_preedit(&spec, "ni", None));
        assert!(!state.ime_commit(&spec, "ni"));
        assert_eq!(state.text(), "abc");
        state.select_all();
        assert_eq!(state.selection(), 0..3);
    }

    #[test]
    fn disabled_rejects_edits() {
        let spec = InputSpec {
            disabled: true,
            ..spec()
        };
        let mut state = state("abc");
        assert!(!state.insert(&spec, "x"));
        assert!(!state.delete_backward(&spec));
        assert_eq!(state.text(), "abc");
    }

    #[test]
    fn validate_reports_empty_and_too_long_with_stable_i18n_keys() {
        let mut empty_state = state("");
        assert_eq!(empty_state.validate(&spec()), Some(InputError::Empty));
        assert_eq!(
            empty_state.error().map(InputError::i18n_key),
            Some("input.error.empty")
        );
        let limit = InputSpec {
            max_length: Some(1),
            ..spec()
        };
        let mut too_long = state("ab");
        assert_eq!(
            too_long.validate(&limit),
            Some(InputError::TooLong { max: 1 })
        );
        assert_eq!(
            too_long.error().map(InputError::i18n_key),
            Some("input.error.tooLong")
        );
        too_long.set_error(Some(InputError::Invalid {
            i18n_key: SmolStr::new_static("account.error.nameTaken"),
        }));
        assert_eq!(
            too_long.error().map(InputError::i18n_key),
            Some("account.error.nameTaken")
        );
    }

    #[test]
    fn utf16_helpers_count_surrogate_pairs_and_snap() {
        let text = "a🌍中";
        assert_eq!(utf16_len(text), 4, "ASCII 1 + emoji 2 + CJK 1");
        assert_eq!(utf8_from_utf16_offset(text, 0), 0);
        assert_eq!(utf8_from_utf16_offset(text, 1), 1);
        assert_eq!(
            utf8_from_utf16_offset(text, 2),
            1,
            "落在代理对内部：向下吸附到 emoji 起点"
        );
        assert_eq!(utf8_from_utf16_offset(text, 3), 5);
        assert_eq!(utf8_from_utf16_offset(text, 4), 8);
        assert_eq!(utf8_from_utf16_offset(text, 99), 8, "越界 clamp");
    }

    #[test]
    fn ime_preedit_then_commit_cjk() {
        let mut state = focused_state("");
        assert!(state.ime_preedit(&spec(), "ni", Some(2)));
        assert!(state.is_composing());
        assert_eq!(state.text(), "", "preedit 不进入提交文本");
        assert_eq!(state.display_text(false), "ni");
        assert_eq!(state.caret(), 2);
        assert!(state.ime_commit(&spec(), "你"));
        assert!(!state.is_composing());
        assert_eq!(state.text(), "你");
        assert_eq!(state.caret(), 3);
        assert_eq!(state.display_text(false), "你");
    }

    #[test]
    fn ime_commit_without_preedit_inserts_at_caret() {
        let mut state = focused_state("ab");
        assert!(state.ime_commit(&spec(), "中"));
        assert_eq!(state.text(), "ab中");
        assert_eq!(state.caret(), 5);
        assert!(!state.is_composing());
    }

    #[test]
    fn ime_cancel_on_end_drops_the_preedit() {
        let mut state = focused_state("ab");
        assert!(state.ime_preedit(&spec(), "ni", None));
        assert_eq!(state.display_text(false), "abni");
        state.ime_cancel();
        assert!(!state.is_composing());
        assert_eq!(state.display_text(false), "ab");
        assert_eq!(state.caret(), 2);
        assert_eq!(state.text(), "ab");
    }

    #[test]
    fn ime_preedit_replace_subrange_keeps_the_untouched_text() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let spec = spec();
        let mut state = focused_state("");
        assert!(state.ime_preedit(&spec, "abcdef", Some(6)));
        assert!(state.handle_ime(
            app.app_mut(),
            window,
            &spec,
            &ImeEvent::Preedit {
                text: SmolStr::new_static("XY"),
                caret: Some(2),
                clauses: Default::default(),
                replace: Some(2..4),
            },
        ));
        assert_eq!(state.display_text(false), "abXYef");
        assert!(state.ime_commit(&spec, "abXYef"));
        assert_eq!(state.text(), "abXYef");
    }

    #[test]
    fn ime_caret_utf16_is_relative_to_the_preedit_not_the_document() {
        let mut state = focused_state("🌍ab");
        assert_eq!(state.caret(), 6);
        assert!(state.ime_preedit(&spec(), "你好", Some(1)));
        assert_eq!(
            state.caret(),
            6 + 3,
            "组合串内 caret=1 → 第一个汉字之后（不是整串 UTF-16 偏移）"
        );
        assert_eq!(state.display_text(false), "🌍ab你好");
        assert!(state.ime_preedit(&spec(), "你好", Some(2)));
        assert_eq!(state.caret(), 6 + 6);
        assert!(state.ime_preedit(&spec(), "🌍a", Some(2)));
        assert_eq!(
            state.caret(),
            6 + 4,
            "emoji 的 2 个 UTF-16 码元 = 4 个 UTF-8 字节"
        );
        assert!(state.ime_preedit(&spec(), "🌍a", Some(1)));
        assert_eq!(state.caret(), 6, "落在代理对内部：向下吸附到 emoji 起点");
        assert!(state.ime_commit(&spec(), "🌍a"));
        assert_eq!(state.text(), "🌍ab🌍a");
    }

    #[test]
    fn ime_start_clears_the_selection() {
        let mut state = focused_state("hello");
        state.set_caret(1, false);
        state.set_caret(3, true);
        assert!(state.ime_preedit(&spec(), "ni", Some(1)));
        assert_eq!(state.text(), "hlo", "组合开始时选区被删掉");
        assert_eq!(state.display_text(false), "hnilo");
        assert!(state.ime_commit(&spec(), "你"));
        assert_eq!(state.text(), "h你lo");
    }

    #[test]
    fn losing_focus_drops_the_preedit() {
        let mut state = focused_state("ab");
        assert!(state.ime_preedit(&spec(), "ni", None));
        assert!(state.is_composing());
        state.set_focused(false);
        assert!(!state.is_composing());
        assert_eq!(state.text(), "ab");
    }

    #[test]
    fn clipboard_copy_cut_paste_through_app() {
        let app = TestApp::new();
        let app_ref = app.app();
        let platform = app_ref.set_clipboard_text("seed").is_ok();
        let mut state = state("hello");
        state.set_caret(1, false);
        state.set_caret(4, true);
        let copied = state.copy(app_ref, &spec());
        if platform {
            assert!(copied, "有原生剪贴板时复制必须成功");
            assert_eq!(app_ref.clipboard_text().expect("读回"), "ell");
            let mut target = self::state("");
            assert!(target.paste(app_ref, &spec()));
            assert_eq!(target.text(), "ell");
        } else {
            assert!(!copied, "平台拒绝剪贴板时复制返回 false");
            assert!(!state.paste(app_ref, &spec()));
        }
        let masked = InputSpec {
            masked: true,
            ..spec()
        };
        assert!(!state.copy(app_ref, &masked));
        assert!(!state.cut(app_ref, &masked));
        let readonly = InputSpec {
            readonly: true,
            ..spec()
        };
        assert!(!state.cut(app_ref, &readonly));
        assert!(!state.paste(app_ref, &readonly));
        let empty = self::state("");
        assert!(!empty.copy(app_ref, &spec()));
    }

    #[test]
    fn build_renders_background_and_text_and_placeholder() {
        let harness = harness(spec(), state("hello"));
        assert!(harness.root_quad_count() >= 1, "根节点必须画出背景");
        assert_eq!(harness.node_text(harness.node.text), "hello");
        assert_eq!(harness.node_text(harness.node.placeholder), "请输入");
    }

    #[test]
    fn typing_through_dispatch_grows_the_text_node() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.type_text("你");
        harness.type_text("a");
        harness.drain();
        assert_eq!(harness.state().text(), "你a");
        assert_eq!(harness.node_text(harness.node.text), "你a");
    }

    #[test]
    fn caret_canvas_tracks_window_caret_rect() {
        let mut harness = harness(spec(), state("hi"));
        harness.focus();
        harness.sync();
        harness.drain();
        let caret_rect = harness
            .window_ref()
            .text_caret_rect(harness.node.text, 2)
            .expect("文本已 shape");
        let caret_quad = harness.caret_quads().last().copied().expect("caret quad");
        assert!(
            (caret_quad.origin.x - caret_rect.origin.x).value().abs() <= 0.5
                && (caret_quad.origin.y - caret_rect.origin.y).value().abs() <= 0.5,
            "caret 画布必须与引擎 caret 矩形同源：{caret_quad:?} vs {caret_rect:?}"
        );
        assert!(
            (caret_quad.size.width - caret_rect.size.width)
                .value()
                .abs()
                <= 0.5
        );
    }

    #[test]
    fn masked_caret_canvas_uses_bullet_offsets() {
        let mut harness = harness(
            InputSpec {
                masked: true,
                ..spec()
            },
            state("ab"),
        );
        harness.focus();
        let bullet_caret = 2 * '•'.len_utf8();
        let caret_rect = harness
            .window_ref()
            .text_caret_rect(harness.node.text, bullet_caret)
            .expect("脱敏文本已 shape");
        let caret_quad = harness.caret_quads().last().copied().expect("caret quad");
        assert!((caret_quad.origin.x - caret_rect.origin.x).value().abs() <= 0.5);
        assert!((caret_quad.origin.y - caret_rect.origin.y).value().abs() <= 0.5);
        let area = harness
            .app
            .app()
            .ime_cursor_area(harness.window())
            .expect("窗口存在")
            .expect("聚焦时必须登记 IME caret 区域");
        assert!((area.origin.x - caret_rect.origin.x).value().abs() <= 0.5);
    }

    #[test]
    fn selection_draws_quads_behind_the_caret() {
        let mut harness = harness(spec(), state("hello"));
        harness.focus();
        harness
            .app
            .app_mut()
            .update(&harness.state, |state| state.select_all());
        harness.sync();
        harness.drain();
        let quads = harness.caret_quads();
        assert!(quads.len() >= 2, "全选必须画出选区矩形 + caret：{quads:?}");
    }

    #[test]
    fn sync_mirrors_focus_and_sets_ime_cursor_area() {
        let mut harness = harness(spec(), state("hi"));
        let window = harness.window();
        let root = harness.node.root;
        harness.app.app_mut().set_focus(window, Some(root));
        assert!(harness.node.sync(harness.app.app_mut()));
        harness.drain();
        assert!(harness.state().is_focused(), "焦点镜像必须写回状态");
        let area = harness
            .app
            .app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .expect("聚焦时必须登记 IME caret 区域");
        let caret_rect = harness
            .window_ref()
            .text_caret_rect(harness.node.text, 2)
            .expect("文本已 shape");
        assert!((area.origin.x - caret_rect.origin.x).value().abs() <= 0.5);
        assert!((area.origin.y - caret_rect.origin.y).value().abs() <= 0.5);
        harness.app.app_mut().set_focus(window, None);
        assert!(harness.node.sync(harness.app.app_mut()));
        assert!(!harness.state().is_focused());
        assert!(
            harness
                .app
                .app()
                .ime_cursor_area(window)
                .expect("窗口存在")
                .is_none()
        );
    }

    #[test]
    fn set_caret_from_point_uses_shaped_text_and_char_boundaries() {
        let harness = harness(spec(), state("ab"));
        let window = harness.window();
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.text)
            .expect("已布局");
        let window_ref = harness.window_ref();
        let mut state = harness.state();
        assert!(state.set_caret_from_point(
            window_ref,
            harness.node.text,
            LogicalPoint::new(px(1.0), bounds.size.height / 2.0),
            false
        ));
        assert_eq!(state.caret(), 0, "点在最左：字节 0");
        assert!(state.set_caret_from_point(
            window_ref,
            harness.node.text,
            LogicalPoint::new(bounds.size.width - 0.5, bounds.size.height / 2.0),
            false
        ));
        assert_eq!(state.caret(), 2, "点在末尾：文本长度");
        assert!(state.text().is_char_boundary(state.caret()));
        assert!(!state.set_caret_from_point(
            window_ref,
            harness.node.root,
            LogicalPoint::ZERO,
            false
        ));
        let _ = window;
    }

    #[test]
    fn click_action_moves_caret_and_focuses_the_input() {
        let mut harness = harness(spec(), state("hello"));
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.text)
            .expect("已布局");
        let position = LogicalPoint::new(
            bounds.origin.x + 1.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
        assert_eq!(
            harness.app.app().focused(harness.window()),
            Some(harness.node.root),
            "主键点击必须把焦点交给输入框"
        );
        assert_eq!(harness.state().caret(), 0, "点在文本最左：caret 到 0");
    }

    #[test]
    fn release_detaches_the_click_and_drag_handlers_and_is_idempotent() {
        let mut harness = harness(spec(), state("hello"));
        let click = click_action(&harness.node.spec);
        let drag = drag_action(&harness.node.spec);
        assert!(
            harness.app.app().has_action_handler(&click),
            "install 注册点击定位"
        );
        assert!(
            harness.app.app().has_action_handler(&drag),
            "install 注册拖选"
        );

        assert_eq!(harness.node.release(harness.app.app_mut()), 2);
        assert!(!harness.app.app().has_action_handler(&click));
        assert!(!harness.app.app().has_action_handler(&drag));

        let window = harness.window();
        let root = harness.node.root;
        assert!(
            !harness.app.app_mut().dispatch_action(
                window,
                root,
                click.clone(),
                ActionSource::Programmatic
            ),
            "release 之后点击动作没有 handler（不再被处理）"
        );

        assert_eq!(
            harness.node.release(harness.app.app_mut()),
            0,
            "重复 release 幂等"
        );
        assert!(!harness.app.app().has_action_handler(&click));
        assert!(!harness.app.app().has_action_handler(&drag));
    }

    #[test]
    fn releasing_a_disabled_input_reports_nothing_to_release() {
        let mut harness = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            state("hello"),
        );
        assert_eq!(
            harness.node.release(harness.app.app_mut()),
            0,
            "禁用的输入框没有注册 handler"
        );
    }

    #[test]
    fn a_clearable_input_shows_a_clear_control_only_while_it_has_text() {
        let mut harness = harness(spec(), focused_state("50"));
        assert!(harness.node.set_clearable(harness.app.app_mut(), true));
        assert!(
            !harness.node.set_clearable(harness.app.app_mut(), true),
            "重复开启幂等"
        );
        let clear = harness.find("test.clear:interactive").expect("清空控件");
        harness.sync();
        harness.drain();
        assert_eq!(
            harness.display_of(clear),
            taffy::style::Display::Flex,
            "有正文 ⇒ 清空控件显示"
        );

        harness.click_node(clear);
        harness.sync();
        harness.drain();
        assert_eq!(harness.state().text(), "", "点清空控件把正文清空");
        assert_eq!(
            harness.display_of(clear),
            taffy::style::Display::None,
            "空正文 ⇒ 清空控件隐藏"
        );

        assert!(harness.node.set_clearable(harness.app.app_mut(), false));
        assert!(
            harness.find("test.clear:interactive").is_none(),
            "关掉能力后控件节点被摘除"
        );
        assert!(
            !harness.node.set_clearable(harness.app.app_mut(), false),
            "重复关闭幂等"
        );
        assert_eq!(
            harness.node.release(harness.app.app_mut()),
            2,
            "清空控件已归还，release 只剩两个 handler"
        );
    }

    #[test]
    fn blur_dispatches_the_submit_action_when_the_switch_is_on() {
        let hits = Rc::new(Cell::new(0u32));
        let submit = Action::Custom(SmolStr::new_static("ui_kit.input.blur.submit"));
        let mut spec = spec();
        spec.submit = Some(submit.clone());
        let mut harness = harness(spec, focused_state("50"));
        let counter = hits.clone();
        harness.app.app_mut().on_action(submit, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        assert!(harness.node.set_submit_on_blur(true));
        assert!(
            !harness.node.set_submit_on_blur(true),
            "重复设置失焦提交开关幂等"
        );

        harness.focus();
        assert_eq!(hits.get(), 0, "聚焦本身不提交");
        let window = harness.window();
        assert!(harness.app.app_mut().set_focus(window, None));
        harness.sync();
        assert_eq!(hits.get(), 1, "失焦的同一轮 sync 提交一次");
        harness.sync();
        assert_eq!(hits.get(), 1, "后续 sync 不重复提交");

        assert!(harness.node.set_submit_on_blur(false));
        harness.focus();
        assert!(harness.app.app_mut().set_focus(window, None));
        harness.sync();
        assert_eq!(hits.get(), 1, "开关关闭后失焦不提交（旧语义）");
    }

    #[test]
    fn disabled_input_is_not_focusable_and_ignores_edits() {
        let mut harness = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            state("hello"),
        );
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.root)
            .expect("已布局");
        let position = LogicalPoint::new(bounds.origin.x + 2.0, bounds.origin.y + 2.0);
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
        assert_eq!(
            harness.app.app().focused(harness.window()),
            None,
            "禁用输入框不可聚焦"
        );
        let window = harness.window();
        let tree = harness.app.app().window(window).expect("窗口存在").tree();
        assert_eq!(
            tree.get(harness.node.root)
                .expect("节点存在")
                .style_patch()
                .interactive,
            Some(false),
            "禁用态必须通过 StylePatch.interactive 关闭交互"
        );
    }

    #[test]
    fn masked_text_is_rendered_as_bullets() {
        let mut harness = harness(
            InputSpec {
                masked: true,
                ..spec()
            },
            state(""),
        );
        harness.focus();
        harness.type_text("你a");
        harness.drain();
        assert_eq!(harness.state().text(), "你a");
        assert_eq!(harness.node_text(harness.node.text), "••");
    }

    #[test]
    fn ime_events_route_through_dispatch_into_the_text_node() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.ime(ImeEvent::Preedit {
            text: SmolStr::new_static("你好"),
            caret: Some(1),
            clauses: Default::default(),
            replace: None,
        });
        harness.drain();
        assert_eq!(harness.node_text(harness.node.text), "你好");
        assert!(harness.state().is_composing());
        harness.ime(ImeEvent::Commit {
            text: SmolStr::new_static("你好"),
            replace: None,
        });
        harness.drain();
        assert_eq!(harness.state().text(), "你好");
        assert!(!harness.state().is_composing());
        assert_eq!(harness.node_text(harness.node.text), "你好");
    }

    #[test]
    fn key_events_route_through_dispatch_and_enter_submits() {
        let submit = Action::custom("ui_kit.input.submit");
        let mut harness = harness(
            InputSpec {
                submit: Some(submit.clone()),
                ..spec()
            },
            state(""),
        );
        let hits = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let counter = hits.clone();
        harness.app.app_mut().on_action(submit, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        harness.focus();
        harness.type_text("ab");
        harness.key(KeyCode::ArrowLeft, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().caret(), 1);
        harness.key(KeyCode::Enter, Modifiers::empty());
        assert_eq!(hits.get(), 1, "Enter 必须派发 spec.submit");
        harness.key(KeyCode::Backspace, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().text(), "b");
    }

    #[test]
    fn multiline_enter_inserts_a_newline_through_dispatch() {
        let mut harness = harness(
            InputSpec {
                multiline: true,
                ..spec()
            },
            state(""),
        );
        harness.focus();
        harness.type_text("ab");
        harness.key(KeyCode::Enter, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().text(), "ab\n");
        assert_eq!(harness.node_text(harness.node.text), "ab\n");
    }

    #[test]
    fn caret_canvas_primitive_is_emitted_for_the_caret_node() {
        let mut harness = harness(spec(), state("hi"));
        harness.focus();
        let record = harness.app.frame_record();
        let frame = record.frame.as_ref().expect("提交帧");
        assert!(
            frame.primitives.iter().any(|primitive| {
                primitive.node == harness.node.caret && primitive.kind == PrimitiveKind::Canvas
            }),
            "caret Canvas 节点必须进入绘制序列"
        );
    }

    #[test]
    fn click_action_key_is_namespaced_by_spec_name() {
        let spec = InputSpec {
            name: SmolStr::new_static("account.email"),
            ..spec()
        };
        assert_eq!(
            format!("{:?}", click_action(&spec)),
            "Custom(\"ui_kit.input.account.email.click\")"
        );
    }

    #[test]
    fn readonly_input_still_gets_a_caret_canvas() {
        let mut harness = harness(
            InputSpec {
                readonly: true,
                ..spec()
            },
            state("abc"),
        );
        harness.focus();
        harness.sync();
        harness.drain();
        assert!(!harness.caret_quads().is_empty(), "只读输入仍要画 caret");
        assert!(
            harness
                .app
                .app()
                .ime_cursor_area(harness.window())
                .expect("窗口存在")
                .is_none(),
            "只读输入不登记 IME caret 区域"
        );
    }

    #[test]
    fn error_state_changes_the_border_patch() {
        let mut harness = harness(spec(), state("abc"));
        let window = harness.window();
        let normal = {
            let tree = harness.app.app().window(window).expect("窗口存在").tree();
            tree.get(harness.node.root)
                .expect("节点存在")
                .style_patch()
                .border
                .expect("绑定边框")
        };
        harness.app.app_mut().update(&harness.state, |state| {
            state.set_error(Some(InputError::Invalid {
                i18n_key: SmolStr::new_static("account.error.nameTaken"),
            }));
        });
        harness.drain();
        let with_error = {
            let tree = harness.app.app().window(window).expect("窗口存在").tree();
            tree.get(harness.node.root)
                .expect("节点存在")
                .style_patch()
                .border
                .expect("绑定边框")
        };
        assert_ne!(
            normal.color, with_error.color,
            "错误态必须换掉边框颜色（StatusDangerBorder）"
        );
    }

    #[test]
    fn undo_redo_restores_insert_delete_and_selection_replacement() {
        let spec = spec();
        let mut state = state("hello");
        assert!(state.insert(&spec, " world"));
        assert!(state.delete_backward(&spec));
        assert_eq!(state.text(), "hello worl");

        assert!(state.undo());
        assert_eq!(state.text(), "hello world", "撤销一次退格");
        assert!(state.undo());
        assert_eq!(state.text(), "hello", "撤销插入");
        assert!(state.redo());
        assert_eq!(state.text(), "hello world");
        assert!(state.redo());
        assert_eq!(state.text(), "hello worl", "重做退格");
        assert!(!state.can_redo() || state.redo());

        let mut state = self::state("hello");
        state.set_caret(1, false);
        state.set_caret(4, true);
        assert!(state.insert(&spec, "i"));
        assert_eq!(state.text(), "hio");
        assert!(state.undo());
        assert_eq!(state.text(), "hello");
        assert_eq!(state.selection(), 1..4, "撤销必须恢复编辑前的选区");
    }

    #[test]
    fn undo_history_is_bounded_to_one_hundred_steps() {
        let spec = spec();
        let mut state = state("");
        for _ in 0..120 {
            assert!(state.insert(&spec, "a"));
        }
        assert_eq!(state.text().chars().count(), 120);

        for _ in 0..100 {
            assert!(state.undo(), "有界历史里的 100 步都能撤销");
        }
        assert_eq!(
            state.text().chars().count(),
            20,
            "上限 100 步：最旧的 20 次编辑不再可撤销"
        );
        assert!(!state.can_undo());
        assert!(state.can_redo(), "撤销出来的 100 步都可重做");
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let spec = spec();
        let mut state = state("");
        assert!(state.insert(&spec, "a"));
        assert!(state.insert(&spec, "b"));
        assert!(state.undo());
        assert!(state.can_redo());
        assert!(state.insert(&spec, "c"));
        assert_eq!(state.text(), "ac");
        assert!(!state.can_redo(), "新编辑必须清空重做栈");
    }

    #[test]
    fn keyboard_undo_redo_shortcuts_route_through_dispatch() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.type_text("ab");
        harness.drain();
        assert_eq!(harness.state().text(), "ab");

        harness.key(KeyCode::KeyZ, Modifiers::CONTROL);
        harness.drain();
        assert_eq!(harness.state().text(), "", "Ctrl+Z 撤销");
        harness.key(KeyCode::KeyZ, Modifiers::CONTROL | Modifiers::SHIFT);
        harness.drain();
        assert_eq!(harness.state().text(), "ab", "Ctrl+Shift+Z 重做");
        harness.key(KeyCode::KeyZ, Modifiers::CONTROL);
        harness.drain();
        harness.key(KeyCode::KeyY, Modifiers::CONTROL);
        harness.drain();
        assert_eq!(harness.state().text(), "ab", "Ctrl+Y 重做");
    }

    #[test]
    fn single_line_insert_drops_carriage_returns_newlines_and_tabs() {
        let mut state = state("");
        assert!(state.insert(&spec(), "a\r\nb\tc"));
        assert_eq!(state.text(), "abc");
        assert_eq!(state.caret(), 3);

        let multiline = InputSpec {
            multiline: true,
            ..spec()
        };
        let mut state = self::state("");
        assert!(state.insert(&multiline, "a\r\nb\tc"));
        assert_eq!(state.text(), "a\r\nb\tc", "多行输入不改写换行/制表符");
    }

    #[test]
    fn single_line_ime_commit_and_set_text_drop_line_breaks() {
        let spec = spec();
        let mut state = state("");
        state.set_focused(true);
        assert!(state.ime_preedit(&spec, "ab", None));
        assert!(state.ime_commit(&spec, "a\nb\tc"));
        assert_eq!(state.text(), "abc");

        assert!(state.set_text(&spec, "x\r\ny\tz"));
        assert_eq!(state.text(), "xyz");
    }

    #[test]
    fn repeated_editing_and_navigation_keys_are_not_discarded() {
        let mut harness = harness(spec(), state("abc"));
        harness.focus();
        harness.key_repeat(KeyCode::Backspace, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().text(), "ab", "长按退格必须持续删除");
        harness.key_repeat(KeyCode::Backspace, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().text(), "a");
        harness.key_repeat(KeyCode::ArrowLeft, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().caret(), 0, "长按方向键必须移动光标");
        harness.key_repeat(KeyCode::ArrowRight, Modifiers::empty());
        harness.drain();
        assert_eq!(harness.state().caret(), 1);
    }

    #[test]
    fn repeated_enter_does_not_submit_again() {
        let submit = Action::custom("ui_kit.input.submit.repeat");
        let mut harness = harness(
            InputSpec {
                submit: Some(submit.clone()),
                ..spec()
            },
            state(""),
        );
        let hits = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let counter = hits.clone();
        harness.app.app_mut().on_action(submit, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        harness.focus();
        harness.key(KeyCode::Enter, Modifiers::empty());
        harness.key_repeat(KeyCode::Enter, Modifiers::empty());
        assert_eq!(hits.get(), 1, "自动重复的 Enter 不再派发 submit");
    }

    #[test]
    fn ime_end_event_cancels_the_composition_through_dispatch() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.ime(ImeEvent::Preedit {
            text: SmolStr::new_static("ni"),
            caret: Some(2),
            clauses: Default::default(),
            replace: None,
        });
        harness.drain();
        assert!(harness.state().is_composing(), "Preedit 必须开启组合");
        assert_eq!(harness.node_text(harness.node.text), "ni");

        harness.ime(ImeEvent::End);
        harness.drain();
        assert!(!harness.state().is_composing(), "End 必须取消组合");
        assert_eq!(harness.state().text(), "", "组合串不进正文");
        assert_eq!(harness.node_text(harness.node.text), "");
        assert_eq!(harness.state().caret(), 0, "caret 回到组合起点");
    }

    #[test]
    fn escape_collapses_the_selection_after_cancelling_composition() {
        let mut harness = harness(spec(), state("hello"));
        harness.focus();
        let mut state = harness.state();
        state.set_caret(1, false);
        state.set_caret(4, true);
        harness.write_state(state);
        harness.drain();
        assert_eq!(harness.state().selection(), 1..4);
        harness.key(KeyCode::Escape, Modifiers::empty());
        harness.drain();
        assert_eq!(
            harness.state().selection(),
            1..1,
            "Esc 收缩选区到 caret 起点"
        );
        assert_eq!(harness.state().caret(), 1);
    }

    #[test]
    fn long_single_line_scrolls_to_keep_the_caret_visible() {
        let mut harness = harness(spec(), state(&"a".repeat(200)));
        harness.focus();
        for _ in 0..3 {
            harness.sync();
            harness.drain();
        }
        let handle = harness
            .scroll_handle()
            .expect("单行输入必须登记横向滚动容器");
        let scroll = harness
            .app
            .app()
            .scroll_state(&handle)
            .expect("滚动容器状态");
        assert!(
            scroll.offset.x.value() > 0.0,
            "200 字符的 caret 在末尾，必须横向滚动才能看见：{:?}",
            scroll.offset
        );
        let viewport = harness.node.viewport.expect("单行视口");
        let viewport_bounds = harness.app.probe().bounds(viewport).expect("视口布局");
        let caret = harness
            .window_ref()
            .text_caret_rect(harness.node.text, 200)
            .expect("caret 几何");
        assert!(
            caret.origin.x >= viewport_bounds.origin.x - 0.5,
            "滚动不得把 caret 推到视口左侧之外：caret={}，视口左缘={}",
            caret.origin.x,
            viewport_bounds.origin.x
        );
        assert!(
            caret.origin.x <= viewport_bounds.origin.x + viewport_bounds.size.width + 0.5,
            "滚动后 caret 必须落在视口内：caret={}，视口右缘={}",
            caret.origin.x,
            viewport_bounds.origin.x + viewport_bounds.size.width
        );
        let caret_quad = harness
            .caret_quads()
            .last()
            .copied()
            .expect("caret 画布至少有一个 quad");
        assert!(
            (caret_quad.origin.x - caret.origin.x).value().abs() <= 0.5
                && (caret_quad.origin.y - caret.origin.y).value().abs() <= 0.5,
            "滚动后 caret 画布必须等于引擎屏幕坐标：quad={caret_quad:?}，caret={caret:?}，offset={:?}",
            scroll.offset
        );
    }

    #[test]
    fn scrolled_caret_and_selection_canvases_use_screen_coordinates() {
        let mut harness = harness(spec(), state(&"a".repeat(200)));
        harness.focus();
        let mut state = harness.state();
        state.set_caret(120, false);
        state.set_caret(140, true);
        harness.write_state(state);
        for _ in 0..3 {
            harness.sync();
            harness.drain();
        }
        let handle = harness
            .scroll_handle()
            .expect("单行输入必须登记横向滚动容器");
        let scroll = harness
            .app
            .app()
            .scroll_state(&handle)
            .expect("滚动容器状态");
        assert!(
            scroll.offset.x.value() > 0.0,
            "选区末端在视口外，必须横向滚动才看得见：{:?}",
            scroll.offset
        );
        let viewport = harness.node.viewport.expect("单行视口");
        let viewport_bounds = harness.app.probe().bounds(viewport).expect("视口布局");
        let caret = harness
            .window_ref()
            .text_caret_rect(harness.node.text, 140)
            .expect("caret 几何");
        assert!(
            caret.origin.x >= viewport_bounds.origin.x - 0.5
                && caret.origin.x + caret.size.width
                    <= viewport_bounds.origin.x + viewport_bounds.size.width + 0.5,
            "滚动后 caret 必须完整落在视口内：caret={caret:?}，视口={viewport_bounds:?}"
        );

        let window_ref = harness.window_ref();
        let local_zero = window_ref
            .tree()
            .text_caret_rect(harness.node.text, 0)
            .expect("局部原点");
        let absolute_zero = window_ref
            .text_caret_rect(harness.node.text, 0)
            .expect("屏幕原点");
        let origin = LogicalPoint::new(
            absolute_zero.origin.x - local_zero.origin.x,
            absolute_zero.origin.y - local_zero.origin.y,
        );
        let expected_selection = window_ref
            .tree()
            .text_selection_rects(harness.node.text, 120, 140)
            .expect("选区矩形");
        assert!(
            !expected_selection.is_empty(),
            "140 字节的选区至少要有一个矩形"
        );

        let quads = harness.caret_quads();
        assert_eq!(
            quads.len(),
            expected_selection.len() + 1,
            "选区矩形 + caret：{quads:?}"
        );
        let caret_quad = *quads.last().expect("caret quad");
        assert!(
            (caret_quad.origin.x - caret.origin.x).value().abs() <= 0.5
                && (caret_quad.origin.y - caret.origin.y).value().abs() <= 0.5,
            "caret quad 必须是屏幕坐标：quad={caret_quad:?}，caret={caret:?}，offset={:?}",
            scroll.offset
        );
        for (quad, rect) in quads[..quads.len() - 1]
            .iter()
            .zip(expected_selection.iter())
        {
            let expected = rect.translate(origin);
            assert!(
                (quad.origin.x - expected.origin.x).value().abs() <= 0.5
                    && (quad.origin.y - expected.origin.y).value().abs() <= 0.5
                    && (quad.size.width - expected.size.width).value().abs() <= 0.5
                    && (quad.size.height - expected.size.height).value().abs() <= 0.5,
                "选区 quad 必须是局部矩形 + 屏幕原点：quad={quad:?}，expected={expected:?}"
            );
            assert!(
                quad.origin.x >= viewport_bounds.origin.x - 0.5
                    && quad.origin.x <= viewport_bounds.origin.x + viewport_bounds.size.width,
                "滚动后选区高亮的左缘必须在视口内（不能被裁到视口外）：quad={quad:?}，视口={viewport_bounds:?}"
            );
        }
    }

    #[test]
    fn clicks_map_through_the_horizontal_scroll_offset() {
        let mut harness = harness(spec(), state(&"0123456789".repeat(20)));
        harness.focus();
        for _ in 0..3 {
            harness.sync();
            harness.drain();
        }
        let handle = harness.scroll_handle().expect("横向滚动容器");
        let scroll = harness.app.app().scroll_state(&handle).expect("滚动状态");
        assert!(scroll.offset.x.value() > 0.0);
        let viewport = harness.node.viewport.expect("视口");
        let viewport_bounds = harness.app.probe().bounds(viewport).expect("视口布局");
        let text_bounds = harness
            .app
            .probe()
            .bounds(harness.node.text)
            .expect("文本布局");
        let position = LogicalPoint::new(
            viewport_bounds.origin.x + viewport_bounds.size.width - 2.0,
            text_bounds.origin.y + text_bounds.size.height / 2.0,
        );
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
        assert!(
            harness.state().caret() > 150,
            "点在滚动后视口右缘附近必须落到文本尾部：caret={}，offset={}",
            harness.state().caret(),
            scroll.offset.x
        );
    }

    fn caret_position(harness: &Harness, byte: usize) -> LogicalPoint {
        let bounds = harness
            .app
            .probe()
            .bounds(harness.node.text)
            .expect("文本布局");
        let caret = harness
            .window_ref()
            .text_caret_rect(harness.node.text, byte)
            .expect("caret 几何");
        LogicalPoint::new(
            caret.origin.x + 0.5,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn drag(harness: &mut Harness, from: LogicalPoint, to: LogicalPoint) {
        harness.app.inject(InputEvent::pointer_down(
            from,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness
            .app
            .inject(InputEvent::pointer_move(to, Modifiers::empty()));
        harness.app.inject(InputEvent::pointer_up(
            to,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
    }

    #[test]
    fn dragging_across_text_selects_the_span_forward() {
        let mut harness = harness(spec(), state("hello world"));
        harness.focus();
        let start = caret_position(&harness, 0);
        let end = caret_position(&harness, 5);
        drag(&mut harness, start, end);
        assert_eq!(harness.state().selection(), 0..5);
        assert_eq!(&harness.state().text()[0..5], "hello");
        assert!(!harness.state().is_dragging());
    }

    #[test]
    fn dragging_backwards_selects_the_span_the_other_way() {
        let mut harness = harness(spec(), state("hello world"));
        harness.focus();
        let start = caret_position(&harness, 5);
        let end = caret_position(&harness, 0);
        drag(&mut harness, start, end);
        assert_eq!(harness.state().selection(), 0..5);
        assert_eq!(harness.state().caret(), 0, "反向拖选 caret 在起点");
        assert_eq!(harness.state().selection_anchor(), 5);
    }

    #[test]
    fn dragging_past_the_text_clamps_to_the_edge() {
        let mut harness = harness(spec(), state("hello world"));
        harness.focus();
        let start = caret_position(&harness, 5);
        let far_right = LogicalPoint::new(px(2_000.0), start.y);
        drag(&mut harness, start, far_right);
        assert_eq!(
            harness.state().selection(),
            5..11,
            "拖出框必须 clamp 到文本尾"
        );
    }

    #[test]
    fn a_plain_click_after_dragging_collapses_the_selection() {
        let mut harness = harness(spec(), state("hello world"));
        harness.focus();
        let from = caret_position(&harness, 0);
        let to = caret_position(&harness, 5);
        drag(&mut harness, from, to);
        assert_eq!(harness.state().selection(), 0..5);
        let position = caret_position(&harness, 8);
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.drain();
        assert_eq!(harness.state().selection(), 8..8, "单击不扩选，折叠选区");
        assert_eq!(harness.state().caret(), 8);
    }

    fn click(harness: &mut Harness, position: LogicalPoint) {
        harness.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    #[test]
    fn double_click_selects_the_word_under_the_pointer() {
        let mut harness = harness(spec(), state("hello world"));
        harness.focus();
        let position = caret_position(&harness, 2);
        click(&mut harness, position);
        click(&mut harness, position);
        harness.drain();
        assert_eq!(harness.state().selection(), 0..5, "双击选中 hello");
    }

    #[test]
    fn double_click_treats_a_cjk_run_as_one_word() {
        let mut harness = harness(spec(), state("你好世界 abc"));
        harness.focus();
        let position = caret_position(&harness, 3);
        click(&mut harness, position);
        click(&mut harness, position);
        harness.drain();
        assert_eq!(harness.state().selection(), 0..12, "CJK 连续段是一词");
    }

    #[test]
    fn triple_click_selects_the_whole_single_line() {
        let mut harness = harness(spec(), state("hello world"));
        harness.focus();
        let position = caret_position(&harness, 2);
        click(&mut harness, position);
        click(&mut harness, position);
        click(&mut harness, position);
        harness.drain();
        assert_eq!(
            harness.state().selection(),
            0..11,
            "三击选行（单行 = 全文）"
        );
    }

    #[test]
    fn update_value_rewrites_the_signal_and_reports_invalid_values() {
        let mut harness = harness(
            InputSpec {
                max_length: Some(3),
                ..spec()
            },
            state("old"),
        );
        assert!(harness.node.update_value(harness.app.app_mut(), "new"));
        assert_eq!(harness.state().text(), "new");
        assert_eq!(harness.state().caret(), 3, "程序化写值后光标在末尾");
        assert!(
            harness.state().selection().is_empty(),
            "程序化写值后没有选区"
        );
        harness.drain();
        assert_eq!(
            harness.node_text(harness.node.text),
            "new",
            "重绘后文本节点更新"
        );

        assert!(
            harness.node.update_value(harness.app.app_mut(), "abcd"),
            "超长值按字符裁剪后仍然生效"
        );
        assert_eq!(harness.state().text(), "abc");
        assert_eq!(
            harness.state().error(),
            Some(&InputError::TooLong { max: 3 }),
            "非法值必须写入错误态"
        );
        harness.drain();
        assert_eq!(harness.node_text(harness.node.text), "abc");

        assert!(harness.node.update_value(harness.app.app_mut(), "ab"));
        assert_eq!(harness.state().text(), "ab");
        assert_eq!(harness.state().error(), None, "合法值清空错误态");
    }

    #[test]
    fn update_value_bypasses_readonly_and_disabled() {
        let mut readonly = harness(
            InputSpec {
                readonly: true,
                ..spec()
            },
            state("locked"),
        );
        assert!(
            readonly
                .node
                .update_value(readonly.app.app_mut(), "changed")
        );
        assert_eq!(readonly.state().text(), "changed");
        readonly.drain();
        assert_eq!(readonly.node_text(readonly.node.text), "changed");

        let mut disabled = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            state("locked"),
        );
        assert!(
            disabled
                .node
                .update_value(disabled.app.app_mut(), "changed")
        );
        assert_eq!(disabled.state().text(), "changed");

        let mut state = state("locked");
        let readonly_spec = InputSpec {
            readonly: true,
            ..spec()
        };
        assert!(!state.set_text(&readonly_spec, "changed"));
        assert!(!state.insert(&readonly_spec, "x"));
        let disabled_spec = InputSpec {
            disabled: true,
            ..spec()
        };
        assert!(!state.set_text(&disabled_spec, "changed"));
        assert_eq!(state.text(), "locked");
    }

    #[test]
    fn update_value_clears_the_undo_history() {
        let mut harness = harness(spec(), state(""));
        harness.focus();
        harness.type_text("a");
        harness.drain();
        assert!(harness.state().can_undo(), "用户编辑进入撤销历史");
        assert!(harness.node.update_value(harness.app.app_mut(), "reset"));
        assert_eq!(harness.state().text(), "reset");
        assert!(!harness.state().can_undo(), "程序化写值必须清空撤销历史");
        assert!(!harness.state().can_redo(), "重做历史同样清空");
        let mut after = harness.state();
        assert!(!after.undo(), "程序化写值不可撤销");
        assert_eq!(after.text(), "reset");
    }

    #[test]
    fn update_placeholder_rewrites_the_placeholder_node() {
        let mut harness = harness(spec(), state(""));
        harness.drain();
        assert_eq!(harness.node_text(harness.node.placeholder), "请输入");
        assert!(
            harness
                .node
                .update_placeholder(harness.app.app_mut(), "搜索…")
        );
        assert_eq!(
            harness.app.app().read(&harness.placeholder).as_str(),
            "搜索…",
            "占位符信号必须失效"
        );
        harness.drain();
        assert_eq!(harness.node_text(harness.node.placeholder), "搜索…");
        assert!(
            !harness
                .node
                .update_placeholder(harness.app.app_mut(), "搜索…"),
            "同值写入被等值短路"
        );
    }

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    fn style_patch(
        app: &ui_core::app::App,
        window: WindowId,
        node: NodeId,
    ) -> ui_core::paint::style::StylePatch {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存活")
            .style_patch()
            .clone()
    }

    #[test]
    fn switching_appearance_repaints_every_input_color() {
        let mut harness = harness(spec(), state(""));
        let static_node = {
            let window = harness.app.window_id();
            let parent = harness.node.root;
            let tree = harness
                .app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            NodeBuilder::boxed()
                .style(StyleBuilder::new().size(px(10.0), px(10.0)).build())
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .build(tree, parent)
                .expect("静态兄弟")
        };
        harness.drain();

        let light = Theme::of(Appearance::Light);
        let light_input = light.components().input;
        let window = harness.window();
        let root_patch = style_patch(harness.app.app(), window, harness.node.root);
        assert_eq!(
            root_patch.background.map(resolved),
            Some(resolved(light_input.background.enabled)),
            "浅色输入框底色"
        );
        assert_eq!(
            root_patch.border.map(|border| resolved(border.color)),
            Some(resolved(light_input.border.normal)),
            "浅色边框"
        );
        assert_eq!(
            style_patch(harness.app.app(), window, harness.node.text)
                .foreground
                .map(resolved),
            Some(resolved(light_input.foreground.enabled)),
            "浅色文字"
        );
        assert_eq!(
            style_patch(harness.app.app(), window, harness.node.placeholder)
                .foreground
                .map(resolved),
            Some(resolved(light_input.placeholder)),
            "浅色占位符"
        );

        assert!(
            harness
                .theme
                .set_appearance(harness.app.app_mut(), Appearance::Dark)
        );
        let record = harness.app.drain().expect("切深色").clone();
        for node in [
            harness.node.root,
            harness.node.text,
            harness.node.placeholder,
            harness.node.caret,
        ] {
            assert!(
                record.dirty_nodes.contains(&node),
                "读过主题的节点必须被失效：{node:?}"
            );
        }
        assert!(
            !record.dirty_nodes.contains(&static_node),
            "未读主题的静态节点不得被失效：{:?}",
            record.dirty_nodes
        );

        let dark = Theme::of(Appearance::Dark);
        let dark_input = dark.components().input;
        let window = harness.window();
        assert_eq!(
            style_patch(harness.app.app(), window, harness.node.root)
                .background
                .map(resolved),
            Some(resolved(dark_input.background.enabled)),
            "深色输入框底色"
        );
        assert_eq!(
            style_patch(harness.app.app(), window, harness.node.root)
                .border
                .map(|border| resolved(border.color)),
            Some(resolved(dark_input.border.normal)),
            "深色边框"
        );
        assert_eq!(
            style_patch(harness.app.app(), window, harness.node.text)
                .foreground
                .map(resolved),
            Some(resolved(dark_input.foreground.enabled)),
            "深色文字"
        );
        assert_eq!(
            style_patch(harness.app.app(), window, harness.node.placeholder)
                .foreground
                .map(resolved),
            Some(resolved(dark_input.placeholder)),
            "深色占位符"
        );
        assert_ne!(
            resolved(light_input.background.enabled),
            resolved(dark_input.background.enabled),
            "两套外观的输入框底色必须不同（否则上面的断言无意义）"
        );
    }

    #[test]
    fn switching_appearance_repaints_the_caret_and_selection_canvas() {
        let mut harness = harness(spec(), state("abc"));
        harness.focus();
        let mut selected = harness.state();
        selected.set_caret(0, false);
        selected.set_caret(3, true);
        harness.write_state(selected);
        harness.sync();
        harness.drain();

        let light = Theme::of(Appearance::Light);
        let light_fills = harness.caret_fills();
        assert_eq!(
            light_fills.len(),
            2,
            "一个选区 + 一个 caret：{light_fills:?}"
        );
        assert_eq!(
            resolved(light_fills[0]),
            resolved(light.components().input.selection),
            "浅色选区"
        );
        assert_eq!(
            resolved(light_fills[1]),
            resolved(light.role(Role::FgDefault)),
            "浅色 caret"
        );

        assert!(
            harness
                .theme
                .set_appearance(harness.app.app_mut(), Appearance::Dark)
        );
        harness.sync();
        harness.drain();

        let dark = Theme::of(Appearance::Dark);
        let dark_fills = harness.caret_fills();
        assert_eq!(dark_fills.len(), 2, "一个选区 + 一个 caret：{dark_fills:?}");
        assert_eq!(
            resolved(dark_fills[0]),
            resolved(dark.components().input.selection),
            "深色选区"
        );
        assert_eq!(
            resolved(dark_fills[1]),
            resolved(dark.role(Role::FgDefault)),
            "深色 caret"
        );
        assert_ne!(
            resolved(light.role(Role::FgDefault)),
            resolved(dark.role(Role::FgDefault)),
            "两套外观的 caret 颜色必须不同（否则上面的断言无意义）"
        );
    }
}
