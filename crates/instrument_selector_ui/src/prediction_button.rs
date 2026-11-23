use std::rc::Rc;

use market_participants_model::prediction::PredictionEventRef;
use smol_str::SmolStr;
use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use ui_kit::kit::{
    button::Button,
    size::{UnifiedSizable, UnifiedSize},
};

use crate::{host::SelectorHost, prediction_selector::PredictionEventSelector};

pub(crate) type PredictionConfirm =
    Rc<dyn Fn(PredictionEventRef, SmolStr, &mut App, WindowId) + 'static>;

pub struct PredictionEventButton {
    key: SmolStr,
    selection: Option<PredictionEventRef>,
    label: Option<SmolStr>,
    placeholder: &'static str,
    on_select: PredictionConfirm,
}

impl PredictionEventButton {
    pub fn new(
        key: impl Into<SmolStr>,
        selection: Option<PredictionEventRef>,
        label: Option<SmolStr>,
        placeholder: &'static str,
        on_select: impl Fn(PredictionEventRef, SmolStr, &mut App, WindowId) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            selection,
            label,
            placeholder,
            on_select: Rc::new(on_select),
        }
    }

    pub fn build(
        self,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let text = self
            .label
            .clone()
            .unwrap_or_else(|| i18n::t_smol!(host.translate(), self.placeholder));
        let button = Button::new(app, host.theme().clone(), self.key)
            .primary()
            .with_unified_size(UnifiedSize::Small)
            .label(text.clone())
            .a11y_label(text)
            .on_click({
                let selection = self.selection;
                let label = self.label;
                let on_select = self.on_select;
                let host = host.clone();
                move |ctx| {
                    let window = ctx.window();
                    let app = ctx.app_mut();
                    let on_select = Rc::clone(&on_select);
                    PredictionEventSelector::show(
                        selection.clone(),
                        label.clone(),
                        Rc::new(move |event, title, app, window| {
                            on_select(event, title, app, window);
                        }),
                        &host,
                        app,
                        window,
                    );
                }
            });

        button.build(app, window, parent)
    }
}
