use market_participants_model::{
    decimal::UnixMillis,
    error::ModelError,
    instrument::InstrumentKind,
    listing::route::{VenueListingKey, VenueRoute},
    market::selection::OrderQuantity,
    outcome::{OutcomeLegRef, leg_label},
    prediction::{PredictionEventRef, PredictionLeg},
};

pub(crate) use crate::market::timeframe::{Adjustment, StatisticsTimeFrame, TimeFrame};
use crate::{
    account::shared::{MarginMode, OrderId, OrderKind, PositionMode},
    protocol::AuthenticatedTarget,
};

macro_rules! listing_scoped_query {
    ($name:ident) => {
        impl $name {
            pub fn route(&self) -> &VenueRoute {
                &self.listing.route
            }
        }
    };
}

macro_rules! route_scoped_query {
    ($name:ident) => {
        impl $name {
            pub const fn route(&self) -> &VenueRoute {
                &self.route
            }
        }
    };
}

macro_rules! account_scoped_query {
    ($name:ident) => {
        impl $name {
            pub fn route(&self) -> &VenueRoute {
                self.target.route()
            }

            pub const fn target(&self) -> &AuthenticatedTarget {
                &self.target
            }
        }
    };
}

macro_rules! account_target_query {
    ($name:ident) => {
        impl $name {
            pub const fn target(&self) -> &AuthenticatedTarget {
                &self.target
            }
        }
    };
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MarketCatalogQuery {
    pub route: VenueRoute,
}
route_scoped_query!(MarketCatalogQuery);

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CandlestickFetchQuery {
    pub listing: VenueListingKey,
    pub timeframe: TimeFrame,
    pub adjustment: Adjustment,
    pub start_timestamp: Option<UnixMillis>,
    pub end_timestamp: Option<UnixMillis>,
    pub limit: u16,
}
listing_scoped_query!(CandlestickFetchQuery);

impl CandlestickFetchQuery {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_time_range(self.start_timestamp, self.end_timestamp)?;
        validate_positive(self.limit, "limit")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CandlestickSubscriptionQuery {
    pub listing: VenueListingKey,
    pub timeframe: TimeFrame,
    pub adjustment: Adjustment,
}
listing_scoped_query!(CandlestickSubscriptionQuery);

impl CandlestickSubscriptionQuery {
    pub const fn validate(&self) -> Result<(), ModelError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MarketTradeFetchQuery {
    pub listing: VenueListingKey,
    pub start_timestamp: Option<UnixMillis>,
    pub end_timestamp: Option<UnixMillis>,
    pub limit: u16,
}
impl MarketTradeFetchQuery {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_time_range(self.start_timestamp, self.end_timestamp)?;
        validate_positive(self.limit, "limit")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MarketTradeSubscriptionQuery {
    pub listing: VenueListingKey,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TickerQuery {
    pub listing: VenueListingKey,
}
listing_scoped_query!(TickerQuery);

impl TickerQuery {
    pub const fn validate(&self) -> Result<(), ModelError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TickersQuery {
    pub route: VenueRoute,
}
route_scoped_query!(TickersQuery);

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct OrderBookQuery {
    pub listing: VenueListingKey,
    pub depth: u16,
}
impl OrderBookQuery {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_positive(self.depth, "depth")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ForceOrderSubscriptionQuery {
    pub route: VenueRoute,
}

impl ForceOrderSubscriptionQuery {
    pub const fn validate(&self) -> Result<(), ModelError> {
        Ok(())
    }

    pub fn require_perpetual(kind: InstrumentKind) -> Result<(), ModelError> {
        if kind == InstrumentKind::Perpetual {
            return Ok(());
        }
        Err(ModelError::UnsupportedInstrumentKind { kind })
    }
}
route_scoped_query!(ForceOrderSubscriptionQuery);

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PerpetualStatisticsQuery {
    pub listing: VenueListingKey,
    pub timeframe: StatisticsTimeFrame,
    pub start_timestamp: Option<UnixMillis>,
    pub end_timestamp: Option<UnixMillis>,
    pub limit: u16,
}
impl PerpetualStatisticsQuery {
    pub fn validate_for(&self, kind: InstrumentKind) -> Result<(), ModelError> {
        ForceOrderSubscriptionQuery::require_perpetual(kind)?;
        validate_time_range(self.start_timestamp, self.end_timestamp)?;
        validate_positive(self.limit, "limit")
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_time_range(self.start_timestamp, self.end_timestamp)?;
        validate_positive(self.limit, "limit")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TradingSettingsQuery {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub margin_mode: MarginMode,
}

impl TradingSettingsQuery {
    pub fn validate_for(&self, kind: InstrumentKind) -> Result<(), ModelError> {
        ForceOrderSubscriptionQuery::require_perpetual(kind)
    }
}
account_target_query!(TradingSettingsQuery);

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct AuthenticatedQuery {
    pub target: AuthenticatedTarget,
    pub listing: Option<VenueListingKey>,
    pub start_timestamp: Option<UnixMillis>,
    pub end_timestamp: Option<UnixMillis>,
    pub limit: u16,
}

impl AuthenticatedQuery {
    pub fn account_wide(target: AuthenticatedTarget) -> Self {
        Self {
            target,
            listing: None,
            start_timestamp: None,
            end_timestamp: None,
            limit: 100,
        }
    }

    pub fn for_listing(target: AuthenticatedTarget, listing: VenueListingKey) -> Self {
        Self {
            listing: Some(listing),
            ..Self::account_wide(target)
        }
    }

    pub fn with_window(
        mut self,
        start_timestamp: Option<UnixMillis>,
        end_timestamp: Option<UnixMillis>,
        limit: u16,
    ) -> Self {
        self.start_timestamp = start_timestamp;
        self.end_timestamp = end_timestamp;
        self.limit = limit;
        self
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        validate_time_range(self.start_timestamp, self.end_timestamp)?;
        validate_positive(self.limit, "limit")
    }
}
account_scoped_query!(AuthenticatedQuery);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaceOrderCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub side: market_participants_model::market::units::Side,
    pub position_side: Option<market_participants_model::market::units::PositionSide>,
    pub kind: OrderKind,
    pub quantity: OrderQuantity,
    pub reduce_only: bool,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<std::num::NonZeroU8>,
    pub market_max_slippage: Option<market_participants_model::decimal::Ratio>,
}

impl PlaceOrderCommand {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.quantity.value().is_zero() {
            return Err(ModelError::ZeroValue { field: "quantity" });
        }
        match (&self.kind, self.market_max_slippage) {
            (OrderKind::Market, Some(slippage))
                if slippage.is_zero()
                    || slippage >= market_participants_model::decimal::Ratio::from(1_u8) =>
            {
                Err(invalid_field("market_max_slippage", "out_of_range"))
            }
            (OrderKind::Market, _) | (_, None) => Ok(()),
            (_, Some(_)) => Err(invalid_field(
                "market_max_slippage",
                "requires_immediate_market_order",
            )),
        }
    }
}
account_target_query!(PlaceOrderCommand);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CancelOrderCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub order_id: OrderId,
}
account_target_query!(CancelOrderCommand);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetPositionModeCommand {
    pub target: AuthenticatedTarget,
    pub mode: PositionMode,
}
account_target_query!(SetPositionModeCommand);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetMarginModeCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub mode: MarginMode,
}
account_target_query!(SetMarginModeCommand);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetLeverageCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub margin_mode: MarginMode,
    pub position_side: Option<market_participants_model::market::units::PositionSide>,
    pub leverage: std::num::NonZeroU8,
}
account_target_query!(SetLeverageCommand);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredictionEventsQuery {
    pub route: VenueRoute,
    pub category: Option<smol_str::SmolStr>,
    pub search: Option<smol_str::SmolStr>,
    pub cursor: Option<smol_str::SmolStr>,
    pub page_size: u16,
}
route_scoped_query!(PredictionEventsQuery);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredictionEventQuery {
    pub event: market_participants_model::prediction::PredictionEventRef,
}

impl PredictionEventQuery {
    pub const fn route(&self) -> &VenueRoute {
        &self.event.route
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredictionSeriesQuery {
    pub event: market_participants_model::prediction::PredictionEventRef,
    pub legs: Vec<market_participants_model::prediction::PredictionLeg>,
    pub range: market_participants_model::prediction::PredictionRange,
}

impl PredictionSeriesQuery {
    pub const fn route(&self) -> &VenueRoute {
        &self.event.route
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredictionSeriesSubscriptionQuery {
    pub event: market_participants_model::prediction::PredictionEventRef,
    pub legs: Vec<market_participants_model::prediction::PredictionLeg>,
}

impl PredictionSeriesSubscriptionQuery {
    pub const fn route(&self) -> &VenueRoute {
        &self.event.route
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredictionOrderBookQuery {
    pub event: PredictionEventRef,
    pub legs: Vec<PredictionLeg>,
    pub leg: OutcomeLegRef,
}

impl PredictionOrderBookQuery {
    pub const fn route(&self) -> &VenueRoute {
        &self.event.route
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.legs.is_empty() {
            return Err(ModelError::EmptyCollection { field: "legs" });
        }
        if self.legs.iter().any(|leg| leg.leg == self.leg) {
            return Ok(());
        }
        Err(ModelError::InvalidField {
            field: "leg",
            value: leg_label(&self.leg),
        })
    }
}

pub(crate) fn invalid_field(field: &'static str, value: &'static str) -> ModelError {
    ModelError::InvalidField {
        field,
        value: smol_str::SmolStr::new_static(value),
    }
}

fn validate_time_range(
    start_timestamp: Option<UnixMillis>,
    end_timestamp: Option<UnixMillis>,
) -> Result<(), ModelError> {
    if let Some(start_timestamp) = start_timestamp
        && start_timestamp < 0
    {
        return Err(ModelError::InvalidTimestamp { field: "start" });
    }
    if let Some(end_timestamp) = end_timestamp
        && end_timestamp < 0
    {
        return Err(ModelError::InvalidTimestamp { field: "end" });
    }
    if start_timestamp
        .zip(end_timestamp)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(ModelError::InvalidTimeRange);
    }
    Ok(())
}

fn validate_positive(value: u16, field: &'static str) -> Result<(), ModelError> {
    if value == 0 {
        Err(ModelError::ZeroValue { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        error::ModelError,
        identifier::{CtfConditionId, IdentifierValue, VenueId, VenueScopeId},
        listing::{
            continuity::{ListingContinuityKey, ListingContinuityTerms},
            route::VenueRoute,
            venue::{PolymarketAssetId, PolymarketProtocolVersion},
        },
        outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId, leg_label},
        prediction::{PredictionEventRef, PredictionLeg, PredictionNativeLeg},
    };
    use smol_str::format_smolstr;

    use super::PredictionOrderBookQuery;

    fn event() -> PredictionEventRef {
        PredictionEventRef {
            route: VenueRoute::new(
                VenueId::Polymarket,
                VenueScopeId::new("predict").expect("the prediction scope is canonical"),
            ),
            native_event: IdentifierValue::new("1262243").expect("the fixture event id is valid"),
        }
    }

    fn leg_ref(digit: &str) -> OutcomeLegRef {
        let identity = |digits: &str| format!("v1/{}", digits.repeat(64));
        OutcomeLegRef {
            group: OutcomeGroupId::from_identity_value(&identity("1"))
                .expect("the fixture group identity is valid"),
            member: OutcomeMemberId::from_identity_value(&identity(digit))
                .expect("the fixture member identity is valid"),
            side: OutcomeSideId::from_identity_value(&identity("3"))
                .expect("the fixture side identity is valid"),
        }
    }

    fn leg(digit: &str, asset_id: &str) -> PredictionLeg {
        let asset_id: PolymarketAssetId = asset_id.parse().expect("the fixture asset id is valid");
        let listing = ListingContinuityKey {
            route: event().route,
            terms: ListingContinuityTerms::Polymarket {
                condition_id: CtfConditionId::new(format!("0x{}", "a".repeat(64)))
                    .expect("the fixture condition id is valid"),
                asset_id: asset_id.clone(),
                protocol: PolymarketProtocolVersion::V1,
            },
        }
        .listing_key()
        .expect("the fixture leg listing is valid");
        PredictionLeg {
            leg: leg_ref(digit),
            label: format_smolstr!("Leg {digit}"),
            probability: None,
            listing,
            native: PredictionNativeLeg::Polymarket {
                protocol: PolymarketProtocolVersion::V1,
                asset_id,
            },
        }
    }

    #[test]
    fn an_order_book_query_addresses_the_route_of_its_event() {
        let event = event();
        let query = PredictionOrderBookQuery {
            event: event.clone(),
            legs: vec![leg("2", "3116"), leg("4", "4329")],
            leg: leg_ref("2"),
        };

        assert_eq!(query.route(), &event.route);
        assert_eq!(query.validate(), Ok(()));
    }

    #[test]
    fn an_order_book_query_without_legs_is_refused() {
        let query = PredictionOrderBookQuery {
            event: event(),
            legs: Vec::new(),
            leg: leg_ref("2"),
        };

        assert_eq!(
            query.validate(),
            Err(ModelError::EmptyCollection { field: "legs" })
        );
    }

    #[test]
    fn an_order_book_query_whose_leg_is_not_a_market_leg_is_refused() {
        let requested = leg_ref("4");
        let query = PredictionOrderBookQuery {
            event: event(),
            legs: vec![leg("2", "3116")],
            leg: requested.clone(),
        };

        assert_eq!(
            query.validate(),
            Err(ModelError::InvalidField {
                field: "leg",
                value: leg_label(&requested),
            })
        );
    }
}
