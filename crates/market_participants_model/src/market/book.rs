use serde::{Deserialize, Serialize};

use crate::{
    decimal::{Ratio, UnixMillis},
    error::ModelError,
    identifier::IdentifierValue,
    listing::route::VenueListingKey,
    market::{
        price::MarketPrice,
        units::{MarketQuantity, MarketTradeId, PositionSide, Side},
    },
    reference::PriceReferenceId,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingMarketData<T> {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "data")]
    pub data: T,
}

impl<T> ListingMarketData<T> {
    pub fn new(listing: VenueListingKey, data: T) -> Self {
        Self { listing, data }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingTrade {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "id")]
    pub id: MarketTradeId,
    #[serde(rename = "price")]
    pub price: MarketPrice,
    #[serde(rename = "quantity")]
    pub quantity: MarketQuantity,
    #[serde(rename = "side")]
    pub side: Side,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

impl ListingTrade {
    pub fn validate(&self) -> Result<(), ModelError> {
        self.quantity.validate_traded("trade.quantity")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingCandlestick {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "open")]
    pub open: MarketPrice,
    #[serde(rename = "high")]
    pub high: MarketPrice,
    #[serde(rename = "low")]
    pub low: MarketPrice,
    #[serde(rename = "close")]
    pub close: MarketPrice,
    #[serde(rename = "traded_quantity")]
    pub traded_quantity: Option<MarketQuantity>,
    #[serde(rename = "traded_notional")]
    pub traded_notional: Option<MarketQuantity>,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

impl ListingCandlestick {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.low.value > self.high.value {
            return Err(ModelError::InvalidCandlestickPrices);
        }
        if let Some(quantity) = &self.traded_quantity {
            quantity.validate_traded("candlestick.traded_quantity")?;
        }
        if let Some(notional) = &self.traded_notional {
            notional.validate_amount("candlestick.traded_notional")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingOrderBookLevel {
    #[serde(rename = "price")]
    pub price: MarketPrice,
    #[serde(rename = "quantity")]
    pub quantity: MarketQuantity,
}

impl ListingOrderBookLevel {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.price.value.is_zero() {
            return Err(ModelError::InvalidField {
                field: "order_book.price",
                value: "zero".into(),
            });
        }
        self.quantity.validate_traded("order_book.quantity")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "sequence")]
pub enum MarketSequence {
    #[serde(rename = "numeric")]
    Numeric(u64),
    #[serde(rename = "native")]
    Native(IdentifierValue),
}

impl MarketSequence {
    pub fn is_after(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (Self::Numeric(value), Self::Numeric(previous)) => Some(value > previous),
            (Self::Native(value), Self::Native(previous)) => Some(value > previous),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingForceOrder {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "price")]
    pub price: MarketPrice,
    #[serde(rename = "quantity")]
    pub quantity: MarketQuantity,
    #[serde(rename = "position_side")]
    pub position_side: PositionSide,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReferencePriceRole {
    #[serde(rename = "mark")]
    Mark,
    #[serde(rename = "index")]
    Index,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingReferencePrice {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "reference")]
    pub reference: PriceReferenceId,
    #[serde(rename = "role")]
    pub role: Option<ReferencePriceRole>,
    #[serde(rename = "price")]
    pub price: MarketPrice,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingOrderBook {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "sequence")]
    pub sequence: MarketSequence,
    #[serde(rename = "timestamp")]
    pub timestamp: Option<UnixMillis>,
    #[serde(rename = "bids")]
    pub bids: Vec<ListingOrderBookLevel>,
    #[serde(rename = "asks")]
    pub asks: Vec<ListingOrderBookLevel>,
}

impl ListingOrderBook {
    pub fn validate(&self) -> Result<(), ModelError> {
        for level in self.bids.iter().chain(self.asks.iter()) {
            level.validate()?;
            if level.quantity.value.is_zero() {
                return Err(ModelError::EmptyOrderBookLevel);
            }
        }
        if self
            .bids
            .windows(2)
            .any(|pair| pair[0].price.value < pair[1].price.value)
        {
            return Err(ModelError::UnsortedOrderBook { side: "bids" });
        }
        if self
            .asks
            .windows(2)
            .any(|pair| pair[0].price.value > pair[1].price.value)
        {
            return Err(ModelError::UnsortedOrderBook { side: "asks" });
        }
        if let (Some(bid), Some(ask)) = (self.bids.first(), self.asks.first())
            && bid.price.value >= ask.price.value
        {
            return Err(ModelError::CrossedOrderBook);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingOrderBookUpdate {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "snapshot")]
    pub snapshot: bool,
    #[serde(rename = "first_sequence")]
    pub first_sequence: MarketSequence,
    #[serde(rename = "last_sequence")]
    pub last_sequence: MarketSequence,
    #[serde(rename = "previous_sequence")]
    pub previous_sequence: Option<MarketSequence>,
    #[serde(rename = "timestamp")]
    pub timestamp: Option<UnixMillis>,
    #[serde(rename = "bids")]
    pub bids: Vec<ListingOrderBookLevel>,
    #[serde(rename = "asks")]
    pub asks: Vec<ListingOrderBookLevel>,
}

impl ListingOrderBookUpdate {
    pub fn validate(&self) -> Result<(), ModelError> {
        let advances = |after: &MarketSequence, before: &MarketSequence| {
            after == before || after.is_after(before) == Some(true)
        };
        if let Some(previous) = &self.previous_sequence
            && !advances(&self.first_sequence, previous)
        {
            return Err(ModelError::UnsortedCollection {
                field: "order_book_update.first_sequence",
            });
        }
        if !advances(&self.last_sequence, &self.first_sequence) {
            return Err(ModelError::UnsortedCollection {
                field: "order_book_update.last_sequence",
            });
        }
        for level in self.bids.iter().chain(self.asks.iter()) {
            level.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingOpenInterest {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "quantity")]
    pub quantity: MarketQuantity,
    #[serde(rename = "notional")]
    pub notional: Option<MarketQuantity>,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

impl ListingOpenInterest {
    pub fn validate(&self) -> Result<(), ModelError> {
        self.quantity.validate_traded("open_interest.quantity")?;
        if let Some(notional) = &self.notional {
            notional.validate_amount("open_interest.notional")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingLongShortRatio {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "ratio")]
    pub ratio: Ratio,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingTakerVolume {
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "buy")]
    pub buy: MarketQuantity,
    #[serde(rename = "sell")]
    pub sell: MarketQuantity,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}
