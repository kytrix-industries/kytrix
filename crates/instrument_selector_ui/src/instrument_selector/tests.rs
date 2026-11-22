//! The selector's pure decisions, and the dialog it is mounted in.
//!
//! The pure tests drive the free functions directly; the dialog tests open the real kit
//! dialog against the crate's test host and read the engine tree back
//! ([`Harness::find`] / [`Harness::text_of`]), which is what the old bounds lookups
//! assertions became.

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use market_participants_connector::catalog::test_support;
use market_participants_model::{
    decimal::UnixMillis,
    instrument::Instrument,
    listing::{listing_impl::ListingStatus, route::VenueListingKey},
    market::selection::InstrumentSelection,
    routing::CapabilityId,
};
use smol_str::format_smolstr;
use ui_core::math::Point;

use super::*;
use crate::test_support::{Harness, catalog_with, loaded, scope};

fn route(name: &str, scope: &str) -> VenueRoute {
    test_support::route(name, scope)
}

fn listing_key(route: &VenueRoute, symbol: &str) -> VenueListingKey {
    test_support::listing_key(route, symbol)
}

fn selection(route: &VenueRoute, symbol: &str) -> InstrumentSelection {
    InstrumentSelection {
        listing: listing_key(route, symbol),
    }
}

/// The loaded scopes the fixtures are described by: two Binance scopes and one OKX scope.
fn registry() -> Arc<[LoadedScope]> {
    loaded(vec![
        scope(
            route("binance", "spot"),
            &[CapabilityId::Candlesticks, CapabilityId::Tickers],
        ),
        scope(
            route("binance", "usd-m-perp"),
            &[CapabilityId::Candlesticks, CapabilityId::ForceOrder],
        ),
        scope(route("okx", "spot"), &[CapabilityId::Candlesticks]),
    ])
}

fn empty_registry() -> Arc<[LoadedScope]> {
    loaded(Vec::new())
}

fn filters(
    kind_filter: InstrumentKindFilter,
    required_capability: CapabilityId,
) -> InstrumentFilters {
    InstrumentFilters {
        kind_filter,
        status_filter: InstrumentStatusFilter::Any,
        required_capability,
        entry_predicate: None,
        routes: registry(),
    }
}

fn spot(base: &str, quote: &str) -> Instrument {
    test_support::spot_token(test_support::VENUE, base, quote)
}

const CONFIRMED_AT: UnixMillis = 1_700_000_000_000;

fn entry(
    route: &VenueRoute,
    symbol: &str,
    instrument: Instrument,
    status: ListingStatus,
) -> CatalogEntry {
    test_support::with_status(test_support::entry(route, instrument, symbol), status)
}

/// One row of a listing whose generation published no name for it.
fn unnamed_row(
    route: &VenueRoute,
    symbol: &str,
    instrument: Instrument,
    status: ListingStatus,
) -> ListingRow {
    let entry = entry(route, symbol, instrument, status);
    ListingRow {
        route_order: format_smolstr!("{}", entry.listing.key.route),
        display: None,
        aliases: Arc::default(),
        entry,
    }
}

/// One unnamed row of `entry`, ordered under `route_order`.
fn ordered_under(entry: &CatalogEntry, route_order: &str) -> ListingRow {
    ListingRow {
        route_order: SmolStr::new(route_order),
        display: None,
        aliases: Arc::default(),
        entry: entry.clone(),
    }
}

fn row_keys(rows: &[ListingRow]) -> Vec<VenueListingKey> {
    rows.iter()
        .map(|row| row.entry.listing.key.clone())
        .collect()
}

fn row_names(rows: &[ListingRow]) -> Vec<&str> {
    rows.iter()
        .map(|row| {
            row.display
                .as_ref()
                .expect("a published fixture names every row it offers")
                .as_str()
        })
        .collect()
}

/// The rows one published fixture offers under `filters`.
fn fixture_rows(
    catalog: &market_participants_connector::catalog::store::Catalog,
    route: &VenueRoute,
    entries: Vec<CatalogEntry>,
    filters: &InstrumentFilters,
    entry_predicate: Option<&EntryPredicate>,
) -> Vec<ListingRow> {
    let display = catalog.cached_display();
    rows_of(&display, route, entries, filters, entry_predicate)
}

