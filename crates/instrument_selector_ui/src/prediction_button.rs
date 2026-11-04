//! The control that opens the prediction-event selector.

use std::rc::Rc;

use base_ui::{button::Button, interactive::InteractiveVariantExt, size::UnifiedSizable};
use gpui_kit::{
    App, ElementId, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Window,
};
use i18n::t;
use market_participants_model::prediction::PredictionEventRef;
use smol_str::SmolStr;

use crate::prediction_selector::PredictionEventSelector;

/// One button that states the event a pane follows and opens the selector.
///
/// The label is the event title. A selected event is never named by its native
/// id here; a caller that has no title shows the placeholder instead.
#[derive(IntoElement)]
pub struct PredictionEventButton {
    id: ElementId,
    selection: Option<PredictionEventRef>,
    label: Option<SmolStr>,
    placeholder: &'static str,
    on_select: Rc<dyn Fn(PredictionEventRef, SmolStr, &mut App) + 'static>,
}

impl PredictionEventButton {
    /// Builds the control.
    pub fn new(
        id: impl Into<ElementId>,
        selection: Option<PredictionEventRef>,
        label: Option<SmolStr>,
        placeholder: &'static str,
        on_select: impl Fn(PredictionEventRef, SmolStr, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            selection,
            label,
            placeholder,
            on_select: Rc::new(on_select),
        }
    }
}

impl RenderOnce for PredictionEventButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            selection,
            label,
            placeholder,
            on_select,
        } = self;
        let text = label
            .clone()
            .map(SharedString::new)
            .unwrap_or_else(|| t!(cx, placeholder));
        Button::new(id)
            .filled()
            .small()
            .child(text)
            .on_click(move |_, window, cx| {
                let on_select = on_select.clone();
                let title = label.clone();
                PredictionEventSelector::show(
                    selection.clone(),
                    title,
                    Rc::new(move |event, title, _, app| on_select(event, title, app)),
                    window,
                    cx,
                );
            })
    }
}
