//! What a region says when it holds nothing.
//!
//! An empty region and a broken one leave the same pixels behind, so anything that
//! can hold nothing has to say which of the two the user is looking at. This is
//! that sentence, and it is the only thing it is: the colour, the weight and the
//! centring are the same everywhere, so the same emptiness reads the same in a
//! table body, a list and a chart that has drawn nothing.
//!
//! It places no size of its own. The region it stands in decides that, because
//! only the region knows whether it is a table body's whole box, a list's first
//! row, or a rectangle over a canvas — so a caller either gives it a box to fill
//! or centres it in one. What it refuses to do is draw a background, a border or a
//! radius: it stands *in* a surface that already has them.

use gpui_kit::*;

use crate::style::theme::ThemeExt;

/// The sentence a region with nothing in it draws.
///
/// The text is the caller's, because it is the caller that knows what is empty —
/// a table with no rows, a feed with no reports, a chart with no candles. What is
/// shared is how it reads.
#[derive(IntoElement)]
pub struct EmptyState {
    id: ElementId,
    text: SharedString,
}

impl EmptyState {
    /// Draws `text` as the reason a region holds nothing.
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
        }
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { id, text } = self;

        div()
            .id(id)
            .debug_selector(|| "EMPTY_STATE".into())
            // The reason, not a status: it says what is not there rather than what
            // the region is doing, so it carries no role and is not announced as one.
            .flex_none()
            .min_w_0()
            .max_w_full()
            .px_4()
            .text_sm()
            .text_color(cx.theme().roles.fg.muted.hsla)
            .child(text)
    }
}