/// Whether the matcher answers `query` with the one row whose column is `column`.
fn matcher_hits(column: &str, query: &str) -> bool {
    let mut matcher: Nucleo<()> = Nucleo::new(
        MatcherConfig::DEFAULT,
        Arc::new(|| {}),
        Some(MATCHER_THREADS),
        MATCHER_COLUMNS,
    );
    let injector = matcher.injector();
    injector.push((), |_, columns| columns[0] = Utf32String::from(column));
    injector.push((), |_, columns| columns[0] = Utf32String::from(""));
    drop(injector);
    matcher
        .pattern
        .reparse(0, query, CaseMatching::Ignore, Normalization::Smart, false);
    while matcher.tick(10).running {}

    matcher.snapshot().matched_item_count() == 1
}

#[test]
fn kind_filters_should_apply_to_every_venue() {
    let spot_filters = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);

    assert_eq!(
        spot_filters
            .offered()
            .iter()
            .map(|scope| scope.route.clone())
            .collect::<Vec<_>>(),
        vec![
            route("binance", "spot"),
            route("binance", "usd-m-perp"),
            route("okx", "spot"),
        ]
    );
    assert!(spot_filters.kind_filter.allows(InstrumentKind::Spot));
    assert!(!spot_filters.kind_filter.allows(InstrumentKind::Perpetual));
    assert!(
        InstrumentKindFilter::All.allows(InstrumentKind::Outcome),
        "the default filter offers every family"
    );
}

#[test]
fn capability_filter_should_exclude_routes_that_do_not_declare_it() {
    let force_order = filters(InstrumentKindFilter::Perpetual, CapabilityId::ForceOrder);

    assert_eq!(
        force_order
            .offered()
            .iter()
            .map(|scope| scope.route.clone())
            .collect::<Vec<_>>(),
        vec![route("binance", "usd-m-perp")]
    );
}

#[test]
fn initial_route_should_fall_back_to_first_permitted_route() {
    let okx_spot = route("okx", "spot");

    assert_eq!(
        initial_route(
            Some(&okx_spot),
            &filters(InstrumentKindFilter::Spot, CapabilityId::ForceOrder)
        )
        .as_ref(),
        Some(&route("binance", "usd-m-perp"))
    );
    assert_eq!(
        initial_route(
            None,
            &filters(InstrumentKindFilter::All, CapabilityId::Candlesticks)
        )
        .as_ref(),
        Some(&route("binance", "spot"))
    );
    assert_eq!(
        initial_route(
            None,
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks).with(empty_registry())
        ),
        None,
        "a registry with nothing in it offers no route to open on"
    );
}

impl InstrumentFilters {
    /// The same filters over another registry snapshot.
    fn with(mut self, routes: Arc<[LoadedScope]>) -> Self {
        self.routes = routes;
        self
    }
}

#[test]
fn initial_listing_should_be_cleared_after_route_fallback() {
    let binance_spot = route("binance", "spot");

    assert_eq!(
        initial_listing(
            &Some(selection(&binance_spot, "BTCUSDT")),
            &route("binance", "usd-m-perp")
        ),
        None,
        "a listing key means something else on another route and is never translated"
    );
    assert_eq!(
        initial_listing(&Some(selection(&binance_spot, "BTCUSDT")), &binance_spot),
        Some(listing_key(&binance_spot, "BTCUSDT"))
    );
    assert_eq!(initial_listing(&None, &binance_spot), None);
}

#[test]
fn confirmation_should_refuse_a_listing_the_catalog_does_not_hold() {
    let binance_spot = route("binance", "spot");
    let published = listing_key(&binance_spot, "BTCUSDT");
    let absent = listing_key(&binance_spot, "ETHUSDT");
    let rows = vec![unnamed_row(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    )];

    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            &rows,
            Some(&published),
            CONFIRMED_AT
        ),
        Ok(InstrumentSelection { listing: published })
    );
    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            &rows,
            Some(&absent),
            CONFIRMED_AT
        ),
        Err(SelectionRejection::ListingNotOffered { listing: absent })
    );
}

