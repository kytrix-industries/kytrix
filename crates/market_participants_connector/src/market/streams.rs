//! Listing-scoped market streams.
//!
//! Every stream carries the listing of the event it publishes: the adapter
//! resolved the venue's own identifier to a `VenueListingKey`, and this layer
//! attaches the listing's units to the venue's raw numbers. A stream never
//! guesses a listing from a route, and a route-level stream that publishes a
//! subject it cannot name is a failure, not a silent skip.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use market_participants_model::market::{
    book::{ListingCandlestick, ListingForceOrder, ListingOrderBookUpdate, ListingTrade},
    selection::ListingTicker,
};

use super::{
    domain::SubscriptionStatus,
    error::MarketError,
    units::{ListingUnits, candlestick, order_book_update, ticker, trade},
};
use crate::{
    adapter::{NativeListing, ProtocolEvent, SubscriptionHandle},
    catalog::resolved::EnsuredRoute,
    market_participants::model::ForceOrderSubscriptionQuery,
};

type ProtocolStream =
    Pin<Box<dyn Stream<Item = Result<ProtocolEvent, crate::error::MarketParticipantError>> + Send>>;

fn map_status(
    mut status: tokio::sync::watch::Receiver<crate::adapter::SubscriptionStatus>,
) -> tokio::sync::watch::Receiver<SubscriptionStatus> {
    let (sender, receiver) = tokio::sync::watch::channel(status.borrow().clone().into());
    tokio::spawn(async move {
        while status.changed().await.is_ok() {
            sender.send_replace(status.borrow().clone().into());
        }
    });
    receiver
}

/// Declares the shared lifecycle surface of one stream wrapper.
macro_rules! stream_lifecycle {
    ($name:ident) => {
        impl $name {
            pub fn observe_status(&self) -> tokio::sync::watch::Receiver<SubscriptionStatus> {
                self.status.clone()
            }

            pub fn status(&self) -> SubscriptionStatus {
                self.status.borrow().clone()
            }
        }
    };
}

/// Declares one listing-scoped stream over one native payload.
macro_rules! unit_stream {
    ($name:ident, $item:ty, $variant:ident, $map:path) => {
        pub struct $name {
            inner: ProtocolStream,
            status: tokio::sync::watch::Receiver<SubscriptionStatus>,
            units: ListingUnits,
        }

        impl $name {
            pub(crate) fn from_handle(handle: SubscriptionHandle, units: ListingUnits) -> Self {
                let status = map_status(handle.observe_status());
                Self {
                    inner: handle.into_stream(),
                    status,
                    units,
                }
            }
        }

        stream_lifecycle!($name);

        impl Stream for $name {
            type Item = Result<$item, MarketError>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                loop {
                    match self.inner.as_mut().poll_next(cx) {
                        Poll::Ready(Some(Ok(ProtocolEvent::$variant(value)))) => {
                            let item = $map(&value.listing, &value.data, &self.units)
                                .map_err(MarketError::from);
                            return Poll::Ready(Some(item));
                        }
                        Poll::Ready(Some(Ok(_))) => continue,
                        Poll::Ready(Some(Err(error))) => {
                            return Poll::Ready(Some(Err(error.into())));
                        }
                        Poll::Ready(None) => return Poll::Ready(None),
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    };
}

unit_stream!(
    CandlestickStream,
    ListingCandlestick,
    Candlestick,
    candlestick
);
unit_stream!(TradeStream, ListingTrade, Trade, trade);
unit_stream!(
    OrderBookStream,
    ListingOrderBookUpdate,
    OrderBook,
    order_book_update
);

pub struct TickerStream {
    inner: ProtocolStream,
    status: tokio::sync::watch::Receiver<SubscriptionStatus>,
    units: ListingUnits,
}

impl TickerStream {
    pub(crate) fn from_handle(handle: SubscriptionHandle, units: ListingUnits) -> Self {
        let status = map_status(handle.observe_status());
        Self {
            inner: handle.into_stream(),
            status,
            units,
        }
    }
}

stream_lifecycle!(TickerStream);

impl Stream for TickerStream {
    type Item = Result<ListingTicker, MarketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(ProtocolEvent::Ticker(NativeListing { listing, data })))) => {
                    return Poll::Ready(Some(
                        ticker(&listing, &data, &self.units).map_err(MarketError::from),
                    ));
                }
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error.into()))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub struct ForceOrderStream {
    inner: ProtocolStream,
    status: tokio::sync::watch::Receiver<SubscriptionStatus>,
    ensured: EnsuredRoute,
}

