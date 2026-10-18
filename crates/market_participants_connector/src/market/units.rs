//! Attaching one listing's units to the venue's raw numbers.
//!
//! A venue publishes numbers; the listing it published them for says what they
//! mean. The quote asset and the price basis come from the listing's price rule,
//! and the quantity unit from its quantity rule, so no field name — `base`,
//! `quote` or `fee_token` — is ever read as a unit.

use market_participants_model::{
    decimal::{Price, Quantity},
    error::ModelError,
    listing::rules::TradingRules,
    market::{
        book::{
            ListingCandlestick, ListingOrderBook, ListingOrderBookLevel, ListingOrderBookUpdate,
            ListingTakerVolume, ListingTrade,
        },
        price::MarketPrice,
        selection::ListingTicker,
        units::{MarketQuantity, QuantityUnit},
    },
};

use crate::{
    adapter::{
        NativeCandlestick, NativeOrderBook, NativeOrderBookUpdate, NativeTakerVolume, NativeTicker,
        NativeTrade,
    },
    error::MarketParticipantError,
    market_participants::model::NativeOrderBookLevel,
};

/// Everything one resolved listing says about the units of its own market data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListingUnits {
    quote_asset: market_participants_model::asset::AssetId,
    basis: market_participants_model::listing::venue::PriceBasis,
    quantity: QuantityUnit,
    notional_asset: Option<market_participants_model::asset::AssetId>,
}

impl ListingUnits {
    /// Reads the units of one rule set.
    pub(crate) fn of(rules: &TradingRules) -> Self {
        Self {
            quote_asset: rules.common.price.quote_asset.clone(),
            basis: rules.common.price.basis.clone(),
            quantity: rules.common.quantity.unit.clone(),
            notional_asset: rules
                .common
                .notional
                .as_ref()
                .map(|rule| rule.asset.clone()),
        }
    }

    /// Wraps one raw price in this listing's terms.
    pub(crate) fn price(&self, value: Price) -> MarketPrice {
        MarketPrice {
            value,
            quote_asset: self.quote_asset.clone(),
            basis: self.basis.clone(),
        }
    }

    /// Wraps one traded quantity in this listing's unit.
    pub(crate) fn traded(&self, value: Quantity) -> Result<MarketQuantity, ModelError> {
        MarketQuantity::traded(value, self.quantity.clone())
    }

    /// Wraps one monetary amount in this listing's quote asset.
    pub(crate) fn amount(&self, value: Quantity) -> Result<MarketQuantity, ModelError> {
        MarketQuantity::amount(
            value,
            QuantityUnit::Notional {
                asset: self.quote_asset.clone(),
            },
        )
    }

    /// Wraps one optional monetary amount.
    pub(crate) fn optional_amount(
        &self,
        value: Option<Quantity>,
    ) -> Result<Option<MarketQuantity>, ModelError> {
        value.map(|value| self.amount(value)).transpose()
    }

    /// Wraps one optional traded quantity.
    pub(crate) fn optional_traded(
        &self,
        value: Option<Quantity>,
    ) -> Result<Option<MarketQuantity>, ModelError> {
        value.map(|value| self.traded(value)).transpose()
    }

    /// Wraps one native order-book level.
    pub(crate) fn level(
        &self,
        level: &NativeOrderBookLevel,
    ) -> Result<ListingOrderBookLevel, ModelError> {
        Ok(ListingOrderBookLevel {
            price: self.price(level.price),
            quantity: self.traded(level.quantity)?,
        })
    }

    /// Wraps every level of one native side.
    pub(crate) fn levels(
        &self,
        levels: &[NativeOrderBookLevel],
    ) -> Result<Vec<ListingOrderBookLevel>, ModelError> {
        levels.iter().map(|level| self.level(level)).collect()
    }
}

/// Maps one native candlestick of one listing.
pub(crate) fn candlestick(
    listing: &market_participants_model::listing::route::VenueListingKey,
    native: &NativeCandlestick,
    units: &ListingUnits,
) -> Result<ListingCandlestick, MarketParticipantError> {
    let value = ListingCandlestick {
        listing: listing.clone(),
        open: units.price(native.open),
        high: units.price(native.high),
        low: units.price(native.low),
        close: units.price(native.close),
        traded_quantity: Some(
            units
                .traded(native.volume)
                .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        ),
        traded_notional: None,
        timestamp: native.timestamp,
    };
    value
        .validate()
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
    Ok(value)
}

/// Maps one native ticker of one listing.
pub(crate) fn ticker(
    listing: &market_participants_model::listing::route::VenueListingKey,
    native: &NativeTicker,
    units: &ListingUnits,
) -> Result<ListingTicker, MarketParticipantError> {
    let value = ListingTicker {
        listing: listing.clone(),
        monotonic_id: native.monotonic_id,
        open: units.price(native.open),
        high: native.high.map(|price| units.price(price)),
        low: native.low.map(|price| units.price(price)),
        close: units.price(native.close),
        traded_quantity: units
            .optional_traded(native.traded_quantity)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        traded_notional: units
            .optional_amount(native.traded_notional)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
    };
    value
        .validate()
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
    Ok(value)
}

/// Maps one native trade of one listing.
pub(crate) fn trade(
    listing: &market_participants_model::listing::route::VenueListingKey,
    native: &NativeTrade,
    units: &ListingUnits,
) -> Result<ListingTrade, MarketParticipantError> {
    let value = ListingTrade {
        listing: listing.clone(),
        id: native.id.clone(),
        price: units.price(native.price),
        quantity: units
            .traded(native.quantity)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        side: native.side,
        timestamp: native.timestamp,
    };
    value
        .validate()
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
    Ok(value)
}

/// Maps one native order book snapshot of one listing.
pub(crate) fn order_book(
    listing: &market_participants_model::listing::route::VenueListingKey,
    native: &NativeOrderBook,
    units: &ListingUnits,
) -> Result<ListingOrderBook, MarketParticipantError> {
    let value = ListingOrderBook {
        listing: listing.clone(),
        sequence: native.sequence.clone(),
        timestamp: native.timestamp,
        bids: units
            .levels(&native.bids)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        asks: units
            .levels(&native.asks)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
    };
    value
        .validate()
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
    Ok(value)
}

/// Maps one native order book update of one listing.
pub(crate) fn order_book_update(
    listing: &market_participants_model::listing::route::VenueListingKey,
    native: &NativeOrderBookUpdate,
    units: &ListingUnits,
) -> Result<ListingOrderBookUpdate, MarketParticipantError> {
    let value = ListingOrderBookUpdate {
        listing: listing.clone(),
        snapshot: native.snapshot,
        first_sequence: native.first_sequence.clone(),
        last_sequence: native.last_sequence.clone(),
        previous_sequence: native.previous_sequence.clone(),
        timestamp: native.timestamp,
        bids: units
            .levels(&native.bids)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        asks: units
            .levels(&native.asks)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
    };
    value
        .validate()
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
    Ok(value)
}

/// Maps one native taker-volume row of one listing.
pub(crate) fn taker_volume(
    listing: &market_participants_model::listing::route::VenueListingKey,
    native: &NativeTakerVolume,
    units: &ListingUnits,
) -> Result<ListingTakerVolume, MarketParticipantError> {
    Ok(ListingTakerVolume {
        listing: listing.clone(),
        buy: units
            .traded(native.buy)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        sell: units
            .traded(native.sell)
            .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?,
        timestamp: native.timestamp,
    })
}