#[test]
fn confirmation_should_refuse_a_route_the_registry_revoked() {
    let revoked = route("hyperliquid", "core-perp");
    let rows = vec![unnamed_row(
        &revoked,
        "BTCUSD",
        spot("BTC", "USD"),
        ListingStatus::Trading,
    )];

    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            &rows,
            Some(&listing_key(&revoked, "BTCUSD")),
            CONFIRMED_AT
        ),
        Err(SelectionRejection::RouteNotOffered { route: revoked })
    );
}

#[test]
fn confirmation_should_refuse_a_status_the_filters_do_not_offer() {
    let binance_spot = route("binance", "spot");
    let halted = listing_key(&binance_spot, "BTCUSDT");
    let rows = vec![unnamed_row(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Halted,
    )];
    let mut tradable = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);
    tradable.status_filter = InstrumentStatusFilter::Tradable;

    assert_eq!(
        confirmed_selection(&tradable, &rows, Some(&halted), CONFIRMED_AT),
        Err(SelectionRejection::StatusNotOffered {
            listing: halted,
            status: ListingStatus::Halted,
        })
    );
}

/// A listing that declares a window is only selectable inside it: the window is
/// part of what a selection satisfies, not only what the list was filtered by.
#[test]
fn confirmation_should_refuse_a_listing_whose_window_has_not_opened() {
    let binance_spot = route("binance", "spot");
    let listing = listing_key(&binance_spot, "BTCUSDT");
    let mut row = unnamed_row(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    row.entry.snapshot.available_from = Some(CONFIRMED_AT + 1);

    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            &[row],
            Some(&listing),
            CONFIRMED_AT
        ),
        Err(SelectionRejection::NotAvailableAt {
            listing,
            at: CONFIRMED_AT,
        })
    );
}

/// The same listing is selectable once its window covers the moment of confirmation.
#[test]
fn confirmation_should_accept_a_listing_inside_its_window() {
    let binance_spot = route("binance", "spot");
    let listing = listing_key(&binance_spot, "BTCUSDT");
    let mut row = unnamed_row(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    row.entry.snapshot.available_from = Some(CONFIRMED_AT - 1);
    row.entry.snapshot.available_until = Some(CONFIRMED_AT + 1);

    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            &[row],
            Some(&listing),
            CONFIRMED_AT
        ),
        Ok(InstrumentSelection { listing })
    );
}

#[test]
fn confirmation_should_require_a_selection() {
    let rows = Vec::new();

    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
            &rows,
            None,
            CONFIRMED_AT
        ),
        Err(SelectionRejection::NoneSelected)
    );
}

#[test]
fn two_listings_of_one_instrument_should_stay_distinct_and_ordered_by_their_keys() {
    let spot_route = route("binance", "spot");
    let perp_route = route("binance", "usd-m-perp");
    let instrument = spot("BTC", "USDT");
    let spot_entry = entry(
        &spot_route,
        "BTCUSDT",
        instrument.clone(),
        ListingStatus::Trading,
    );
    let perp_entry = entry(&perp_route, "BTCUSDT", instrument, ListingStatus::Trading);
    let catalog = catalog_with(&spot_route, vec![spot_entry.clone()]);
    let filters = filters(InstrumentKindFilter::All, CapabilityId::Candlesticks);
    let display = catalog.cached_display();

    let rows = rows_of(
        &display,
        &spot_route,
        vec![perp_entry.clone(), spot_entry.clone()],
        &filters,
        None,
    );

    assert_eq!(rows.len(), 1, "only the route's own listing is offered");
    assert_eq!(rows[0].entry.listing.key, spot_entry.listing.key);
    assert_ne!(spot_entry.listing.key, perp_entry.listing.key);
}

