use market_participants_model::decimal::{Price, Quantity, UnixMillis};

use crate::adapter::{NativeCandlestick, longbridge::candles::session_table::MarketSession};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandleSource {
    Quote,
    Trade,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CandlePrint {
    pub timestamp: UnixMillis,
    pub session: MarketSession,
    pub source: CandleSource,
    pub price: Option<Price>,
    pub volume_delta: Option<Quantity>,
    pub cumulative: Option<CumulativeDay>,
    fields: UpdatedFields,
}

impl CandlePrint {
    pub(crate) fn quote(print: QuotePrint) -> Self {
        Self {
            timestamp: print.timestamp,
            session: print.session,
            source: CandleSource::Quote,
            price: Some(print.price),
            volume_delta: print.volume_delta,
            cumulative: print.cumulative,
            fields: UpdatedFields::PriceAndVolume,
        }
    }

    pub(crate) const fn fields(&self) -> UpdatedFields {
        self.fields
    }

    pub(crate) fn trade(print: TradePrint) -> Self {
        Self {
            timestamp: print.timestamp,
            session: print.session,
            source: CandleSource::Trade,
            price: Some(print.price),
            volume_delta: Some(print.volume),
            cumulative: None,
            fields: UpdatedFields::of(print.trade_type, print.session),
        }
    }
}

pub(crate) struct QuotePrint {
    pub timestamp: UnixMillis,
    pub session: MarketSession,
    pub price: Price,
    pub volume_delta: Option<Quantity>,
    pub cumulative: Option<CumulativeDay>,
}

pub(crate) struct TradePrint {
    pub timestamp: UnixMillis,
    pub session: MarketSession,
    pub price: Price,
    pub volume: Quantity,
    pub trade_type: TradeType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CumulativeDay {
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Quantity,
}

impl CumulativeDay {
    pub(crate) fn candle(self, timestamp: UnixMillis) -> NativeCandlestick {
        NativeCandlestick {
            timestamp,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TradeType {
    Unnamed,
    Named(&'static str),
    Unknown,
}

impl TradeType {
    pub(crate) fn of(value: &str) -> Self {
        match value {
            "" => Self::Unnamed,
            "A" => Self::Named("A"),
            "B" => Self::Named("B"),
            "C" => Self::Named("C"),
            "D" => Self::Named("D"),
            "E" => Self::Named("E"),
            "F" => Self::Named("F"),
            "G" => Self::Named("G"),
            "H" => Self::Named("H"),
            "I" => Self::Named("I"),
            "K" => Self::Named("K"),
            "M" => Self::Named("M"),
            "P" => Self::Named("P"),
            "S" => Self::Named("S"),
            "V" => Self::Named("V"),
            "W" => Self::Named("W"),
            "X" => Self::Named("X"),
            "1" => Self::Named("1"),
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdatedFields {
    PriceAndVolume,
    VolumeOnly,
    Nothing,
}

impl UpdatedFields {
    pub(crate) fn of(trade_type: TradeType, session: MarketSession) -> Self {
        match trade_type {
            TradeType::Unnamed => Self::PriceAndVolume,
            TradeType::Named("A" | "B" | "D" | "E" | "F" | "K" | "S" | "X" | "1") => {
                Self::PriceAndVolume
            }
            TradeType::Named("C" | "G" | "H" | "V" | "W") => Self::VolumeOnly,
            TradeType::Named("I")
                if matches!(session, MarketSession::Pre | MarketSession::Post) =>
            {
                Self::PriceAndVolume
            }
            TradeType::Named("I") => Self::VolumeOnly,
            TradeType::Named("M" | "P") | TradeType::Unknown => Self::Nothing,
            TradeType::Named(_) => Self::Nothing,
        }
    }

    pub(crate) const fn updates_price(self) -> bool {
        matches!(self, Self::PriceAndVolume)
    }

    pub(crate) const fn updates_volume(self) -> bool {
        matches!(self, Self::PriceAndVolume | Self::VolumeOnly)
    }
}
