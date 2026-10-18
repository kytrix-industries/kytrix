//! The lexical layer of the display grammar.
//!
//! The pipeline fixes the order in which one dynamic component is processed:
//! validate the
//! typed domain, normalize free text and uppercase the raw value. The order is
//! part of the contract and is never rearranged.
//!
//! A component is rendered exactly as it was normalized. The grammar has no
//! escape and no marker, so a structure character inside a dynamic value is a
//! literal character of that value and never a grammar element: two names that
//! differ only in where a `/` came from are deliberately the same text.
//!
//! One component of a route is not a component of this pipeline at all: a venue
//! is a closed identity whose name is a message of the catalogue, so the pipeline
//! keeps its scope half here — a protocol namespace with one form in every
//! language — and leaves the venue half to the interface layer that owns the
//! catalogue.

use smol_str::SmolStr;

use crate::{
    display::values::{DisplayFormatError, InvalidTextReason},
    identifier::VenueScopeId,
};

/// One plain display component and the normalization its typed domain allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentText<'a> {
    /// Free text: the question of an outcome, an OutcomeRegistry member or side
    /// label, a verified security ticker and the code a route staged for an
    /// asset. Leading and trailing whitespace is trimmed and every run of Unicode
    /// whitespace collapses into one ASCII space before the rest of the pipeline
    /// runs.
    FreeText(&'a str),
    /// A typed value whose own constructor already rejects whitespace and control
    /// characters — an ISO 4217 code, a verified token code, a commodity code, an
    /// index namespace or code, an external reference. The formatter does not
    /// repeat that normalization.
    Typed(&'a str),
}

/// Normalizes one dynamic component text through the pipeline's rules.
///
/// The typed domain is validated first (non-empty, no control character), then
/// free text is trimmed and collapsed, and only then are ASCII letters
/// uppercased. Non-ASCII characters are kept as they are: the display layer
/// never transliterates.
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

/// Renders the user-visible form of one venue scope: the scope half of the
/// pipeline, with every `/`-separated segment uppercased, so `perp-dex/xyz` reads
/// `PERP-DEX/XYZ`.
///
/// A `/` is not a letter, so uppercasing the scope text states the same rule as
/// uppercasing it segment by segment, once. A scope is a protocol namespace
/// rather than a brand — it has one form in every language — so this is the
/// whole of its user-visible form; the venue half of a route is a name the
/// catalogue owns and is resolved by the interface layer instead.
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
