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
//! says (AGENTS.md §8).
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

use std::rc::Rc;

use base_ui::{button::Button, interactive::InteractiveVariantExt, size::UnifiedSizable};
use gpui_kit::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Window, base::Disableable, div, prelude::FluentBuilder,
};
use i18n::t;
use market_participants_connector::connector::MarketParticipantsConnector;
use market_participants_model::{
    listing::route::VenueRoute, market::selection::InstrumentSelection,
};
use smol_str::format_smolstr;
use widget::{
    error::{ListingErrorPresentation, LocalizedError},
    venue::{VenueDisplay as _, route_display_string},
};

use crate::instrument_selector::{InstrumentFilters, InstrumentSelector, LoadedScope};

/// The control that opens the instrument selector for one pane.
#[derive(IntoElement)]
pub struct InstrumentButton {
    /// The element id of the button itself, which the caller owns so the control keeps its
    /// identity across the frames a header is rebuilt in.
    id: ElementId,
    /// The listing the pane follows, or nothing before one has been chosen.
    selection: Option<InstrumentSelection>,
    /// What the selector this control opens offers, and the routing the route text is read from.
    filters: InstrumentFilters,
    /// The i18n key of the message drawn while nothing is followed: a control that stands for no
    /// listing states the offer rather than a name.
    placeholder: &'static str,
    /// What a confirmed selection is handed to. Held as a shared handler because a click is one of
    /// several the frame's control may receive.
    on_confirm: Rc<dyn Fn(InstrumentSelection, &mut Window, &mut App) + 'static>,
}

impl InstrumentButton {
    /// A control that opens the selector described by `filters` and hands what the user confirms
    /// to `on_confirm`.
    ///
    /// `placeholder` is the i18n key drawn while `selection` is `None`.
    pub fn new(
        id: impl Into<ElementId>,
        selection: Option<InstrumentSelection>,
        filters: InstrumentFilters,
        placeholder: &'static str,
        on_confirm: impl Fn(InstrumentSelection, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
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
    fn text(selection: &InstrumentSelection, scopes: &[LoadedScope], cx: &App) -> SharedString {
        let presentation = ListingErrorPresentation::of(
            cx.global::<MarketParticipantsConnector>()
                .catalog()
                .cached_listing_instrument(&selection.listing),
        );
        let Some(display) = presentation.display.as_ref() else {
            return presentation.localized_message(cx);
        };

        SharedString::new(format_smolstr!(
            "{} @ {}",
            display.as_str(),
            route_suffix(
                &selection.listing.route,
                scopes.iter().map(|scope| &scope.route),
                cx,
            )
        ))
    }
}

impl RenderOnce for InstrumentButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            selection,
            filters,
            placeholder,
            on_confirm,
        } = self;
        let text = selection.as_ref().map_or_else(
            || t!(cx, placeholder),
            |selection| Self::text(selection, &filters.routes, cx),
        );
        // Whether the market participant's metadata this control would offer is in memory at all. "Offer
        // nothing" is the filters' own answer rather than "nothing is loaded": a venue that is
        // loaded but serves none of what the caller requires leaves the same nothing to select
        // from, and a control that opened a picker with nothing in it would be the same broken
        // control. The control becomes live on the frame the first scope that admits it is in the
        // snapshot — the pane reports the venue commit, and a header is rebuilt every frame.
        let disabled = !filters.offers_anything();

        // The control occludes what it covers: it is a button in a header, and the space a header
        // is dragged by is the space around its controls rather than the controls themselves.
        div().occlude().child(
            Button::new(id)
                .filled()
                .small()
                .disabled(disabled)
                .aria_label(text.clone())
                .child(text)
                .when(!disabled, |button| {
                    button.on_click(move |_, window, app| {
                        InstrumentSelector::show_with_filters(
                            selection.clone(),
                            filters.clone(),
                            on_confirm.clone(),
                            window,
                            app,
                        );
                    })
                }),
        )
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
    cx: &App,
) -> SharedString {
    let scopes = loaded.filter(|loaded| loaded.venue == route.venue).count();

    if scopes <= 1 {
        route.venue.display_string(cx)
    } else {
        route_display_string(route, cx)
    }
}

#[cfg(test)]
mod tests {
    //! What the control states, and where its text comes from.

