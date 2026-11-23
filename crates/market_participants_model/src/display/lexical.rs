use smol_str::SmolStr;

use crate::{
    display::values::{DisplayFormatError, InvalidTextReason},
    identifier::VenueScopeId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentText<'a> {
    FreeText(&'a str),
    Typed(&'a str),
}

pub fn normalize_component_text(text: ComponentText<'_>) -> Result<SmolStr, DisplayFormatError> {
    let (raw, free_text) = match text {
        ComponentText::FreeText(value) => (value, true),
        ComponentText::Typed(value) => (value, false),
    };
    validate_component_text(raw)?;
    let normalized = if free_text {
        collapse_free_text(raw)
    } else {
        SmolStr::new(raw)
    };
    if normalized.is_empty() {
        return Err(invalid_text(raw, InvalidTextReason::Empty));
    }
    Ok(SmolStr::new(normalized.as_str().to_ascii_uppercase()))
}

pub fn scope_display_code(scope: &VenueScopeId) -> SmolStr {
    SmolStr::new(scope.as_str().to_ascii_uppercase())
}

fn invalid_text(value: &str, reason: InvalidTextReason) -> DisplayFormatError {
    DisplayFormatError::InvalidText {
        value: SmolStr::new(value),
        reason,
    }
}

fn validate_component_text(value: &str) -> Result<(), DisplayFormatError> {
    if value.is_empty() {
        return Err(invalid_text(value, InvalidTextReason::Empty));
    }
    if value.chars().any(|character| character.is_control()) {
        return Err(invalid_text(value, InvalidTextReason::ControlCharacter));
    }
    Ok(())
}

fn collapse_free_text(value: &str) -> SmolStr {
    let mut collapsed = String::with_capacity(value.len());
    for character in value.trim().chars() {
        if character.is_whitespace() {
            if !collapsed.is_empty() && !collapsed.ends_with(' ') {
                collapsed.push(' ');
            }
        } else {
            collapsed.push(character);
        }
    }
    SmolStr::new(collapsed)
}
