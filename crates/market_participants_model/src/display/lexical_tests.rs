//! Unit tests of the lexical layer.

use crate::{
    display::{
        lexical::{ComponentText, normalize_component_text, scope_display_code},
        values::{DisplayFormatError, InvalidTextReason},
    },
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
};

/// free text is trimmed, collapsed and uppercased.
#[test]
fn free_text_is_trimmed_collapsed_and_uppercased() {
    let question = normalize_component_text(ComponentText::FreeText(
        "  will BTC exceed 100k by 20251231  ",
    ))
    .expect("a question is renderable");
    assert_eq!(question.as_str(), "WILL BTC EXCEED 100K BY 20251231");

    let collapsed =
        normalize_component_text(ComponentText::FreeText("WILL\u{00a0}\u{2003}BTC 100K"))
            .expect("unicode whitespace is not a control character");
    assert_eq!(collapsed.as_str(), "WILL BTC 100K");
}

/// a typed value was normalized by its own constructor — which rejects
/// whitespace and control characters — so the formatter neither collapses nor
/// trims it a second time; only ASCII case is applied to it.
#[test]
fn typed_text_is_not_normalized_again() {
    let typed = normalize_component_text(ComponentText::Typed("btc/usdt"))
        .expect("a typed value is renderable");
    assert_eq!(typed.as_str(), "BTC/USDT");
    let internal =
        normalize_component_text(ComponentText::Typed("A|B")).expect("a typed value is renderable");
    assert_eq!(
        internal.as_str(),
        "A|B",
        "a typed value keeps every character its own constructor allowed"
    );
}

/// the grammar escapes nothing, so every structure character is a
/// literal character of the component it appears in.
#[test]
fn structure_characters_are_literal_component_text() {
    for value in [
        "A/B", "A:B", "A-B", "A@B", "A~B", "A[B]", "A(B)", "A,B", "A;B", "A=B", "A\\B",
    ] {
        assert_eq!(
            normalize_component_text(ComponentText::Typed(value))
                .expect("a structure character is renderable")
                .as_str(),
            value,
            "`{value}` is rendered as it is"
        );
        assert_eq!(
            normalize_component_text(ComponentText::FreeText(value))
                .expect("a structure character is renderable")
                .as_str(),
            value
        );
    }
    // A space is not a structure character and stays literal.
    assert_eq!(
        normalize_component_text(ComponentText::FreeText("A B"))
            .expect("a space is renderable")
            .as_str(),
        "A B"
    );
}

/// only ASCII letters are uppercased and non-ASCII text is kept as
/// it is, never transliterated.
#[test]
fn non_ascii_text_is_preserved() {
    assert_eq!(
        normalize_component_text(ComponentText::FreeText("ünïcode 中文"))
            .expect("non-ASCII text is renderable")
            .as_str(),
        "üNïCODE 中文"
    );
    assert_eq!(
        normalize_component_text(ComponentText::Typed("btc/中文"))
            .expect("non-ASCII text is renderable")
            .as_str(),
        "BTC/中文"
    );
}

/// a control character is refused in every component, and a format
/// character is not.
///
/// Only Cc is a control character: a zero-width space or a bidi override is a
/// format character (Cf), and the name is neither parsed nor required to be unique,
/// so the display layer carries it rather than refusing a question a venue really
/// asked.
#[test]
fn control_characters_are_rejected() {
    for value in ["a\nb", "a\tb", "a\u{7f}b", "\u{1}", "a\u{85}b"] {
        assert_eq!(
            normalize_component_text(ComponentText::Typed(value))
                .expect_err("a control character fails"),
            DisplayFormatError::InvalidText {
                value: smol_str::SmolStr::new(value),
                reason: InvalidTextReason::ControlCharacter,
            }
        );
        assert_eq!(
            normalize_component_text(ComponentText::FreeText(value))
                .expect_err("a control character fails"),
            DisplayFormatError::InvalidText {
                value: smol_str::SmolStr::new(value),
                reason: InvalidTextReason::ControlCharacter,
            }
        );
    }
    // A non-breaking space is whitespace, not a control character.
    assert_eq!(
        normalize_component_text(ComponentText::FreeText("A\u{00a0}B"))
            .expect("a non-breaking space is not a control character")
            .as_str(),
        "A B"
    );
    // A zero-width space and a right-to-left override are format characters.
    for value in ["A\u{200b}B", "A\u{202e}B"] {
        assert_eq!(
            normalize_component_text(ComponentText::FreeText(value))
                .expect("a format character is not a control character")
                .as_str(),
            value,
            "`{value}` is carried as the venue wrote it"
        );
    }
}

/// empty text is refused, whether it was empty or only
/// whitespace.
#[test]
fn empty_text_is_rejected() {
    for value in ["", "   ", "\u{00a0}"] {
        assert_eq!(
            normalize_component_text(ComponentText::FreeText(value))
                .expect_err("empty free text fails"),
            DisplayFormatError::InvalidText {
                value: smol_str::SmolStr::new(value),
                reason: InvalidTextReason::Empty,
            }
        );
    }
    assert_eq!(
        normalize_component_text(ComponentText::Typed("")).expect_err("empty typed text fails"),
        DisplayFormatError::InvalidText {
            value: smol_str::SmolStr::new_static(""),
            reason: InvalidTextReason::Empty,
        }
    );
}

/// a scope has a user-visible form of its own, which is the
/// canonical text uppercased — every segment of a multi-segment scope included.
///
/// The venue half of a route is not stated here: it is a name the catalogue owns
/// and the interface layer resolves, so the only spelling of it this crate has is
/// the canonical identity one.
#[test]
fn a_scope_display_code_uppercases_its_own_text() {
    let perp_dex = VenueScopeId::new("perp-dex/xyz").expect("perp-dex/xyz is a scope");
    assert_eq!(scope_display_code(&perp_dex).as_str(), "PERP-DEX/XYZ");
    let spot = VenueScopeId::new("spot").expect("spot is a scope");
    assert_eq!(scope_display_code(&spot).as_str(), "SPOT");

    // The canonical identity spelling of each half is unchanged.
    assert_eq!(VenueId::Hyperliquid.to_string(), "hyperliquid");
    assert_eq!(perp_dex.to_string(), "perp-dex/xyz");
}

/// The canonical route spelling is what this crate states, and it is not a
/// user-visible form: `binance/spot` stays lowercase whatever the interface
/// shows.
#[test]
fn a_route_states_its_canonical_spelling() {
    let binance = VenueRoute::new(
        VenueId::Binance,
        VenueScopeId::new("spot").expect("spot is a scope"),
    );
    assert_eq!(binance.to_string(), "binance/spot");
    let hip3 = VenueRoute::new(
        VenueId::Hyperliquid,
        VenueScopeId::new("perp-dex/xyz").expect("perp-dex/xyz is a scope"),
    );
    assert_eq!(hip3.to_string(), "hyperliquid/perp-dex/xyz");
}