#[test]
fn rows_without_a_published_name_should_be_ordered_by_their_typed_key() {
    let spot_route = route("binance", "spot");
    let btc = unnamed_row(
        &spot_route,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    let eth = unnamed_row(
        &spot_route,
        "ETHUSDT",
        spot("ETH", "USDT"),
        ListingStatus::Trading,
    );
    let mut rows = vec![eth.clone(), btc.clone()];
    rows.sort_by(compare_rows);

    assert_eq!(
        row_keys(&rows),
        vec![btc.entry.listing.key, eth.entry.listing.key],
        "a row no generation named is ordered by its own typed key"
    );
}

#[test]
fn the_route_segment_should_decide_for_rows_the_display_cannot_tell_apart() {
    let spot_route = route("binance", "spot");
    let first = entry(
        &spot_route,
        "AAA",
        spot("AAA", "USDT"),
        ListingStatus::Trading,
    );
    let second = entry(
        &spot_route,
        "BBB",
        spot("BBB", "USDT"),
        ListingStatus::Trading,
    );
    let a = ordered_under(&first, "binance/spot");
    let b = ordered_under(&second, "binance/usd-m-perp");
    let mut rows = [b, a];
    rows.sort_by(compare_rows);

    assert_eq!(rows[0].route_order, "binance/spot");
    assert_eq!(rows[1].route_order, "binance/usd-m-perp");
}

#[test]
fn the_unknown_listing_label_should_be_defined_in_every_locale() {
    for language in i18n::language::codes::LanguageCode::all() {
        let translate = crate::test_support::translate_in(language);
        assert!(
            !i18n::t_smol!(translate, UNKNOWN_LISTING_KEY).is_empty(),
            "{language} must define the unknown-listing label"
        );
    }
}

#[test]
fn selection_should_survive_while_the_route_still_offers_it() {
    let spot_route = route("binance", "spot");
    let selected = listing_key(&spot_route, "BTCUSDT");
    let rows = vec![unnamed_row(
        &spot_route,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    )];

    assert_eq!(
        selected_listing_if_available(&rows, Some(&selected)),
        Some(&selected)
    );
}

#[test]
fn selection_should_be_cleared_when_a_switch_leaves_it_behind() {
    let spot_route = route("binance", "spot");
    let selected = listing_key(&spot_route, "ETHUSDT");
    let rows = vec![unnamed_row(
        &spot_route,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    )];

    assert_eq!(selected_listing_if_available(&rows, Some(&selected)), None);
}

#[test]
fn selection_should_not_be_invented_for_a_selector_opened_without_one() {
    let spot_route = route("binance", "spot");

    assert_eq!(initial_listing(&None, &spot_route), None);
    assert_eq!(
        confirmed_selection(
            &filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
            &[],
            None,
            CONFIRMED_AT
        ),
        Err(SelectionRejection::NoneSelected)
    );
}

#[test]
fn a_scope_row_should_open_the_route_it_is_named_after() {
    let filters = filters(InstrumentKindFilter::All, CapabilityId::Candlesticks);
    let spot_route = route("binance", "spot");

    assert_eq!(
        route_of_row(&format_smolstr!("{spot_route}"), &filters),
        Some(spot_route)
    );
    assert_eq!(
        route_of_row("binance", &filters),
        None,
        "a venue row names no route: it is opened or folded rather than chosen"
    );
}

#[test]
fn a_venue_without_an_offered_scope_should_not_be_a_row() {
    let filters = filters(InstrumentKindFilter::All, CapabilityId::ForceOrder);
    let rows = venue_rows(&filters, &crate::test_support::translate());

    assert_eq!(
        rows.len(),
        1,
        "only the venue that carries the capability is a row"
    );
    assert_eq!(rows[0].label.as_str(), "Binance");
    assert!(
        !rows.iter().any(|row| row.label.as_str() == "OKX"),
        "a venue with nothing this caller can read is not drawn as a row: {rows:?}"
    );
}

#[test]
fn preparation_should_filter_route_kind_and_status() {
    let binance_spot = route("binance", "spot");
    let btc = entry(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    let eth = entry(
        &binance_spot,
        "ETHUSDT",
        spot("ETH", "USDT"),
        ListingStatus::Halted,
    );
    let catalog = catalog_with(&binance_spot, vec![btc.clone(), eth.clone()]);
    let filters = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);

    let rows = fixture_rows(
        &catalog,
        &binance_spot,
        vec![btc.clone(), eth.clone()],
        &filters,
        None,
    );
    assert_eq!(
        row_keys(&rows),
        vec![btc.listing.key.clone(), eth.listing.key.clone()],
        "kind and route admit both rows: the status filter is `Any`"
    );

    let mut tradable = filters;
    tradable.status_filter = InstrumentStatusFilter::Tradable;
    let rows = fixture_rows(
        &catalog,
        &binance_spot,
        vec![btc.clone(), eth],
        &tradable,
        None,
    );
    assert_eq!(row_keys(&rows), vec![btc.listing.key]);
}

#[test]
fn preparation_should_apply_the_predicate_before_a_selection_is_resolved() {
    let binance_spot = route("binance", "spot");
    let btc = entry(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    let eth = entry(
        &binance_spot,
        "ETHUSDT",
        spot("ETH", "USDT"),
        ListingStatus::Trading,
    );
    let catalog = catalog_with(&binance_spot, vec![btc.clone(), eth.clone()]);
    let filters = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);
    let wanted = btc.listing.key.clone();
    let predicate: EntryPredicate = Rc::new(move |entry| entry.listing.key == wanted);

    let rows = fixture_rows(
        &catalog,
        &binance_spot,
        vec![btc.clone(), eth],
        &filters,
        Some(&predicate),
    );
    assert_eq!(row_keys(&rows), vec![btc.listing.key]);
}

#[test]
fn the_matcher_column_should_carry_the_name_the_native_symbol_and_the_aliases() {
    let binance_spot = route("binance", "spot");
    let published = entry(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    let catalog = catalog_with(&binance_spot, vec![published.clone()]);
    let filters = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);
    let rows = fixture_rows(&catalog, &binance_spot, vec![published], &filters, None);
    let column = rows[0].matcher_column();

    assert!(
        column.contains("BTC/USDT"),
        "the published name is one column: {column}"
    );
    assert!(
        matcher_hits(&column, "btc/usdt"),
        "the name finds the row through one query: {column}"
    );
}

#[test]
fn rows_should_be_ordered_by_the_published_name() {
    let binance_spot = route("binance", "spot");
    let eth = entry(
        &binance_spot,
        "ETHUSDT",
        spot("ETH", "USDT"),
        ListingStatus::Trading,
    );
    let btc = entry(
        &binance_spot,
        "BTCUSDT",
        spot("BTC", "USDT"),
        ListingStatus::Trading,
    );
    let catalog = catalog_with(&binance_spot, vec![eth.clone(), btc.clone()]);
    let filters = filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks);

    let rows = fixture_rows(&catalog, &binance_spot, vec![eth, btc], &filters, None);
    assert_eq!(
        row_names(&rows),
        ["BTC/USDT", "ETH/USDT"],
        "the list is ordered by the published name, not by the order the catalog was read in"
    );
}

