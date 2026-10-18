//! The catalog's display read entries.
//!
//! Two entries on [`Catalog`] answer a listing's name: a cached
//! one that reads only what is published and never fetches, and a loading one
//! that asserts the route's catalog is published and then answers from one
//! captured generation. Both are driven here against a fixture catalogue, because
//! what they promise is only visible against a whole catalogue: that no request
//! is made from a read, that a route nothing has loaded is refused as such, that
//! one operation reads one publication, that a publication happening meanwhile is
//! never mixed into an answer, and that a listing the publication never named is
//! a miss rather than a mismatch.

use std::sync::Arc;

use market_participants_model::{
    display::grammar::format_instrument,
    instrument::{Instrument, InstrumentTerms},
    listing::route::VenueListingKey,
};

use crate::catalog::{
    batch::CatalogEntry,
    display::types::DisplayError,
    error::{CatalogError, ListingInstrumentError},
    test_support::{
        self, FIXTURE_CODES, Fixture, SPOT_SOURCE, VENUE, entry, listing_key, spot_route,
        spot_token,
    },
};

/// The venue symbols of the two fixture listings, and the instruments they trade.
const BTC_SYMBOL: &str = "BTCUSDT";
const ETH_SYMBOL: &str = "ETHUSDT";

fn btc() -> Instrument {
    spot_token(VENUE, "BTC", "USDT")
}

fn eth() -> Instrument {
    spot_token(VENUE, "ETH", "USDT")
}

fn btc_entry() -> CatalogEntry {
    entry(&spot_route(), btc(), BTC_SYMBOL)
}

fn eth_entry() -> CatalogEntry {
    entry(&spot_route(), eth(), ETH_SYMBOL)
}

fn btc_key() -> VenueListingKey {
    listing_key(&spot_route(), BTC_SYMBOL)
}

fn eth_key() -> VenueListingKey {
    listing_key(&spot_route(), ETH_SYMBOL)
}

/// Publishes the fixture route's catalog through the one write entry a load has.
///
/// Every read in this module is answered from what a load published, so a test
/// that is about a read drives the same load the connection action does and then
/// reads.
async fn publish(fixture: &Fixture) {
    fixture
        .catalog()
        .refresh_for_test(fixture.route())
        .await
        .expect("the fixture spot route publishes its catalog");
}

// ---- the cached entry ----------------------------------------------------

/// a cached read names the listing from the generation it is published in.
#[tokio::test]
async fn a_cached_read_should_name_the_published_listing() {
    let fixture = Fixture::new(vec![btc_entry()]);
    publish(&fixture).await;
    let catalog = fixture.catalog();

    let name = catalog
        .cached_listing_instrument(&btc_key())
        .expect("the published generation names the listing");

    assert_eq!(name.as_str(), "BTC/USDT");
    assert_eq!(
        fixture
            .catalog()
            .generation()
            .display_catalog()
            .listing_instrument(&btc_key()),
        Some(&name),
        "the cached read answers with the name the generation published"
    );
}

/// a listing the generation never named is a miss, and a miss is never
/// forged into a generation mismatch.
#[tokio::test]
async fn a_cached_read_should_answer_a_listing_the_generation_never_named() {
    let fixture = Fixture::new(vec![btc_entry()]);
    publish(&fixture).await;
    let catalog = fixture.catalog();
    let missing = eth_key();

    assert_eq!(
        catalog.cached_listing_instrument(&missing),
        Err(DisplayError::ListingNotFound {
            listing: Box::new(missing.clone())
        }),
        "a listing the published generation never named is missing"
    );
    assert_eq!(
        catalog
            .cached_display()
            .resolver()
            .instrument_details(eth().id()),
        None,
        "an instrument the published generation never named has no details"
    );
}

/// the cached entry reads the published snapshot and triggers no network,
/// however little is published.
///
/// The catalogue carries an application runtime, exactly as the installed
/// connector does, so a read that went behind its caller's back — a detached load,
/// a revalidation — would have somewhere to run and would be caught here.
#[tokio::test]
async fn a_cached_read_should_never_fetch() {
    let (gate, source) = test_support::gated(vec![btc_entry()]);
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    fixture
        .catalog()
        .set_runtime(widget_tokio_runtime::runtime::Runtime::for_test());
    let catalog = fixture.catalog();

    let captured = catalog.cached_display();
    assert_eq!(captured.generation_id(), catalog.generation_id());
    assert!(
        captured
            .generation()
            .route_snapshot(&spot_route())
            .is_none(),
        "nothing has loaded the route yet"
    );
    assert!(
        matches!(
            captured.listing_instrument(&btc_key()),
            Err(DisplayError::ListingNotFound { .. })
        ),
        "a route that was never loaded names no listing"
    );
    // A read that detached a fetch behind its caller's back would still be
    // counted here: the runtime runs every task it has queued first.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        gate.calls(),
        0,
        "a cached read never reaches the route's source"
    );
    assert!(fixture.catalog().cached_listings(&spot_route()).is_none());
}

