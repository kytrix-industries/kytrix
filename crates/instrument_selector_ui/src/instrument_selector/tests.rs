use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use market_participants_connector::catalog::test_support;
use market_participants_model::{
    decimal::UnixMillis,
    instrument::Instrument,
    listing::{listing_impl::ListingStatus, route::VenueListingKey},
    market::selection::InstrumentSelection,
    routing::CapabilityId,
};
use smol_str::{SmolStr, format_smolstr};
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
    click_node_id(harness, node);
}

fn click_node_id(harness: &mut Harness, node: ui_core::node::id::NodeId) {
    let bounds = harness.bounds(node).expect("the node is laid out");
    let centre = Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    harness.move_pointer(centre);
    harness.click(centre);
}

fn selector_keys(harness: &Harness) -> Vec<SmolStr> {
    let mut keys: Vec<SmolStr> = Vec::new();
    for id in harness.debug_ids() {
        let Some((key, _)) = id.split_once(':') else {
            continue;
        };
        if key.starts_with("instrument-selector@") && !keys.iter().any(|known| known == key) {
            keys.push(SmolStr::new(key));
        }
    }
    keys.sort_by_key(|key| {
        key.rsplit_once('@')
            .and_then(|(_, index)| index.parse::<u32>().ok())
            .unwrap_or_default()
    });
    keys
}

fn selector_key(harness: &Harness) -> SmolStr {
    selector_keys(harness)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no selector is open; ids: {:?}", harness.debug_ids()))
}

fn click_selector(harness: &mut Harness, suffix: &str) {
    let key = selector_key(harness);
    click_node(harness, &format_smolstr!("{key}{suffix}"));
}

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

fn open_confirming(
    harness: &mut Harness,
    current: Option<InstrumentSelection>,
    filters: InstrumentFilters,
    confirmed: Rc<RefCell<Vec<InstrumentSelection>>>,
) -> bool {
    let host = harness.host().clone();
    let window = harness.window();
    InstrumentSelector::show_with_filters(
        current,
        filters,
        Rc::new(move |selection, _, _| confirmed.borrow_mut().push(selection)),
        &host,
        harness.app_mut(),
        window,
    )
}

fn both_selectors_list(harness: &Harness) -> bool {
    let keys = selector_keys(harness);
    keys.len() == 2
        && keys.iter().all(|key| {
            let rows = format_smolstr!("{key}:list:row:");
            harness
                .debug_ids()
                .iter()
                .any(|id| id.starts_with(rows.as_str()))
        })
}

