use smol_str::{SmolStr, SmolStrBuilder};

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

pub(crate) struct EditableText {
    committed: SmolStr,
    spec: EditableSpec,
}

impl EditableText {
    pub(crate) fn new(spec: EditableSpec) -> Self {
        Self {
            committed: SmolStr::new_static(""),
            spec,
        }
    }

    pub(crate) fn committed(&self) -> &str {
        &self.committed
    }

    pub(crate) fn spec(&self) -> &EditableSpec {
        &self.spec
    }

    fn set_committed(&mut self, text: SmolStr) {
        self.committed = text;
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

pub(crate) fn set_value(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    text: &str,
) -> Result<bool, EngineError> {
    let (multiline, style) = {
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
            editable.spec().text_style.clone(),
        )
    };
    let sanitized = sanitize(text, multiline);
    let changed = {
        let target = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let tree = target.tree_mut();
        validate_node(tree, window, node)?;
        let changed = {
            let Some(editable) = tree.editable_mut(node) else {
                return Err(EngineError::TextEditNotAvailable { node });
            };
            if editable.committed() == sanitized.as_str() {
                false
            } else {
                editable.set_committed(sanitized.clone());
                true
            }
        };
        if changed {
            tree.set_text_content(node, TextContent::new(sanitized, style));
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

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{EditableSpec, sanitize};
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
}
