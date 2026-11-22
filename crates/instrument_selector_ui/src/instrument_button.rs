//! The control that opens the instrument selector.
//!
//! Every pane that follows one listing draws the same control: one button that states what is
//! followed and opens the selector when it is clicked. It lives here rather than in each pane
//! because the two things every caller has to agree on are the same everywhere — the text, and
//! the dialog the click opens. What the callers differ in is what they hand over: the listing they
//! follow, the filters and the routing snapshot their selector may offer, the message drawn before
//! anything is followed, and what a confirmation does with the choice.
//!
//! A control whose filters offer nothing is *disabled* rather than live: the loaded venues the
//! caller snapped are the only thing the selector can show, so with none of them serving what the
//! caller requires the market participant's metadata is not in memory yet and the control has nothing to
//! open. A disabled control is what says so — a click that opened nothing would read as broken,
//! and an alert on every click would be a second way of saying what the greyed control already
//! says (AGENTS.md §8). The kit button's `disabled` only paints the state, so a disabled control
//! is built **without** a click handler: nothing can dispatch an action it does not carry.
//!
//! The text is the published name of the listing and the route it is on:
//! `BTC/USDT:USDT @ BINANCE/SPOT`. The route is read from the caller's own `VenueListingKey` and
//! appended at the control rather than asked for as a longer name, because a published
//! `InstrumentDisplayString` states the economic identity of an instrument and never the listing
//! instance: two listings of one instrument differ in exactly the route and the listing id the
//! caller already holds, and the display layer deliberately has no ` @ <ROUTE>` of its own. The
//! `@` is punctuation this module states, not a sentence any locale translates.
//!
//! A venue the caller's routing registers a single scope for has nothing to tell apart, so its
//! route reads as the venue alone — `005546 @ XHKG` — while a venue with several scopes keeps the
//! whole `VENUE/SCOPE` form, which is what says which of them the listing is on. A listing the
//! published generation names nothing for reads as the localized unknown and states no route: the
//! name is what a reader looks a listing up by, and a control that named no listing has nothing to
//! place on a route.
//!
//! The control reads the catalogue the host was opened with; it never looks a directory root up.
//! A host without a catalogue (headless tests, an embedder with no directory) states the
//! placeholder and is disabled, exactly like a host whose snapshot offers no scope.

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

/// What a confirmed selection is handed to.
pub(crate) type ConfirmHandler = Rc<dyn Fn(InstrumentSelection, &mut App, WindowId) + 'static>;

/// The control that opens the instrument selector for one pane.
pub struct InstrumentButton {
    /// The kit component key of the button itself, which the caller owns so the control keeps its
    /// identity across the frames a header is rebuilt in.
    key: SmolStr,
    /// The listing the pane follows, or nothing before one has been chosen.
    selection: Option<InstrumentSelection>,
    /// What the selector this control opens offers, and the routing the route text is read from.
    filters: InstrumentFilters,
    /// The i18n key of the message drawn while nothing is followed: a control that stands for no
    /// listing states the offer rather than a name.
    placeholder: &'static str,
    /// What a confirmed selection is handed to.
    on_confirm: ConfirmHandler,
}

impl InstrumentButton {
    /// A control that opens the selector described by `filters` and hands what the user confirms
    /// to `on_confirm`.
    ///
    /// `placeholder` is the i18n key drawn while `selection` is `None`.
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

    /// The text this control states for `selection`.
    ///
    /// The name is the one the published generation gives the listing, resolved through the same
    /// boundary every other pane names a listing at, and the route is the one appended to it: see
    /// the module documentation for both. A listing that generation published no name for is the
    /// localized unknown, with no route beside it.
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

    /// Whether the control can open the selector at all.
    ///
    /// "Offer nothing" is the filters' own answer rather than "nothing is loaded": a venue that is
    /// loaded but serves none of what the caller requires leaves the same nothing to select from,
    /// and a control that opened a picker with nothing in it would be the same broken control. The
    /// control becomes live on the frame the first scope that admits it is in the snapshot — the
    /// pane reports the venue commit, and a header is rebuilt every frame.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        !self.filters.offers_anything()
    }

    /// The text the control states, against the catalogue the host was opened with.
    ///
    /// A host with no catalogue states the placeholder: there is nothing to name a listing from.
    fn label(&self, host: &SelectorHost) -> SmolStr {
        match (&self.selection, host.catalog()) {
            (Some(selection), Some(catalog)) => {
                Self::text(selection, &self.filters.routes, catalog, host.translate())
            }
            _ => i18n::t_smol!(host.translate(), self.placeholder),
        }
    }

    /// Builds the control under `parent`.
    ///
    /// The click opens the selector through [`InstrumentSelector::show_with_filters`], which
    /// returns `false` when the filters offer no route: the caller keeps its state and no dialog
    /// appears.
    ///
    /// # Errors
    /// Engine node-building errors (`UnknownWindow` / `StaleNode` / resource failures).
    pub fn build(
        self,
        host: &SelectorHost,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
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

        button.build(app, window, parent)
    }
}

/// The route a control states beside the name of the listing it follows.
///
/// `loaded` is every scope the caller's snapshot of the loaded venues holds, whatever venue each
/// one belongs to. A venue that snapshot loaded exactly one scope for has nothing to
/// disambiguate, so the route reads as that venue alone; a venue it loaded several for reads as
/// the whole `VENUE/SCOPE` form, because the scope is then part of what tells this listing's
/// route from another.
///
/// A venue that is not loaded at all reads as the venue alone: "not loaded" is a wait
/// rather than a venue that was proved to have several scopes, and a cold start must not state
/// every route of every venue in its full form.
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