fn settle_dialog_close(harness: &mut Harness, key: &str) -> bool {
    let confirm = format_smolstr!("{key}:ok");
    let closed = harness.settle_until(120, |harness| harness.find(&confirm).is_none());
    for _ in 0..8 {
        harness.advance(Duration::from_millis(40));
        harness.drain();
    }
    closed && harness.find(&confirm).is_none()
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
fn the_dialog_builds_through_the_services_singletons() {
    let mut harness = Harness::new();
    assert!(Rc::ptr_eq(
        &harness.host().services_rc(),
        &harness.services_rc()
    ));
    assert!(std::ptr::eq(
        harness.host().router(),
        &*harness.services_rc().frame_router
    ));
    assert!(std::ptr::eq(
        harness.host().layer(),
        &*harness.services_rc().layer
    ));

    assert!(open(
        &mut harness,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks)
    ));
    harness.settle(4);

    assert_eq!(harness.overlay_count(), 1, "the dialog is open");
    assert_eq!(
        harness.layer().len(harness.window()),
        1,
        "the dialog's overlay is registered in the composition root's layer stack"
    );
    assert!(
        !harness.router().is_empty(),
        "the dialog's frame route lives in the composition root's router"
    );
    click_selector(&mut harness, ":cancel");
    assert!(settle_close(&mut harness));
    assert!(
        harness.layer().is_empty(harness.window()),
        "closing removes the overlay from the same stack"
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
    click_selector(&mut harness, ":cancel");
    harness.settle(2);

    assert!(
        settle_close(&mut harness),
        "the cancel control closes the dialog and releases its route"
    );
}

#[test]
fn two_panes_confirm_their_own_selector_and_release_only_their_own_handlers() {
    let spot_route = test_support::spot_route();
    let catalog = catalog_with(
        &spot_route,
        vec![
            test_support::entry(&spot_route, spot("BTC", "USDT"), "BTCUSDT"),
            test_support::entry(&spot_route, spot("ETH", "USDT"), "ETHUSDT"),
        ],
    );
    let mut harness = Harness::with_catalog(catalog);
    let pane_filters = || InstrumentFilters {
        kind_filter: InstrumentKindFilter::Spot,
        status_filter: InstrumentStatusFilter::Any,
        required_capability: CapabilityId::Candlesticks,
        entry_predicate: None,
        routes: loaded(vec![scope(
            spot_route.clone(),
            &[CapabilityId::Candlesticks],
        )]),
    };
    let confirmed: [Rc<RefCell<Vec<InstrumentSelection>>>; 2] = Default::default();
    let stats =
        |harness: &Harness| ui_core::test_support::diagnostics::signal_slot_stats(harness.app());
    let baseline = stats(&harness);
    let baseline_nodes = harness.tree_len();

    assert!(open_confirming(
        &mut harness,
        Some(selection(&spot_route, "BTCUSDT")),
        pane_filters(),
        Rc::clone(&confirmed[0]),
    ));
    harness.settle(4);
    assert!(open_confirming(
        &mut harness,
        Some(selection(&spot_route, "ETHUSDT")),
        pane_filters(),
        Rc::clone(&confirmed[1]),
    ));
    assert!(
        harness.settle_until(200, both_selectors_list),
        "both dialogs list the published catalog: {:?}",
        harness.debug_ids()
    );
    let keys = selector_keys(&harness);
    assert_eq!(keys.len(), 2, "two selectors are open: {keys:?}");
    assert_ne!(keys[0], keys[1], "each dialog owns its own kit key prefix");
    assert_eq!(harness.overlay_count(), 2, "both dialogs are open at once");

    let rows = harness.find_all("INSTRUMENT_SELECTOR:row:");
    let row_text: Vec<String> = rows.iter().map(|row| harness.text_of(*row)).collect();
    assert_eq!(
        row_text,
        ["BTC/USDT", "ETH/USDT", "BTC/USDT", "ETH/USDT"],
        "each dialog lists both published listings, the first dialog's rows first"
    );
    click_node_id(&mut harness, rows[3]);
    harness.settle(2);
    click_node(&mut harness, &format_smolstr!("{}:ok", keys[1]));
    harness.settle(2);
    assert_eq!(
        confirmed[1].borrow().as_slice(),
        [selection(&spot_route, "ETHUSDT")],
        "the top dialog confirms into its own pane"
    );
    assert!(
        confirmed[0].borrow().is_empty(),
        "the first pane's callback was not run by the second dialog's confirm"
    );
    assert!(
        settle_dialog_close(&mut harness, &keys[1]),
        "the confirmed dialog closes and releases its route"
    );
    assert_eq!(
        harness.overlay_count(),
        1,
        "the first pane's dialog is still open"
    );

    let rows = harness.find_all("INSTRUMENT_SELECTOR:row:");
    assert_eq!(rows.len(), 2, "only the first dialog lists rows now");
    assert_eq!(harness.text_of(rows[0]), "BTC/USDT");
    click_node_id(&mut harness, rows[0]);
    harness.settle(2);
    click_node(&mut harness, &format_smolstr!("{}:ok", keys[0]));
    harness.settle(2);
    assert_eq!(
        confirmed[0].borrow().as_slice(),
        [selection(&spot_route, "BTCUSDT")],
        "the still-open dialog kept its own confirm handler"
    );
    assert_eq!(
        confirmed[1].borrow().len(),
        1,
        "the closed dialog's pane was not written a second time"
    );
    assert!(
        settle_close(&mut harness),
        "the second dialog closes and releases its route"
    );

    assert_eq!(harness.overlay_count(), 0, "both dialogs are closed");
    assert!(
        harness.router().is_empty(),
        "no frame route outlived the dialogs"
    );
    assert_eq!(
        harness.tree_len(),
        baseline_nodes,
        "both dialogs' nodes are gone"
    );
    let settled = stats(&harness);
    assert_eq!(
        settled.live, baseline.live,
        "no value slot outlived the dialogs (slots is a peak: {settled:?} vs {baseline:?})"
    );
    for key in &keys {
        for suffix in [":ok.click", ":cancel.click"] {
            let action = ui_core::event::Action::Custom(format_smolstr!("{key}{suffix}"));
            assert!(
                !harness.app().has_action_handler(&action),
                "{key}{suffix} is reclaimed when the dialog finishes"
            );
        }
        let list = format_smolstr!("{key}:list");
        assert!(
            !harness
                .app()
                .has_action_handler(&ui_core::event::Action::Custom(format_smolstr!(
                    "{list}.select"
                ))),
            "the list's selection action is reclaimed"
        );
        let venues = format_smolstr!("{key}:venues");
        assert!(
            !harness
                .app()
                .has_action_handler(&ui_core::event::Action::Custom(format_smolstr!(
                    "{venues}.up"
                ))),
            "the venue tree's arrow action is reclaimed"
        );
        let filter = format_smolstr!("{key}:filter");
        assert!(
            !harness
                .app()
                .has_action_handler(&ui_core::event::Action::Custom(format_smolstr!(
                    "base_ui.input.{filter}.click"
                ))),
            "the filter input's click action is reclaimed"
        );
    }
}

#[test]
fn selecting_a_match_then_confirming_reports_the_key() {
    let spot_route = test_support::spot_route();
    let entry = test_support::entry(&spot_route, spot("BTC", "USDT"), "BTCUSDT");
    let catalog = catalog_with(&spot_route, vec![entry]);
    let mut harness = Harness::with_catalog(catalog);
    let confirmed: Rc<RefCell<Vec<InstrumentSelection>>> = Rc::new(RefCell::new(Vec::new()));
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
            callback.borrow_mut().push(selection);
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
        confirmed.borrow().as_slice(),
        [InstrumentSelection {
            listing: test_support::listing_key(&spot_route, "BTCUSDT"),
        }],
        "one confirmation hands the key to the caller exactly once"
    );
    state.confirm(harness.app_mut());
    assert_eq!(
        confirmed.borrow().len(),
        2,
        "a second confirmation is a second report — the callback is not deduplicated, it \
         simply never runs twice for one"
    );
}

