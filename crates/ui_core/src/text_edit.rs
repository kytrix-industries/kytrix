use std::{ops::Range, sync::Arc};

use smallvec::SmallVec;
use smol_str::{SmolStr, SmolStrBuilder};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    a11y::A11yRole,
    app::App,
    error::EngineError,
    event::Action,
    math::LogicalPx,
    node::{id::NodeId, tree::NodeTree},
    paint::color::Color,
    text::{TextContent, TextStyle},
    window::WindowId,
};

const MASK_GLYPH: char = '\u{2022}';

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextAffinity {
    Upstream,
    Downstream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextCursor {
    pub byte: usize,
    pub affinity: TextAffinity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSelection {
    anchor: TextCursor,
    focus: TextCursor,
}

impl TextSelection {
    fn collapsed(cursor: TextCursor) -> Self {
        Self {
            anchor: cursor,
            focus: cursor,
        }
    }

    pub fn anchor(&self) -> TextCursor {
        self.anchor
    }

    pub fn focus(&self) -> TextCursor {
        self.focus
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.focus
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

pub(crate) struct EditableText {
    committed: Arc<str>,
    display: DisplayText,
    spec: EditableSpec,
    selection: TextSelection,
    generation: u64,
    snapshot: TextEditSnapshot,
    #[cfg(test)]
    display_builds: u32,
}

impl EditableText {
    pub(crate) fn new(spec: EditableSpec) -> Self {
        let committed: Arc<str> = Arc::from("");
        let selection = TextSelection::collapsed(end_cursor(0));
        Self {
            snapshot: snapshot_of(0, &committed, &selection),
            committed,
            display: DisplayText::Identity,
            spec,
            selection,
            generation: 0,
            #[cfg(test)]
            display_builds: 0,
        }
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

    pub(crate) fn display_text(&self) -> &str {
        match &self.display {
            DisplayText::Identity => &self.committed,
            DisplayText::Masked(masked) => &masked.text,
        }
    }

    pub(crate) fn raw_byte_for_display(&self, display_byte: usize) -> usize {
        match &self.display {
            DisplayText::Identity => display_byte.min(self.committed.len()),
            DisplayText::Masked(masked) => {
                masked.raw_byte_for_display(&self.committed, display_byte)
            }
        }
    }

    pub(crate) fn display_byte_for_raw(&self, raw_byte: usize) -> usize {
        match &self.display {
            DisplayText::Identity => raw_byte.min(self.committed.len()),
            DisplayText::Masked(masked) => masked.display_byte_for_raw(&self.committed, raw_byte),
        }
    }

    fn set_committed(&mut self, text: &str) -> bool {
        self.committed = Arc::from(text);
        let display = if self.spec.masked {
            DisplayText::Masked(MaskedDisplay::new(&self.committed))
        } else {
            DisplayText::Identity
        };
        let display_changed = match (&self.display, &display) {
            (DisplayText::Identity, DisplayText::Identity) => true,
            (DisplayText::Masked(previous), DisplayText::Masked(next)) => {
                previous.text != next.text
            }
            (DisplayText::Identity, DisplayText::Masked(_))
            | (DisplayText::Masked(_), DisplayText::Identity) => true,
        };
        self.display = display;
        #[cfg(test)]
        {
            self.display_builds += 1;
        }
        let committed_end = self.committed.len();
        let display_end = self.display_byte_for_raw(committed_end);
        debug_assert_eq!(self.raw_byte_for_display(display_end), committed_end);
        self.selection = TextSelection::collapsed(end_cursor(display_end));
        self.generation = self.generation.saturating_add(1);
        self.snapshot = snapshot_of(self.generation, &self.committed, &self.selection);
        display_changed
    }

    #[cfg(test)]
    pub(crate) fn display_builds(&self) -> u32 {
        self.display_builds
    }
}

fn end_cursor(display_len: usize) -> TextCursor {
    TextCursor {
        byte: display_len,
        affinity: if display_len == 0 {
            TextAffinity::Downstream
        } else {
            TextAffinity::Upstream
        },
    }
}

fn snapshot_of(generation: u64, text: &Arc<str>, selection: &TextSelection) -> TextEditSnapshot {
    TextEditSnapshot {
        generation,
        text: Arc::clone(text),
        selection: selection.clone(),
        composing: false,
        focused: false,
        can_undo: false,
        can_redo: false,
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
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let (changed, display) = {
            let Some(editable) = tree.editable_mut(node) else {
                return Err(EngineError::TextEditNotAvailable { node });
            };
            if editable.committed() == truncated {
                (false, None)
            } else {
                let display_changed = editable.set_committed(truncated);
                let display = display_changed.then(|| SmolStr::new(editable.display_text()));
                (true, display)
            }
        };
        if let Some(display) = display {
            tree.set_text_content(node, TextContent::new(display, style));
            tree.mark_paint(node);
        }
        if changed {
            tree.mark_a11y(node);
        }
        changed
    };
    if changed {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smol_str::SmolStr;
    use unicode_segmentation::UnicodeSegmentation;

    use super::{
        DisplayText, EditableSpec, EditableText, MASK_GLYPH, sanitize, truncate_graphemes,
    };
    use crate::{a11y::A11yRole, event::Action, math::px, paint::color::Color, text::TextStyle};

    fn spec() -> EditableSpec {
        EditableSpec {
            role: A11yRole::TextInput,
            label: SmolStr::new_static(""),
            placeholder: SmolStr::new_static(""),
            multiline: false,
            masked: false,
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
        editable.set_committed(text);
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
        editable.set_committed("a👍🏽");
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

        assert!(editable.set_committed("abcd"));
        let third = editable.snapshot().clone();
        assert_eq!(editable.display_builds(), 2);
        assert!(!Arc::ptr_eq(&first.text, &third.text));
        assert_eq!(third.generation, first.generation + 1);

        assert!(editable.set_committed("abcde"));
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
}
