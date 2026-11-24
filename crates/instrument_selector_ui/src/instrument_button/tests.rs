use std::sync::Arc;

use market_participants_connector::catalog::test_support;
use market_participants_model::routing::CapabilityId;
use ui_core::math::LogicalPoint;

use super::*;
use crate::{
    instrument_selector::{InstrumentKindFilter, InstrumentStatusFilter, LoadedScope},
    test_support::{catalog_with, loaded, scope, translate},
};

fn fixture_route() -> VenueRoute {
    test_support::spot_route()
}

fn selection(symbol: &str) -> InstrumentSelection {
    InstrumentSelection {
        listing: test_support::listing_key(&fixture_route(), symbol),
    }
}

fn published() -> Arc<Catalog> {
    catalog_with(
        &fixture_route(),
        vec![test_support::entry(
            &fixture_route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )],
    )
}

fn several_scopes() -> Arc<[LoadedScope]> {
    let spot = fixture_route();
    let perpetual = test_support::route(test_support::VENUE, "usd-m-perp");

    loaded(vec![
        scope(spot, &[CapabilityId::Candlesticks]),
        scope(perpetual, &[CapabilityId::Candlesticks]),
    ])
}

fn one_scope() -> Arc<[LoadedScope]> {
    loaded(vec![scope(fixture_route(), &[CapabilityId::Candlesticks])])
}

fn loaded_without_the_required_capability() -> Arc<[LoadedScope]> {
    loaded(vec![scope(fixture_route(), &[CapabilityId::MarketTrade])])
}

fn filters(routes: Arc<[LoadedScope]>) -> InstrumentFilters {
    InstrumentFilters {
        kind_filter: InstrumentKindFilter::All,
        status_filter: InstrumentStatusFilter::Any,
        required_capability: CapabilityId::Candlesticks,
        entry_predicate: None,
        routes,
    }
}

#[test]
fn the_text_should_state_the_published_name_and_the_route_it_is_on() {
    let text = InstrumentButton::text(
        &selection("BTCUSDT"),
        &several_scopes(),
        &published(),
        &translate(),
    );

    assert_eq!(text, "BTC/USDT @ Binance/SPOT");
    assert!(
        !text.contains("BTCUSDT") && !text.contains("listing-id/v1/"),
        "neither the venue's symbol nor an identity is a name: {text}"
    );
}

#[test]
fn a_venue_registered_for_one_route_should_state_the_venue_alone() {
    let text = InstrumentButton::text(
        &selection("BTCUSDT"),
        &one_scope(),
        &published(),
        &translate(),
    );

    assert_eq!(text, "BTC/USDT @ Binance");
    assert_eq!(
        route_suffix(
            &fixture_route(),
            [&fixture_route()].into_iter(),
            &translate()
        ),
        "Binance"
    );
}

#[test]
fn a_venue_registered_for_several_routes_should_state_its_whole_route() {
    let spot = fixture_route();
    let perpetual = test_support::route(test_support::VENUE, "usd-m-perp");
    let elsewhere = test_support::route("okx", "spot");
    let translate = translate();

    assert_eq!(
        route_suffix(&spot, [&spot, &perpetual].into_iter(), &translate),
        "Binance/SPOT"
    );
    assert_eq!(
        route_suffix(
            &spot,
            [&spot, &perpetual, &elsewhere].into_iter(),
            &translate
        ),
        "Binance/SPOT",
        "another venue's routes are not this venue's routes"
    );
}

#[test]
fn a_scope_of_several_segments_should_be_stated_whole() {
    let perp_dex = test_support::route("hyperliquid", "perp-dex/xyz");
    let core = test_support::route("hyperliquid", "core-perp");
    let translate = translate();

    assert_eq!(
        route_suffix(&perp_dex, [&core, &perp_dex].into_iter(), &translate),
        "Hyperliquid/PERP-DEX/XYZ"
    );
    assert_eq!(
        route_suffix(&perp_dex, [].into_iter(), &translate),
        "Hyperliquid",
        "a venue with nothing loaded states its venue name alone, however many \
             segments the scope it would state has"
    );
    assert_eq!(
        route_suffix(&perp_dex, [&perp_dex].into_iter(), &translate),
        "Hyperliquid",
        "a venue with one loaded scope has no scope to disambiguate, however \
             many segments that scope has"
    );
}

#[test]
fn a_listing_no_generation_names_should_state_no_route() {
    let catalog = published();
    let unnamed = selection("ETHUSDT");
    assert!(
        catalog.cached_listing_instrument(&unnamed.listing).is_err(),
        "the fixture generation really published no name for the second listing"
    );

    let unknown = InstrumentButton::text(&unnamed, &[], &catalog, &translate());

    assert_eq!(
        unknown,
        i18n::t_smol!(translate(), "error.display.unknown_listing")
    );
    assert!(
        !unknown.contains("Binance"),
        "a control that named no listing states no route: {unknown}"
    );
}