impl ForceOrderStream {
    pub(crate) fn from_handle(handle: SubscriptionHandle, ensured: EnsuredRoute) -> Self {
        let status = map_status(handle.observe_status());
        Self {
            inner: handle.into_stream(),
            status,
            ensured,
        }
    }
}

stream_lifecycle!(ForceOrderStream);

impl Stream for ForceOrderStream {
    type Item = Result<ListingForceOrder, MarketError>;

    /// Publishes one liquidation order of one perpetual listing.
    ///
    /// A route-level stream may report a subject that is not a perpetual; that
    /// is a product rejection, so it is reported as one instead of being dropped,
    /// while a subject the published catalog does not hold is not a listing this
    /// route offers and is left out.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(ProtocolEvent::ForceOrder(NativeListing {
                    listing,
                    data,
                })))) => {
                    let Ok(resolved) = self.ensured.resolve_listing(&listing) else {
                        continue;
                    };
                    if let Err(error) =
                        ForceOrderSubscriptionQuery::require_perpetual(resolved.instrument.kind())
                    {
                        return Poll::Ready(Some(Err(MarketError::from(error))));
                    }
                    let units = ListingUnits::of(&resolved.rules);
                    return Poll::Ready(Some(Ok(ListingForceOrder {
                        listing,
                        price: units.price(data.price),
                        quantity: units.traded(data.quantity).map_err(MarketError::from)?,
                        position_side: data.position_side,
                        timestamp: data.timestamp,
                    })));
                }
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error.into()))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{StreamExt, stream};
    use market_participants_model::{
        asset::{DerivativeUnderlying, UnderlyingRef},
        decimal::{Fraction, Quantity, Ratio},
        identifier::{NativeSymbol, ReferenceSourceId, ReferenceTermsVersion},
        instrument::{Instrument, InstrumentTerms, PerpetualTerms},
        listing::{
            identifiers::{BinanceListingIdentifiers, VenueListingIdentifiers},
            listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
            route::{VenueListingKey, VenueRoute},
            rules::{
                CommonTradingRules, FundingInterval, FundingSchedule, LeverageTier, MarginMode,
                MarginProfile, PerpetualMarginTerms, PerpetualRisk, PerpetualTradingRules,
                PriceRule, ProductTradingRules, QuantityRule, TradingRules, VenueTradingPolicy,
            },
            venue::{BinanceProduct, PriceBasis},
        },
        market::units::{PositionSide, QuantityUnit},
        reference::{
            PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
            ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
        },
        settlement::{ContractSize, ContractUnit, DerivativePnlModel},
    };
    use tokio_util::sync::CancellationToken;

    use super::ForceOrderStream;
    use crate::{
        adapter::{NativeForceOrder, NativeListing, ProtocolEvent, SubscriptionHandle},
        catalog::test_support,
    };

    /// One perpetual fixture entry on the fixture perpetual route.
    ///
    /// A liquidation is only ever published for a perpetual listing, so the
    /// fixture publishes a real perpetual instrument with perpetual rules rather
    /// than a spot one wearing a perpetual key.
    fn perpetual_entry() -> (
        VenueRoute,
        VenueListingKey,
        crate::catalog::batch::CatalogEntry,
    ) {
        let route = test_support::perp_route();
        // The fixture vocabulary is the one the display contract resolves back from
        // an `AssetId`, so the fixture assets carry its issuer.
        let base = test_support::token(test_support::VENUE, "BTC");
        let quote = test_support::token(test_support::VENUE, "USDT");
        let contract_size = ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(1_u8), quote.clone())
                .expect("the fixture contract unit should be valid"),
        )
        .expect("the fixture contract size should be valid");
        let reference = PriceReferenceTerms {
            underlying: UnderlyingRef::Asset(base.clone()),
            quote_asset: quote.clone(),
            basis: PriceBasis::PerAssetUnit,
            observation: ReferenceObservation {
                source: ReferenceSourceId::new("binance")
                    .expect("the fixture reference source should be canonical"),
                kind: ReferenceObservationKind::Mark,
            },
            aggregation: ReferenceAggregation::Direct,
            session: ReferenceSessionConvention::Continuous,
            settlement: ReferenceSettlementConvention::Continuous,
            schema_version: ReferenceTermsVersion::new("v1")
                .expect("the fixture reference version should be canonical"),
        }
        .reference_id()
        .expect("the fixture price reference should derive");
        let instrument = Instrument::new(InstrumentTerms::Perpetual(
            PerpetualTerms::new(
                DerivativeUnderlying::new(UnderlyingRef::Asset(base.clone()), reference),
                quote.clone(),
                quote.clone(),
                contract_size,
                DerivativePnlModel::Linear,
                None,
            )
            .expect("the fixture perpetual terms should be valid"),
        ))
        .expect("the fixture perpetual instrument should be valid");
        let symbol = NativeSymbol::new("BTCUSDT").expect("the fixture symbol should be valid");
        let key = crate::catalog::test_support::continuity_key(
            &route,
            "BTCUSDT",
            BinanceProduct::Perpetual,
        );
        let listing = VenueListing::new(
            key.clone(),
            *instrument.id(),
            Some(symbol),
            Vec::new(),
            VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
                product: BinanceProduct::Perpetual,
                future_series: None,
                option_series: None,
            }),
        )
        .expect("the fixture perpetual listing should be valid");
        let initial = Fraction::new(
            "0.1"
                .parse::<Ratio>()
                .expect("the fixture initial margin should parse"),
        )
        .expect("the fixture initial margin should be a fraction");
        let maintenance = Fraction::new(
            "0.05"
                .parse::<Ratio>()
                .expect("the fixture maintenance margin should parse"),
        )
        .expect("the fixture maintenance margin should be a fraction");
        let rules = TradingRules {
            common: CommonTradingRules {
                price: PriceRule {
                    quote_asset: quote.clone(),
                    basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
                    tick: market_participants_model::decimal::Tick::try_from(Quantity::from(1_u8))
                        .expect("the fixture tick should be valid"),
                    minimum: None,
                    maximum: None,
                },
                quantity: QuantityRule {
                    unit: QuantityUnit::AssetUnits { asset: base },
                    step: Quantity::from(1_u8),
                    minimum: None,
                    maximum: None,
                },
                notional: None,
                venue_policy: std::sync::Arc::new(VenueTradingPolicy::continuous()),
            },
            product: std::sync::Arc::new(ProductTradingRules::Perpetual(PerpetualTradingRules {
                risk: PerpetualRisk::Published(
                    PerpetualMarginTerms::new(
                        MarginProfile::new(MarginMode::Cross, initial, maintenance, quote.clone())
                            .expect("the fixture margin profile should be valid"),
                        vec![
                            LeverageTier::new(Quantity::from(1_000_u64), 10, maintenance)
                                .expect("the fixture leverage tier should be valid"),
                        ],
                    )
                    .expect("the fixture margin terms should be valid"),
                ),
                funding: FundingSchedule::new(FundingInterval::Adaptive, None, None, quote)
                    .expect("the fixture funding schedule should be valid"),
                open_interest_cap: None,
            })),
        };
        let snapshot =
            ListingSnapshot::new(key.clone(), rules, ListingStatus::Trading, None, None, 0)
                .expect("the fixture perpetual snapshot should be valid");
        (
            route,
            key,
            crate::catalog::batch::CatalogEntry {
                instrument,
                listing,
                snapshot,
            },
        )
    }

    /// A route-level stream resolves every event against the published snapshot
    /// and publishes one listing's liquidation with that listing's key.
    #[tokio::test]
    async fn force_orders_are_published_with_the_listing_key_of_their_own_listing() {
        let (_route, listing, entry) = perpetual_entry();
        let fixture = test_support::Fixture::with_source(
            test_support::perp_route(),
            "binance-perp",
            test_support::listing(vec![entry]),
        );
        let ensured = fixture
            .ensure()
            .await
            .expect("the fixture catalog should load");
        let handle = SubscriptionHandle::new(
            Box::pin(stream::iter([Ok(ProtocolEvent::ForceOrder(
                NativeListing::new(
                    listing.clone(),
                    NativeForceOrder {
                        position_side: PositionSide::Long,
                        price: market_participants_model::decimal::Price::from(100_u64),
                        quantity: market_participants_model::decimal::Quantity::from(2_u64),
                        timestamp: 1_700_000_000_000,
                    },
                ),
            ))])),
            CancellationToken::new(),
        );
        let mut orders = ForceOrderStream::from_handle(handle, ensured);

        let order = orders
            .next()
            .await
            .expect("a liquidation should be published")
            .expect("the fixture liquidation carries its listing");
        assert_eq!(order.listing, listing);
        assert!(orders.next().await.is_none());
    }
}