    use std::{sync::Arc, time::Duration};

    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{
        App, AppContext as _, Context, Entity, IntoElement, Modifiers, Render, SharedString,
        TestAppContext, VisualTestContext, Window, div, px, size,
    };
    use market_participants_connector::catalog::test_support;
    use market_participants_model::routing::CapabilityId;
    use widget_tokio_runtime::global;

    use super::*;
    use crate::{
        instrument_selector::{InstrumentKindFilter, InstrumentStatusFilter},
        test_support::{loaded, scope},
    };

    /// The route the fixture publishes on.
    fn fixture_route() -> VenueRoute {
        test_support::spot_route()
    }

    fn selection(symbol: &str) -> InstrumentSelection {
        InstrumentSelection {
            listing: test_support::listing_key(&fixture_route(), symbol),
        }
    }

    /// An app whose catalog has published one named listing on the fixture route.
    fn published(cx: &mut App) {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        global::install(cx).expect("test Tokio runtime should build");
        let directory = test_support::install(
            cx,
            vec![test_support::entry(
                &fixture_route(),
                test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                "BTCUSDT",
            )],
        );
        global::handle(cx)
            .handle()
            .block_on(directory.ensure())
            .expect("the fixture route should publish its catalog");
    }

    /// The scopes a fixture caller hands its selector: the fixture route beside
    /// another scope of the same venue, so the venue has one to state.
    fn several_scopes() -> Arc<[LoadedScope]> {
        let spot = fixture_route();
        let perpetual = test_support::route(test_support::VENUE, "usd-m-perp");

        loaded(vec![
            scope(spot, &[CapabilityId::Candlesticks]),
            scope(perpetual, &[CapabilityId::Candlesticks]),
        ])
    }

    /// The loaded scopes of a venue that loaded one route only.
    fn one_scope() -> Arc<[LoadedScope]> {
        loaded(vec![scope(fixture_route(), &[CapabilityId::Candlesticks])])
    }

    /// The loaded scopes of a venue that publishes a catalog but serves none of what this
    /// caller requires: it is loaded, so the snapshot is not empty, and still none of
    /// its scopes is one the selector may offer.
    fn loaded_without_the_required_capability() -> Arc<[LoadedScope]> {
        loaded(vec![scope(fixture_route(), &[CapabilityId::MarketTrades])])
    }

    /// The control states the name the publication gave the listing and the route it
    /// is on, because the route is what tells two listings of one name apart.
    #[gpui_kit::test]
    fn the_text_should_state_the_published_name_and_the_route_it_is_on(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let text = cx.update(|cx| {
            published(cx);

            InstrumentButton::text(&selection("BTCUSDT"), &several_scopes(), cx)
        });

        assert_eq!(text.as_ref(), "BTC/USDT @ Binance/SPOT");
        assert!(
            !text.contains("BTCUSDT") && !text.contains("listing-id/v1/"),
            "neither the venue's symbol nor an identity is a name: {text}"
        );
    }

    /// A venue the routing registers a single route for has nothing to tell apart, so
    /// the route it is on reads as the venue alone.
    #[gpui_kit::test]
    fn a_venue_registered_for_one_route_should_state_the_venue_alone(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let text = cx.update(|cx| {
            published(cx);

            InstrumentButton::text(&selection("BTCUSDT"), &one_scope(), cx)
        });

        assert_eq!(text.as_ref(), "BTC/USDT @ Binance");
        assert_eq!(
            cx.update(|cx| route_suffix(&fixture_route(), [&fixture_route()].into_iter(), cx))
                .as_str(),
            "Binance"
        );
    }

