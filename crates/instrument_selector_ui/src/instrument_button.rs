use std::rc::Rc;

use base_ui::kit::{
    button::Button,
    size::{UnifiedSizable, UnifiedSize},
};
use i18n::translate::Translate;
use market_participants_connector::catalog::store::Catalog;
use market_participants_model::{
    listing::route::VenueRoute, market::selection::InstrumentSelection,
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use widget::{
    error::{ListingErrorPresentation, TranslatedError},
    venue::{VenueDisplay as _, route_display_string},
};

use crate::{
    host::SelectorHost,
    instrument_selector::{InstrumentFilters, InstrumentSelector, LoadedScope},
};

pub(crate) type ConfirmHandler = Rc<dyn Fn(InstrumentSelection, &mut App, WindowId) + 'static>;

pub struct InstrumentButton {
    key: SmolStr,
    selection: Option<InstrumentSelection>,
    filters: InstrumentFilters,
    placeholder: &'static str,
    on_confirm: ConfirmHandler,
}

impl InstrumentButton {
    pub fn new(
        key: impl Into<SmolStr>,
        selection: Option<InstrumentSelection>,
        filters: InstrumentFilters,
        placeholder: &'static str,
        on_confirm: impl Fn(InstrumentSelection, &mut App, WindowId) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            selection,
            filters,
            placeholder,
            on_confirm: Rc::new(on_confirm),
        }
    }

    fn text(
        selection: &InstrumentSelection,
        scopes: &[LoadedScope],
        catalog: &Catalog,
        translate: &Translate,
    ) -> SmolStr {
        let presentation =
            ListingErrorPresentation::of(catalog.cached_listing_instrument(&selection.listing));
        let Some(display) = presentation.display.as_ref() else {
            return presentation.translated_message(translate);
        };

        format_smolstr!(
            "{} @ {}",
            display.as_str(),
            route_suffix(
                &selection.listing.route,
                scopes.iter().map(|scope| &scope.route),
                translate,
            )
        )
    }

    #[must_use]
    pub fn is_disabled(&self) -> bool {
        !self.filters.offers_anything()
    }

    fn label(&self, host: &SelectorHost) -> SmolStr {
        match (&self.selection, host.catalog()) {
            (Some(selection), Some(catalog)) => {
                Self::text(selection, &self.filters.routes, catalog, host.translate())
            }
            _ => i18n::t_smol!(host.translate(), self.placeholder),
        }
    }

    pub fn build(
        self,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<InstrumentButtonHandle, EngineError> {
        let text = self.label(host);
        let disabled = self.is_disabled();
        let button = Button::new(app, host.theme().clone(), self.key)
            .primary()
            .with_unified_size(UnifiedSize::Small)
            .label(text.clone())
            .a11y_label(text)
            .disabled(disabled);
        let button = if disabled {
            button
        } else {
            let selection = self.selection;
            let filters = self.filters;
            let on_confirm = self.on_confirm;
            let host = host.clone();
            button.on_click(move |ctx| {
                let window = ctx.window();
                let app = ctx.app_mut();
                InstrumentSelector::show_with_filters(
                    selection.clone(),
                    filters.clone(),
                    on_confirm.clone(),
                    &host,
                    app,
                    window,
                );
            })
        };

        button
            .build(app, window, parent)
            .map(|node| InstrumentButtonHandle { button, node })
    }
}

pub struct InstrumentButtonHandle {
    button: Button,
    node: NodeId,
}

impl InstrumentButtonHandle {
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.button.release(app)
    }
}

fn route_suffix<'a>(
    route: &VenueRoute,
    loaded: impl Iterator<Item = &'a VenueRoute>,
    translate: &Translate,
) -> SmolStr {
    let scopes = loaded.filter(|loaded| loaded.venue == route.venue).count();

    if scopes <= 1 {
        route.venue.display_string(translate)
    } else {
        route_display_string(route, translate)
    }
}

#[cfg(test)]
mod tests;