/// every listing of one operation is read from the capture that operation
/// took, even when the catalogue publishes another generation in between.
#[tokio::test]
async fn every_listing_of_one_capture_should_come_from_one_generation() {
    let fixture = Fixture::new(vec![btc_entry(), eth_entry()]);
    publish(&fixture).await;
    let catalog = Arc::clone(fixture.catalog_arc());
    let captured = catalog.cached_display();

    let first = captured
        .listing_instrument(&btc_key())
        .expect("the capture names the first listing");
    let retired = captured.generation_id();

    fixture.republish_source(
        SPOT_SOURCE,
        test_support::listing(vec![btc_entry(), eth_entry()]),
    );
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("the route republishes");
    assert!(catalog.generation_id() > retired, "the catalogue moved on");

    let second = captured
        .listing_instrument(&eth_key())
        .expect("the capture names the second listing too");
    assert_eq!(first.as_str(), "BTC/USDT");
    assert_eq!(
        second.as_str(),
        "ETH/USDT",
        "the second listing is of the capture, not of the newer publication"
    );
    assert_eq!(captured.generation_id(), retired);
}

/// a publication that replaces what a reader captured never reaches that
/// reader, and a later capture never mixes in the retired one.
#[tokio::test]
async fn a_capture_should_keep_reading_the_publication_it_took() {
    let fixture = Fixture::new(vec![btc_entry()]);
    publish(&fixture).await;
    let catalog = Arc::clone(fixture.catalog_arc());
    let captured = catalog.cached_display();
    let before = captured
        .listing_instrument(&btc_key())
        .expect("the capture names the listing");
    let retired = captured.generation_id();

    fixture.republish_source(SPOT_SOURCE, test_support::listing(vec![eth_entry()]));
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("the route republishes");
    let current = catalog.generation_id();
    assert!(current > retired, "the publication advanced");

    assert_eq!(
        captured
            .listing_instrument(&btc_key())
            .expect("the capture is unchanged"),
        before,
        "a read of the capture never sees the newer publication"
    );
    assert!(
        matches!(
            captured.listing_instrument(&eth_key()),
            Err(DisplayError::ListingNotFound { .. })
        ),
        "the capture never named the listing only the newer publication names"
    );

    let fresh = catalog.cached_display();
    assert_eq!(fresh.generation_id(), current);
    assert!(
        matches!(
            fresh.listing_instrument(&btc_key()),
            Err(DisplayError::ListingNotFound { .. })
        ),
        "the newer publication retired the listing the capture still names"
    );
    assert_eq!(
        fresh
            .listing_instrument(&eth_key())
            .expect("the newer publication names the listing")
            .as_str(),
        "ETH/USDT"
    );
}

/// the cached read captures the published generation instead of copying it.
///
/// The snapshot is the very pointer the publication lock holds and the published
/// generation is handed out by pointer too, so neither read path copies a map.
/// This is what makes naming a list of rows cost one `Arc` rather than a whole
/// generation, and the assertion is an address comparison: a reader that went
/// back to copying would answer from another allocation and fail here.
#[test]
fn a_cached_read_should_capture_the_published_generation_instead_of_copying_it() {
    let fixture = Fixture::new(vec![btc_entry()]);
    let catalog = fixture.catalog();
    let captured = catalog.cached_display();
    let published = catalog.generation();

    assert!(
        std::ptr::eq(captured.generation(), Arc::as_ref(&published)),
        "the cached read captures the published generation itself"
    );
    assert!(
        Arc::ptr_eq(&catalog.generation(), &published),
        "reading the published generation clones its pointer, never its maps"
    );
}

// ---- the loading entry ---------------------------------------------------

/// a route nothing has loaded is refused as "not loaded", and the
/// refusal is not a failed read — no source is ever asked.
#[tokio::test]
async fn the_loading_read_should_refuse_a_route_that_was_never_loaded() {
    let (gate, source) = test_support::gated(vec![btc_entry()]);
    let fixture = Fixture::with_entries(
        spot_route(),
        SPOT_SOURCE,
        source,
        &[btc_entry()],
        FIXTURE_CODES,
    );
    let catalog = Arc::clone(fixture.catalog_arc());

    let error = catalog
        .listing_instrument(&btc_key())
        .expect_err("a route with no published snapshot serves no name");

    assert!(
        matches!(
            error,
            ListingInstrumentError::Catalog(ref boxed)
                if matches!(**boxed, CatalogError::CatalogNotLoaded { .. })
        ),
        "a route nothing loaded is not a route that failed to load, got {error:?}"
    );
    assert_eq!(
        gate.calls(),
        0,
        "the read is answered from memory, so the source is never asked"
    );
    assert!(fixture.catalog().cached_listings(&spot_route()).is_none());
}