    /// A venue the routing registers several routes for keeps the whole route form,
    /// because the scope is what says which of them the listing is on — and the count
    /// is of every route the snapshot holds for that venue, whatever its capabilities.
    #[gpui_kit::test]
    fn a_venue_registered_for_several_routes_should_state_its_whole_route(cx: &mut TestAppContext) {
        let spot = fixture_route();
        let perpetual = test_support::route(test_support::VENUE, "usd-m-perp");
        let elsewhere = test_support::route("okx", "spot");

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");

            assert_eq!(
                route_suffix(&spot, [&spot, &perpetual].into_iter(), cx).as_str(),
                "Binance/SPOT"
            );
            assert_eq!(
                route_suffix(&spot, [&spot, &perpetual, &elsewhere].into_iter(), cx).as_str(),
                "Binance/SPOT",
                "another venue's routes are not this venue's routes"
            );
        });
    }

    /// A venue whose loaded scopes do not include the route states the venue alone: a route the
    /// snapshot does not hold is a market that is not loaded, and "not loaded" is a wait rather
    /// than a venue that was proved to have several scopes.
    #[gpui_kit::test]
    fn a_scope_of_several_segments_should_be_stated_whole(cx: &mut TestAppContext) {
        let perp_dex = test_support::route("hyperliquid", "perp-dex/xyz");
        let core = test_support::route("hyperliquid", "core-perp");

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");

            assert_eq!(
                route_suffix(&perp_dex, [&core, &perp_dex].into_iter(), cx).as_str(),
                "Hyperliquid/PERP-DEX/XYZ"
            );
            assert_eq!(
                route_suffix(&perp_dex, [].into_iter(), cx).as_str(),
                "Hyperliquid",
                "a venue with nothing loaded states its venue name alone, however many \
                 segments the scope it would state has"
            );
            assert_eq!(
                route_suffix(&perp_dex, [&perp_dex].into_iter(), cx).as_str(),
                "Hyperliquid",
                "a venue with one loaded scope has no scope to disambiguate, however \
                 many segments that scope has"
            );
        });
    }

    /// A listing the published generation names nothing for is the localized unknown,
    /// and states no route beside it: the name is what a reader looks a listing up
    /// by, and a control that named no listing has nothing to place on a route.
    #[gpui_kit::test]
    fn a_listing_no_generation_names_should_state_no_route(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (unknown, expected) = cx.update(|cx| {
            published(cx);
            let unnamed = selection("ETHUSDT");
            assert!(
                cx.global::<MarketParticipantsConnector>()
                    .catalog()
                    .cached_listing_instrument(&unnamed.listing)
                    .is_err(),
                "the fixture generation really published no name for the second listing"
            );

            (
                InstrumentButton::text(&unnamed, &[], cx),
                t!(cx, "error.display.unknown_listing"),
            )
        });

        assert_eq!(unknown, expected);
        assert!(
            !unknown.contains("Binance"),
            "a control that named no listing states no route: {unknown}"
        );
    }

    /// A generation republished after a text was stated names the listing again: the
    /// control reads the publication every time it states its text, so the superseded
    /// generation's string never covers the key the new one renamed.
    #[gpui_kit::test]
    fn a_republished_generation_should_rename_what_the_control_states(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (before, before_generation, after, after_generation, named) = cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");

            // Generation A: the route lists the key the control states against USDT.
            let directory = test_support::install(
                cx,
                vec![test_support::entry(
                    &fixture_route(),
                    test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                    "BTCUSDT",
                )],
            );
            global::handle(cx)
                .handle()
                .block_on(directory.ensure())
                .expect("the first generation should publish");
            let routes: Arc<[LoadedScope]> = loaded(vec![
                scope(fixture_route(), &[CapabilityId::Candlesticks]),
                scope(
                    test_support::route(test_support::VENUE, "usd-m-perp"),
                    &[CapabilityId::Candlesticks],
                ),
            ]);
            let before = InstrumentButton::text(&selection("BTCUSDT"), &routes, cx);
            let before_generation = directory.generation();

            // Generation B: the same key, listed against another quote asset, so the
            // name the generation publishes for it changes while the key does not.
            test_support::publish_fixture_route(
                directory.catalog(),
                &fixture_route(),
                vec![test_support::entry(
                    &fixture_route(),
                    test_support::spot_token(test_support::VENUE, "BTC", "USDC"),
                    "BTCUSDT",
                )],
            )
            .expect("the second generation should publish");

            let named = directory
                .catalog()
                .cached_listing_instrument(&selection("BTCUSDT").listing)
                .expect("the second generation names the listing");

            (
                before,
                before_generation,
                InstrumentButton::text(&selection("BTCUSDT"), &routes, cx),
                directory.generation(),
                named,
            )
        });

        assert_eq!(
            before.as_ref(),
            "BTC/USDT @ Binance/SPOT",
            "the first generation names the pair"
        );
        assert_eq!(
            after,
            SharedString::new(format_smolstr!("{} @ Binance/SPOT", named.as_str())),
            "the text is the name of the generation it was read from"
        );
        assert!(
            after_generation > before_generation,
            "a publication allocates a new generation"
        );
        assert!(
            !after.contains("USDT"),
            "the superseded generation's name must not cover the new key: {after}"
        );
    }

    /// The control's own identity, which is the element a test clicks.
    const TRIGGER: &str = "TEST_INSTRUMENT_BUTTON_TRIGGER";
    /// The selector's content area, as the dialog draws it. The selector's own tests read the
    /// same selector by name; this module states it because it is the dialog a click opens rather
    /// than the control this module draws.
    const SELECTOR_PANEL: &str = "INSTRUMENT_SELECTOR";

    /// The control as a pane's header mounts it: the button under a trigger that carries the
    /// debug selector a test clicks.
    struct ButtonRoot {
        id: ElementId,
        filters: InstrumentFilters,
    }

    impl Render for ButtonRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| TRIGGER.into())
                .child(InstrumentButton::new(
                    self.id.clone(),
                    None,
                    self.filters.clone(),
                    "instrument_selector.spot",
                    |_, _, _| {},
                ))
        }
    }

    /// A window whose root is a [`Layer`], the way the application mounts one, holding the
    /// fixture control. `routes` is the caller's own snapshot of the loaded scopes — the view a
    /// pane reads before it builds this control — so an empty one is a caller that has loaded
    /// nothing.
    ///
    /// The connector is installed but no route is loaded: a click must not reach it.
    fn harness(cx: &mut TestAppContext, routes: Arc<[LoadedScope]>) -> &mut VisualTestContext {
        cx.executor().allow_parking();
        let root = cx.update(|cx| -> Entity<ButtonRoot> {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
            let fixture = test_support::Fixture::unanswering();
            market_participants_connector::test_support::install_root(
                cx,
                Arc::clone(fixture.catalog_arc()),
            );

            cx.new(|_| ButtonRoot {
                id: ElementId::Name(SharedString::new_static("instrument-button")),
                filters: InstrumentFilters {
                    kind_filter: InstrumentKindFilter::All,
                    status_filter: InstrumentStatusFilter::Any,
                    required_capability: CapabilityId::Candlesticks,
                    entry_predicate: None,
                    routes,
                },
            })
        });
        let (_, cx) = cx.add_window_view(|_, _| Layer::new(root));
        cx.simulate_resize(size(px(800.0), px(600.0)));
        cx.run_until_parked();

        cx
    }

    /// Lets the frame the click may have started settle and repaints, so what is read back is
    /// what the click actually left on screen.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The control's one entry point while no market is loaded: the filters offer nothing, so the
    /// control is disabled and a click on it opens nothing at all — no selector to select from,
    /// and no alert telling the user what a disabled control already says.
    #[gpui_kit::test]
    fn a_control_with_no_market_loaded_should_be_disabled_and_open_nothing(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx, loaded(Vec::new()));
        let trigger = cx.debug_bounds(TRIGGER).expect("the control should render");
        cx.simulate_click(trigger.center(), Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(SELECTOR_PANEL).is_none(),
            "the selector must not be opened when there is nothing to select from"
        );
        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_none(),
            "a disabled control states itself; it does not answer a click with an alert"
        );
    }

    /// The same with a snapshot that is *not* empty: the loaded venue serves none of what this
    /// caller requires, so the filters offer no scope and the market participant's metadata is not usable
    /// here yet. The control is disabled on the same answer, and opens nothing.
    #[gpui_kit::test]
    fn a_control_whose_required_capability_is_not_served_should_be_disabled_and_open_nothing(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx, loaded_without_the_required_capability());
        assert!(
            !loaded_without_the_required_capability().is_empty(),
            "the fixture venue really is loaded, so the snapshot is not empty"
        );
        let trigger = cx.debug_bounds(TRIGGER).expect("the control should render");
        cx.simulate_click(trigger.center(), Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(SELECTOR_PANEL).is_none(),
            "a loaded venue that serves none of what the caller needs is still nothing to \
             select from"
        );
        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_none(),
            "the control says it is disabled rather than explaining a click it did not take"
        );
    }

    /// The other side of the same rule: a snapshot that offers the required capability is a
    /// control that is live, and the click opens the selector the caller described.
    #[gpui_kit::test]
    fn a_control_with_a_scope_to_offer_should_open_the_selector(cx: &mut TestAppContext) {
        let cx = harness(cx, one_scope());
        let trigger = cx.debug_bounds(TRIGGER).expect("the control should render");
        cx.simulate_click(trigger.center(), Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(SELECTOR_PANEL).is_some(),
            "a loaded scope that serves what the caller requires is something to select from"
        );
    }
}