/* ── the dialog ───────────────────────────────────────────────────────────── */

/// Opens the selector against `harness` and reports what the entry point answered.
fn open(harness: &mut Harness, filters: InstrumentFilters) -> bool {
    let host = harness.host().clone();
    let window = harness.window();
    let app = harness.app_mut();
    InstrumentSelector::show_with_filters(None, filters, Rc::new(|_, _, _| {}), &host, app, window)
}

fn click_node(harness: &mut Harness, prefix: &str) {
    let node = harness.find(prefix).unwrap_or_else(|| {
        panic!(
            "no node with prefix {prefix}; ids: {:?}",
            harness.debug_ids()
        )
    });
    let bounds = harness.bounds(node).expect("the node is laid out");
    let centre = Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    harness.move_pointer(centre);
    harness.click(centre);
}

/// Settles the dialog through a close: the kit fade needs frames and clock movement.
fn settle_close(harness: &mut Harness) -> bool {
    let closed = harness.settle_until(120, |harness| {
        harness.overlay_count() == 0 && harness.router().is_empty()
    });
    for _ in 0..8 {
        harness.advance(Duration::from_millis(40));
        harness.drain();
    }
    closed || (harness.overlay_count() == 0 && harness.router().is_empty())
}

#[test]
fn a_selector_offers_no_dialog_without_a_route() {
    let mut harness = Harness::new();

    let opened = open(
        &mut harness,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks).with(empty_registry()),
    );
    harness.settle(4);

    assert!(
        !opened,
        "a registry with nothing to offer refuses the dialog"
    );
    assert_eq!(harness.overlay_count(), 0);
    assert!(harness.find(PANEL_ID).is_none());
}