#[test]
fn a_republished_generation_should_rename_what_the_control_states() {
    let catalog = published();
    let routes = loaded(vec![
        scope(fixture_route(), &[CapabilityId::Candlesticks]),
        scope(
            test_support::route(test_support::VENUE, "usd-m-perp"),
            &[CapabilityId::Candlesticks],
        ),
    ]);
    let before = InstrumentButton::text(&selection("BTCUSDT"), &routes, &catalog, &translate());
    let before_generation = catalog.generation_id();

    test_support::publish_fixture_route(
        &catalog,
        &fixture_route(),
        vec![test_support::entry(
            &fixture_route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDC"),
            "BTCUSDT",
        )],
    )
    .expect("the second generation should publish");

    let named = catalog
        .cached_listing_instrument(&selection("BTCUSDT").listing)
        .expect("the second generation names the listing");
    let after = InstrumentButton::text(&selection("BTCUSDT"), &routes, &catalog, &translate());

    assert_eq!(
        before, "BTC/USDT @ Binance/SPOT",
        "the first generation names the pair"
    );
    assert_eq!(
        after,
        format_smolstr!("{} @ Binance/SPOT", named.as_str()),
        "the text is the name of the generation it was read from"
    );
    assert!(
        catalog.generation_id() > before_generation,
        "a publication allocates a new generation"
    );
    assert!(
        !after.contains("USDT"),
        "the superseded generation's name must not cover the new key: {after}"
    );
}

#[test]
fn only_a_snapshot_that_offers_a_scope_leaves_the_control_live() {
    assert!(
        InstrumentButton::new("x", None, filters(loaded(Vec::new())), "p", |_, _, _| {})
            .is_disabled()
    );
    assert!(
        InstrumentButton::new(
            "x",
            None,
            filters(loaded_without_the_required_capability()),
            "p",
            |_, _, _| {}
        )
        .is_disabled(),
        "a loaded venue that serves none of what the caller needs is still nothing to \
             select from"
    );
    assert!(
        !InstrumentButton::new("x", None, filters(one_scope()), "p", |_, _, _| {}).is_disabled()
    );
}

#[test]
fn a_click_opens_the_selector_only_when_the_control_is_live() {
    let mut live = crate::test_support::Harness::new();
    let node = {
        let host = live.host().clone();
        let window = live.window();
        let root = live.root();
        let app = live.app_mut();
        InstrumentButton::new(
            "instrument-button",
            None,
            filters(one_scope()),
            "instrument_selector.spot",
            |_, _, _| {},
        )
        .build(&host, app, window, root)
        .expect("the control builds")
        .node()
    };
    live.drain();
    let bounds = live.bounds(node).expect("the control is laid out");
    let centre = LogicalPoint::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    live.click(centre);
    live.drain();

    assert!(
        !live.app().overlays(live.window()).is_empty(),
        "a live control opens a dialog"
    );

    let mut disabled = crate::test_support::Harness::new();
    let node = {
        let host = disabled.host().clone();
        let window = disabled.window();
        let root = disabled.root();
        let app = disabled.app_mut();
        InstrumentButton::new(
            "instrument-button",
            None,
            filters(loaded(Vec::new())),
            "instrument_selector.spot",
            |_, _, _| {},
        )
        .build(&host, app, window, root)
        .expect("the control builds")
        .node()
    };
    disabled.drain();
    let bounds = disabled.bounds(node).expect("the control is laid out");
    let centre = LogicalPoint::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    disabled.click(centre);
    disabled.drain();

    assert!(
        disabled.app().overlays(disabled.window()).is_empty(),
        "a disabled control states itself; it does not answer a click with a dialog"
    );
}

#[test]
fn the_placeholder_is_a_catalogue_key_that_resolves() {
    {
        let language = i18n::language::codes::LanguageCode::En;
        let translate = crate::test_support::translate_in(language);
        assert!(
            !i18n::t_smol!(translate, "instrument_selector.spot").is_empty(),
            "the placeholder key must resolve in {language}"
        );
    }
}

#[test]
fn releasing_the_control_should_reclaim_its_component() {
    let mut harness = crate::test_support::Harness::new();
    let action = ui_core::event::Action::Custom(SmolStr::new_static("instrument-button.click"));
    let stats = |harness: &crate::test_support::Harness| {
        ui_core::test_support::diagnostics::signal_slot_stats(harness.app())
    };
    let baseline = stats(&harness).live;

    for cycle in 0..10 {
        let handle = {
            let host = harness.host().clone();
            let window = harness.window();
            let root = harness.root();
            let app = harness.app_mut();
            InstrumentButton::new(
                "instrument-button",
                None,
                filters(one_scope()),
                "instrument_selector.spot",
                |_, _, _| {},
            )
            .build(&host, app, window, root)
            .expect("the control builds")
        };
        assert!(
            harness.app().has_action_handler(&action),
            "cycle {cycle}: a live control registers the click it carries"
        );

        let window = harness.window();
        harness
            .app_mut()
            .remove_node(window, handle.node())
            .expect("the caller removes the control's node first");
        handle.release(harness.app_mut());

        assert!(
            !harness.app().has_action_handler(&action),
            "cycle {cycle}: release unregisters the click the control registered"
        );
        assert_eq!(
            stats(&harness).live,
            baseline,
            "cycle {cycle}: the component's signal scope came back"
        );
    }
}