#[test]
fn the_dialog_releases_its_signal_scope_when_it_finishes() {
    let mut harness = Harness::new();
    let host = harness.host().clone();
    let window = harness.window();
    let mut state = SelectorState::new(
        &host,
        window,
        route("binance", "spot"),
        None,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks),
        Rc::new(|_, _, _| {}),
        harness.app_mut(),
    );
    let scope = state.scope;

    state.finish(harness.app_mut());

    assert_eq!(
        harness.app_mut().release_scope(scope),
        0,
        "finish already released the dialog's scope, so a second release reclaims nothing"
    );
}

#[test]
fn opening_and_closing_the_dialog_a_hundred_times_reclaims_everything() {
    let mut harness = Harness::new();
    let filters = filters(InstrumentKindFilter::All, CapabilityId::Candlesticks);
    let stats =
        |harness: &Harness| ui_core::test_support::diagnostics::signal_slot_stats(harness.app());

    assert!(open(&mut harness, filters.clone()));
    harness.settle(4);
    click_selector(&mut harness, ":cancel");
    assert!(settle_close(&mut harness), "the first close finishes");
    let settled = stats(&harness);
    let nodes = harness.tree_len();

    for cycle in 0..100 {
        assert!(open(&mut harness, filters.clone()), "cycle {cycle} opens");
        harness.settle(2);
        click_selector(&mut harness, ":cancel");
        assert!(settle_close(&mut harness), "cycle {cycle} closes");
    }

    assert_eq!(
        stats(&harness),
        settled,
        "a hundred open/close cycles allocated no value slot for good"
    );
    assert_eq!(
        harness.tree_len(),
        nodes,
        "a hundred open/close cycles left no node behind"
    );
    assert_eq!(harness.overlay_count(), 0, "the dialog is closed");
    assert!(
        harness.router().is_empty(),
        "no frame route outlived the dialog"
    );
}

#[test]
fn the_filter_takes_its_own_text_and_leaves_every_other_event_queued() {
    let mut harness = Harness::new();
    assert!(open(
        &mut harness,
        filters(InstrumentKindFilter::All, CapabilityId::Candlesticks)
    ));
    harness.settle(4);
    let key = selector_key(&harness);
    click_selector(&mut harness, ":filter.root");
    harness.settle(2);
    assert_eq!(
        harness.app().focused(harness.window()),
        harness.find(&format_smolstr!("{key}:filter.root")),
        "the click focuses the filter input"
    );

    harness.pump(ui_core::event::Event::Input(
        ui_core::event::InputEvent::Text {
            text: SmolStr::new_static("btc"),
            timestamp: std::time::Instant::now(),
        },
    ));
    harness.pump(ui_core::event::Event::Resized {
        size: ui_core::math::Size::new(1280.0, 900.0),
    });
    harness.settle(2);

    let text = harness
        .find(&format_smolstr!("{key}:filter.text"))
        .expect("the input draws its text node");
    assert_eq!(
        harness.node_text(text),
        "btc",
        "the dialog took the text it owns and typed it into the filter"
    );
    let remaining = harness.take_events();
    assert!(
        !remaining
            .iter()
            .any(|(_, event)| matches!(event, ui_core::event::Event::Input(_))),
        "the text event the dialog consumed is not left for the application: {remaining:?}"
    );
    assert!(
        remaining
            .iter()
            .any(|(_, event)| matches!(event, ui_core::event::Event::Resized { .. })),
        "an event the dialog does not own is still readable by the application: {remaining:?}"
    );

    click_selector(&mut harness, ":cancel");
    assert!(settle_close(&mut harness));
    harness.pump(ui_core::event::Event::Input(
        ui_core::event::InputEvent::Text {
            text: SmolStr::new_static("late"),
            timestamp: std::time::Instant::now(),
        },
    ));
    harness.settle(1);
    assert!(
        harness.take_events().iter().any(|(_, event)| matches!(
            event,
            ui_core::event::Event::Input(ui_core::event::InputEvent::Text { .. })
        )),
        "a closed dialog takes nothing: the text stays with the application"
    );
}
