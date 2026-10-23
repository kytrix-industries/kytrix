//! Tests for the identifiers the panel's rows are drawn under.
//!
//! These are the names the window stores state under, so what is pinned here is
//! the part that makes that state survive a frame: one card is one name, and the
//! name is made of nothing that moves.
//!
//! The last test is not about an identifier: it holds the padding the rows assume a
//! checkbox draws to the padding the checkbox actually draws, which is the number
//! the whole settings column is aligned on.

use base_ui::size::UnifiedSize;
use gpui_kit::{ElementId, SharedString};

use super::rows::{CHECKBOX_PADDING, card_id};
use crate::indicator::INDICATOR_CATALOG;

/// The checkbox's padding, as the rows assume it and as the control draws it.
///
/// Every settings row aligns on the square inside the indicator's checkbox, and the
/// square sits one padding inside the checkbox's box. The rows cannot call
/// `UnifiedSize::Small.px()` — a `const` may not — so the number is written out
/// beside them; this is what keeps the written number and the control from drifting
/// apart. A `UnifiedSize::Small` that changed its padding would otherwise move the
/// square while every label stayed where it was, and the column would be off by the
/// difference with nothing failing.
#[test]
fn the_rows_should_assume_the_padding_a_small_checkbox_draws() {
    assert_eq!(
        CHECKBOX_PADDING,
        UnifiedSize::Small.px().as_f32(),
        "the row's alignment takes one checkbox padding off its inset, so it has to \
         be the padding `UnifiedSize::Small` gives a checkbox"
    );
}

/// Every card is named after its place in the catalogue, and only that.
///
/// Sharing one id between two cards is not a wrong colour or a misplaced box: the
/// window keys the hover state and both transitions by this id, so two cards under
/// one name would write each other's state and the test binary hangs rather than
/// failing. That is why the ids are asked for directly here instead of being read
/// off a frame: the failure this test exists for is one that never produces a
/// frame.
#[test]
fn every_card_should_have_an_id_of_its_own() {
    let indices: Vec<usize> = (0..INDICATOR_CATALOG.len()).collect();
    let ids: Vec<ElementId> = indices.iter().map(|index| card_id(*index)).collect();

    for (position, id) in ids.iter().enumerate() {
        assert!(
            !ids[..position].contains(id),
            "{id:?} is the id of two cards, so their hover state and transitions \
             would be one"
        );
    }
}

/// The same card is drawn under the same name on every frame.
///
/// The id carries the pointer's hover and the two transitions that follow it, all
/// of which live across frames. An id that moved would reset them: the card would
/// start every frame from its resting colour and the fade would never run. The
/// calls are interleaved because a counter handed out per call — the shape a
/// "make it unique" fix usually takes — survives asking for one id twice and only
/// shows itself once another card is asked for in between.
#[test]
fn a_card_should_keep_its_id_across_frames() {
    let first = card_id(2);
    let other = card_id(3);
    let again = card_id(2);

    assert_eq!(
        first, again,
        "a card asked for twice is one card: {first:?} against {again:?}"
    );
    assert_ne!(
        first, other,
        "and asking for another card in between changes nothing: {first:?} against \
         {other:?}"
    );
}

/// The name is the catalogue index and a fixed prefix, nothing else.
///
/// Read structurally rather than by comparing two calls, because the thing that
/// must not be in here is a value the panel could hand out differently on a later
/// frame — a slot counter, an entity id, a window id. Any of those would make the
/// two calls above agree today and disagree after a redraw, so the name itself is
/// what has to be pinned.
#[test]
fn a_card_id_should_be_its_index_under_a_fixed_name() {
    for index in 0..INDICATOR_CATALOG.len() {
        assert_eq!(
            card_id(index),
            ElementId::NamedInteger(
                SharedString::new_static("CandlestickIndicatorCard"),
                index as u64
            ),
            "the card at {index} is named after its index and nothing that moves"
        );
    }
}