/// the loading entry answers from the route's published catalog, and a
/// listing that publication never named stays a miss rather than a mismatch.
#[tokio::test]
async fn the_loading_read_should_report_a_listing_the_published_catalog_omits() {
    let fixture = Fixture::new(vec![btc_entry()]);
    publish(&fixture).await;
    let catalog = fixture.catalog();
    let missing = eth_key();

    assert_eq!(
        catalog.listing_instrument(&missing),
        Err(ListingInstrumentError::Display(
            DisplayError::ListingNotFound {
                listing: Box::new(missing.clone())
            }
        )),
        "a listing the loaded publication never named is missing, not a mismatch"
    );
    assert!(
        catalog.cached_listings(&spot_route()).is_some(),
        "the route the read names is the one a load published"
    );
}

/// one loading read answers its listing and the instrument behind it from
/// one captured generation, and the name it publishes is the rendering of the
/// details that generation kept.
#[tokio::test]
async fn the_loading_read_should_answer_from_one_captured_generation() {
    let fixture = Fixture::new(vec![btc_entry()]);
    publish(&fixture).await;
    let catalog = Arc::clone(fixture.catalog_arc());

    let name = catalog
        .listing_instrument(&btc_key())
        .expect("the published route names the listing");

    let captured = catalog.cached_display();
    let generation = captured.generation();

    assert_eq!(
        generation.display_catalog().listing_instrument(&btc_key()),
        Some(&name),
        "the listing is named by the entry that generation published"
    );
    let details = generation
        .display_catalog()
        .instrument_details(btc().id())
        .expect("the generation keeps the details of the instrument it named");
    assert_eq!(
        format_instrument(details).expect("the published details render"),
        name,
        "the published name is the rendering of the published details"
    );
    let InstrumentTerms::Spot(terms) = btc().terms() else {
        panic!("the fixture instrument is a spot pair");
    };
    assert!(
        generation.assets().terms(terms.base()).is_some(),
        "the asset the name renders is registered in the captured generation"
    );
    assert!(generation.assets().terms(terms.quote()).is_some());
}

/// a route the catalogue can serve nothing for fails in the
/// catalogue's own domain and keeps the variant a caller acts on — "this route's
/// catalog is not loaded" and "this route serves no catalog" are two answers.
#[tokio::test]
async fn the_loading_read_should_report_a_route_it_cannot_read_as_a_catalog_failure() {
    let fixture = Fixture::failing();
    let catalog = fixture.catalog();

    let error = catalog
        .listing_instrument(&btc_key())
        .expect_err("the fixture route has no published catalog to read");

    assert!(
        matches!(
            error,
            ListingInstrumentError::Catalog(ref boxed)
                if matches!(**boxed, CatalogError::CatalogNotLoaded { .. })
        ),
        "a registered route nothing loaded is a catalog failure, got {error:?}"
    );

    let unregistered = listing_key(&test_support::perp_route(), BTC_SYMBOL);
    let error = catalog
        .listing_instrument(&unregistered)
        .expect_err("a route that is not registered serves nothing");

    assert!(
        matches!(
            error,
            ListingInstrumentError::Catalog(ref boxed)
                if matches!(**boxed, CatalogError::RouteNotRegistered { .. })
        ),
        "a route that is not registered is refused as such, got {error:?}"
    );
}

/// a generation an operation carries explicitly is checked against the one
/// it read, and that is the only thing answered as a mismatch.
#[tokio::test]
async fn an_explicitly_named_foreign_generation_should_be_refused() {
    let fixture = Fixture::new(vec![btc_entry()]);
    publish(&fixture).await;
    let catalog = Arc::clone(fixture.catalog_arc());
    let captured = catalog.cached_display();
    let held = captured
        .resolver()
        .listing_instrument_in(captured.generation_id(), &btc_key())
        .expect("the capture names the listing")
        .clone();

    fixture.republish_source(SPOT_SOURCE, test_support::listing(vec![eth_entry()]));
    catalog
        .refresh_for_test(&spot_route())
        .await
        .expect("the route republishes");
    let current = catalog.generation_id();
    assert!(
        current > captured.generation_id(),
        "the publication moved on"
    );

    assert_eq!(
        captured
            .resolver()
            .listing_instrument_in(current, &btc_key()),
        Err(DisplayError::GenerationMismatch {
            expected: current,
            actual: captured.generation_id()
        }),
        "a key the caller claims came from another generation is refused rather than mixed"
    );
    assert_eq!(
        captured
            .listing_instrument(&btc_key())
            .expect("the capture still answers its own publication"),
        held
    );
}