#[test]
fn a_selector_with_a_catalog_opens_and_lists_its_matches() {
    let spot_route = test_support::spot_route();
    let catalog = catalog_with(
        &spot_route,
        vec![test_support::entry(
            &spot_route,
            spot("BTC", "USDT"),
            "BTCUSDT",
        )],
    );
    let mut harness = Harness::with_catalog(catalog);
    let opened = open(
        &mut harness,
        filters(InstrumentKindFilter::Spot, CapabilityId::Candlesticks),
    );
    assert!(opened);

    let listed = harness.settle_until(200, |harness| {
        harness.find("INSTRUMENT_SELECTOR:row:").is_some()
    });
    assert!(
        listed,
        "the published listing reaches the list: {:?}",
        harness.debug_ids()
    );
    let row = harness
        .find("INSTRUMENT_SELECTOR:row:")
        .expect("the row exists");
    assert!(
        harness.text_of(row).contains("BTC/USDT"),
        "the row is drawn by the published name: {:?}",
        harness.text_of(row)
    );
    assert_eq!(harness.overlay_count(), 1, "the dialog is open");
}

#[test]
fn a_route_with_no_loaded_catalog_should_report_that_state() {
    let spot_route = test_support::spot_route();
    let mut harness = Harness::new();
    let opened = open(
        &mut harness,
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::Spot,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Candlesticks,
            entry_predicate: None,
            routes: loaded(vec![scope(spot_route, &[CapabilityId::Candlesticks])]),
        },
    );
    assert!(opened);
    harness.settle(4);

    let panel = harness.find(PANEL_ID).expect("the dialog draws its panel");
    assert!(
        harness.has_text(panel, "This market has not been loaded yet"),
        "the list says the route's catalog is not in memory: {:?}",
        harness.text_of(panel)
    );
}

#[test]
fn an_empty_catalog_should_leave_the_list_empty() {
    let spot_route = test_support::spot_route();
    let catalog = catalog_with(
        &spot_route,
        vec![test_support::entry(
            &spot_route,
            spot("BTC", "USDT"),
            "BTCUSDT",
        )],
    );
    let mut harness = Harness::with_catalog(catalog);
    // The one listing is a spot listing, and the filters ask for options only.
    let opened = open(
        &mut harness,
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::Option,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Candlesticks,
            entry_predicate: None,
            routes: loaded(vec![scope(spot_route, &[CapabilityId::Candlesticks])]),
        },
    );
    assert!(opened);
    harness.settle(4);

    let panel = harness.find(PANEL_ID).expect("the dialog draws its panel");
    assert!(
        harness.has_text(panel, "No trading instruments available"),
        "a catalog with nothing this selector may offer says so: {:?}",
        harness.text_of(panel)
    );
}

#[test]
fn the_column_should_hold_every_venue_with_the_scopes_under_it() {
    let mut harness = Harness::new();
    let opened = open(
        &mut harness,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
    );
    assert!(opened);
    harness.settle(4);

    let panel = harness.find(PANEL_ID).expect("the dialog draws its panel");
    let text = harness.text_of(panel);
    assert!(
        text.contains("Binance"),
        "the venue is a fold of its scopes: {text:?}"
    );
    assert!(
        text.contains("SPOT"),
        "the scope reads in the protocol's own case: {text:?}"
    );
    assert!(
        text.contains("USD-M-PERP"),
        "every offered scope is a row: {text:?}"
    );
    assert!(
        text.contains("OKX"),
        "every offered venue is a row: {text:?}"
    );
}

