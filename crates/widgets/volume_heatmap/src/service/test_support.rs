//! The fake venue set the service and the pane both answer through.
//!
//! It lives in a module of its own because two test files need it: the loop's
//! tests drive it to produce maps, and the pane's tests drive it to answer a load.
//! Everything in here is test scaffolding and is compiled only for tests.

use std::{
    collections::VecDeque,
    future::pending,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use ahash::AHashMap;
use market_participants_connector::{
    catalog::{registry::asset::AssetTerms, test_support},
    market::error::MarketError,
};
use market_participants_model::{
    decimal::{Price, Quantity},
    identifier::VenueId,
    listing::route::VenueRoute,
    market::{
        price::MarketPrice,
        selection::ListingTicker,
        units::{MarketQuantity, QuantityUnit},
    },
};
use smol_str::SmolStr;

use super::stream::VolumeHeatMapSource;
use crate::model::{RouteListing, RouteTickerAnswer};

/// A fixture spot route of one venue.
pub(crate) fn route(venue: &str, scope: &str) -> VenueRoute {
    test_support::route(venue, scope)
}

/// The first spot scope of one fixture venue.
pub(crate) fn spot(venue: &str) -> VenueRoute {
    match venue {
        "hyperliquid" => route(venue, "core-spot"),
        _ => route(venue, "spot"),
    }
}

/// The perpetual scope of one fixture venue.
pub(crate) fn perp(venue: &str) -> VenueRoute {
    match venue {
        "hyperliquid" => route(venue, "core-perp"),
        "okx" => route(venue, "swap"),
        _ => route(venue, "usd-m-perp"),
    }
}

/// Binance, the venue most fixtures read.
pub(crate) fn binance() -> VenueId {
    VenueId::Binance
}

/// OKX, the venue a fixture reads when what is being tested is that two venues are
/// two maps.
pub(crate) fn okx() -> VenueId {
    VenueId::Okx
}

/// The scopes one round reads: every scope of one venue, which is every market a
/// map of that venue is drawn from.
pub(crate) fn routes() -> Arc<[VenueRoute]> {
    Arc::from([spot("binance"), perp("binance")])
}

/// One fixture reading of a spot pair, before it is given its listing key.
#[derive(Clone, Debug)]
pub(crate) struct Reading {
    instrument: market_participants_model::instrument::Instrument,
    base_code: SmolStr,
    quote_code: SmolStr,
    notional: u64,
    monotonic_id: i64,
}

/// One reading whose money is the quote asset's notional.
pub(crate) fn ticker(base: &str, quote: &str, notional: u64) -> Reading {
    Reading {
        instrument: test_support::spot_token("fixture", base, quote),
        base_code: SmolStr::new(base),
        quote_code: SmolStr::new(quote),
        notional,
        monotonic_id: i64::try_from(notional).unwrap_or(i64::MAX),
    }
}

/// A pool of identical readings for one route, which is what the bound tests need.
pub(crate) fn repeated(base: &str, quote: &str, notional: u64, count: usize) -> Vec<Reading> {
    vec![ticker(base, quote, notional); count]
}

/// One route's answer, with the listing keys, labels and terms its catalog would
/// carry.
fn answer(route: &VenueRoute, readings: Vec<Reading>) -> RouteTickerAnswer {
    let mut listings = Vec::with_capacity(readings.len());
    let mut asset_labels = AHashMap::with_capacity(readings.len() * 2);
    let mut asset_terms = AHashMap::with_capacity(readings.len() * 2);
    let mut tickers = Vec::with_capacity(readings.len());
    for reading in readings {
        let symbol = format!("{}{}", reading.base_code, reading.quote_code);
        let listing = test_support::listing_key(route, &symbol);
        asset_labels.insert(
            test_support::token("fixture", reading.base_code.as_str()),
            reading.base_code.clone(),
        );
        asset_labels.insert(
            test_support::token("fixture", reading.quote_code.as_str()),
            reading.quote_code.clone(),
        );
        asset_terms.insert(
            test_support::token("fixture", reading.base_code.as_str()),
            AssetTerms::CryptoToken(test_support::token_terms(
                "fixture",
                reading.base_code.as_str(),
            )),
        );
        asset_terms.insert(
            test_support::token("fixture", reading.quote_code.as_str()),
            AssetTerms::CryptoToken(test_support::token_terms(
                "fixture",
                reading.quote_code.as_str(),
            )),
        );
        tickers.push(ListingTicker {
            listing: listing.clone(),
            monotonic_id: Some(reading.monotonic_id),
            open: price_of("10", reading.quote_code.as_str()),
            high: Some(price_of("12", reading.quote_code.as_str())),
            low: Some(price_of("9", reading.quote_code.as_str())),
            close: price_of("11", reading.quote_code.as_str()),
            traded_quantity: None,
            traded_notional: Some(
                MarketQuantity::amount(
                    Quantity::from(reading.notional),
                    QuantityUnit::Notional {
                        asset: test_support::token("fixture", reading.quote_code.as_str()),
                    },
                )
                .expect("a fixture notional is money"),
            ),
        });
        listings.push(RouteListing {
            listing,
            instrument: reading.instrument,
        });
    }
    RouteTickerAnswer {
        listings,
        asset_labels,
        asset_terms,
        tickers,
    }
}

fn price_of(value: &str, quote: &str) -> MarketPrice {
    MarketPrice {
        value: Price::from_str(value).expect("the fixture price should parse"),
        quote_asset: test_support::token("fixture", quote),
        basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
    }
}

/// How the fake answers one route fetch.
///
/// A plan is consumed per fetch, and a source that runs out of plans keeps
/// answering successfully but emptily: a test that watches how many fetches
/// happened has to be able to tell a fetch that failed from one that never ran,
/// and a fake that fails once its queue is empty cannot.
#[derive(Clone, Debug)]
pub(crate) enum FetchPlan {
    Tickers(Vec<Reading>),
    Failure,
    /// Never answers, so the round times out.
    Pending,
}

#[derive(Clone)]
pub(crate) struct FakeSource {
    state: Arc<FakeSourceState>,
}

pub(crate) struct FakeSourceState {
    plans: Mutex<VecDeque<FetchPlan>>,
    /// Whether a fetch the queue has no plan for stays in flight instead of answering emptily.
    hangs_when_out_of_plans: bool,
    routes: Mutex<Vec<VenueRoute>>,
    starts: Mutex<Vec<Instant>>,
    in_flight: AtomicUsize,
    maximum_in_flight: AtomicUsize,
    cancelled_fetches: AtomicUsize,
}

impl FakeSource {
    pub(crate) fn new(plans: impl IntoIterator<Item = FetchPlan>) -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(plans.into_iter().collect()),
                hangs_when_out_of_plans: false,
                routes: Mutex::new(Vec::new()),
                starts: Mutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                maximum_in_flight: AtomicUsize::new(0),
                cancelled_fetches: AtomicUsize::new(0),
            }),
        }
    }

    /// A source whose every fetch stays in flight, however many markets a round asks for.
    ///
    /// The plans of [`FakeSource::new`] are consumed per fetch, so a fixture that wants a whole
    /// round to hang has to know how many markets that round reads; one plan short and the rest
    /// of the round is served with the empty answers an exhausted queue gives. This says it once
    /// instead, so a fixture that grows more markets cannot half-serve its round by accident.
    pub(crate) fn pending() -> Self {
        Self {
            state: Arc::new(FakeSourceState {
                plans: Mutex::new(VecDeque::new()),
                hangs_when_out_of_plans: true,
                routes: Mutex::new(Vec::new()),
                starts: Mutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                maximum_in_flight: AtomicUsize::new(0),
                cancelled_fetches: AtomicUsize::new(0),
            }),
        }
    }

    /// The routes asked, in the order they were asked.
    pub(crate) fn routes(&self) -> Vec<VenueRoute> {
        self.lock_routes().clone()
    }

    /// The routes asked that belong to `venue`, in the order they were asked.
    ///
    /// This is what says which venues the service read: a map is one venue's, so a
    /// fixture asserts that the venue it named was read and that no other one was.
    pub(crate) fn routes_of(&self, venue: VenueId) -> Vec<VenueRoute> {
        self.lock_routes()
            .iter()
            .filter(|route| route.venue == venue)
            .cloned()
            .collect()
    }

    /// How many route fetches ran, which is one per scope of the venue being read.
    pub(crate) fn fetch_count(&self) -> usize {
        self.lock_routes().len()
    }

    /// The instant each fetch started, in call order.
    pub(crate) fn starts(&self) -> Vec<Instant> {
        self.state
            .starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many fetches asked the venues at once, at the highest point.
    pub(crate) fn maximum_in_flight(&self) -> usize {
        self.state.maximum_in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn cancelled_fetches(&self) -> usize {
        self.state.cancelled_fetches.load(Ordering::Acquire)
    }

    fn lock_routes(&self) -> std::sync::MutexGuard<'_, Vec<VenueRoute>> {
        self.state
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn fetch(&self, route: VenueRoute) -> Result<RouteTickerAnswer, MarketError> {
        self.lock_routes().push(route.clone());
        self.state
            .starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Instant::now());
        let in_flight = self.state.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.state
            .maximum_in_flight
            .fetch_max(in_flight, Ordering::AcqRel);
        let mut guard = FetchGuard {
            state: self.state.clone(),
            completed: false,
        };
        // One yield before the answer, so that a round which asks several routes at
        // once can be told from one that asks them in turn: without it every fetch
        // would run to completion inside the first poll and the highest number in
        // flight would always be one.
        tokio::task::yield_now().await;
        let plan = self
            .state
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(if self.state.hangs_when_out_of_plans {
                FetchPlan::Pending
            } else {
                FetchPlan::Tickers(Vec::new())
            });

        let result = match plan {
            FetchPlan::Tickers(readings) => Ok(answer(&route, readings)),
            FetchPlan::Failure => Err(MarketError::ServiceClosed),
            FetchPlan::Pending => pending::<Result<RouteTickerAnswer, MarketError>>().await,
        };
        guard.completed = true;
        result
    }
}

impl VolumeHeatMapSource for FakeSource {
    fn fetch_route_tickers(
        &self,
        route: VenueRoute,
    ) -> impl std::future::Future<Output = Result<RouteTickerAnswer, MarketError>> + Send {
        self.fetch(route)
    }
}

/// Counts a fetch that was dropped rather than answered, which is how a cancelled
/// round is told from one that never started.
struct FetchGuard {
    state: Arc<FakeSourceState>,
    completed: bool,
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        if !self.completed {
            self.state.cancelled_fetches.fetch_add(1, Ordering::AcqRel);
        }
    }
}