#[test]
fn a_multi_segment_scope_should_read_as_its_whole_uppercase_scope() {
    let mut harness = Harness::new();
    let opened = open(
        &mut harness,
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Candlesticks,
            entry_predicate: None,
            routes: loaded(vec![scope(
                route("hyperliquid", "perp-dex/xyz"),
                &[CapabilityId::Candlesticks],
            )]),
        },
    );
    assert!(opened);
    harness.settle(4);

    let panel = harness.find(PANEL_ID).expect("the dialog draws its panel");
    let text = harness.text_of(panel);
    assert!(text.contains("Hyperliquid"), "{text:?}");
    assert!(
        text.contains("PERP-DEX/XYZ"),
        "a multi-segment scope keeps its whole path: {text:?}"
    );
}

#[test]
fn switching_language_rebuilds_the_venue_column() {
    let mut harness = Harness::new();
    let opened = open(
        &mut harness,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
    );
    assert!(opened);
    harness.settle(4);
    let panel = harness.find(PANEL_ID).expect("the dialog draws its panel");
    assert!(harness.text_of(panel).contains("Binance"));

    harness.switch_language(i18n::language::codes::LanguageCode::ZhCn);
    harness.settle(4);

    let panel = harness.find(PANEL_ID).expect("the dialog draws its panel");
    assert!(
        harness.text_of(panel).contains("币安"),
        "the venue column follows the language: {:?}",
        harness.text_of(panel)
    );
}

#[test]
fn the_cancel_control_should_close_the_dialog() {
    let mut harness = Harness::new();
    let opened = open(
        &mut harness,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
    );
    assert!(opened);
    harness.settle(4);
    click_node(&mut harness, "instrument-selector:cancel");
    harness.settle(2);

    assert!(
        settle_close(&mut harness),
        "the cancel control closes the dialog and releases its route"
    );
}

#[test]
fn selecting_a_match_then_confirming_reports_the_key() {
    // The pointer path into the dialog is covered by the cancel / filter tests; this test
    // drives the list's own selection (which is where a row click ends) and then the
    // confirmation, so the state machine's wiring is asserted without leaning on hit
    // testing inside a modal overlay.
    let spot_route = test_support::spot_route();
    let entry = test_support::entry(&spot_route, spot("BTC", "USDT"), "BTCUSDT");
    let catalog = catalog_with(&spot_route, vec![entry]);
    let mut harness = Harness::with_catalog(catalog);
    let confirmed: Rc<RefCell<Option<InstrumentSelection>>> = Rc::new(RefCell::new(None));
    let callback = Rc::clone(&confirmed);
    let host = harness.host().clone();
    let window = harness.window();
    let filters = InstrumentFilters {
        kind_filter: InstrumentKindFilter::Spot,
        status_filter: InstrumentStatusFilter::Any,
        required_capability: CapabilityId::Candlesticks,
        entry_predicate: None,
        routes: loaded(vec![scope(
            spot_route.clone(),
            &[CapabilityId::Candlesticks],
        )]),
    };
    let mut state = SelectorState::new(
        &host,
        window,
        spot_route.clone(),
        None,
        filters,
        Rc::new(move |selection, _, _| {
            *callback.borrow_mut() = Some(selection);
        }),
        harness.app_mut(),
    );
    state.build_components(harness.app_mut());

    let mut published = false;
    for _ in 0..400 {
        if state.advance_matching() && !state.matched.is_empty() {
            published = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(published, "the matcher publishes the listing");
    // The frame route is what copies a published ranking into the list; here the test is
    // the route for the one step it needs.
    state.show_matches(harness.app_mut());
    assert_eq!(state.rows.borrow().len(), 1, "the list is given the row");
    assert!(state.list.select(harness.app_mut(), Some(0)));
    state.follow_list_selection(harness.app_mut());
    assert_eq!(
        state.selected_listing.as_ref(),
        Some(&test_support::listing_key(&spot_route, "BTCUSDT")),
        "the list's selection is mirrored into the selector's choice"
    );
    assert_eq!(
        state.selection(),
        Ok(InstrumentSelection {
            listing: test_support::listing_key(&spot_route, "BTCUSDT"),
        }),
        "the selection is reported for the key the row stands for"
    );

    state.confirm(harness.app_mut());
    assert_eq!(
        confirmed.borrow().as_ref(),
        Some(&InstrumentSelection {
            listing: test_support::listing_key(&spot_route, "BTCUSDT"),
        }),
        "the confirmation hands the key to the caller"
    );
}
