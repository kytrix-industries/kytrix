//! The venue's own vocabulary of a push, and what one print carries.
//!
//! The wire sends two messages that both move a candle, and the difference between them is
//! which of a print's values carry anything: a quote states the session's cumulative
//! counters and the increment its own counters advanced by, while a trade states one
//! print's price, volume and type. Reading them is the wire's work; what they *mean* for a
//! candle is here, because the four values that decide it — [`TradeType`], the
//! [`UpdatedFields`] table it is read against, the session and the source — are the
//! venue's protocol rather than one caller's. The session is one of the four because the
//! table's `I` row asks about it as well as about the type (§4.6).

use market_participants_model::decimal::{Price, Quantity, UnixMillis};

use crate::adapter::{NativeCandlestick, longbridge::candles::session_table::MarketSession};
/// Where one print came from, which decides how its fields are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandleSource {
    /// A quote push: its price is the last done, and its volume is what the venue's own
    /// cumulative counters advanced by.
    Quote,
    /// A trade push: one print, whose price, volume and type are its own.
    Trade,
}

/// One push, read as the inputs a candle is built from.
///
/// The venue sends two different messages that both move a candle, and the difference
/// between them is in which of this value's fields carry anything: a quote carries the
/// session's cumulative open, high and low — which is what its own daily candle is
/// written from — and the increment its counters advanced by, while a trade carries one
/// print's price, volume and type. `price` is therefore present on both and the
/// cumulative fields only on the first.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CandlePrint {
    /// The timestamp the venue stamped the push with, in milliseconds.
    pub timestamp: UnixMillis,
    /// The session the venue says the push happened in.
    ///
    /// The session is read here for the two push kinds differently: a trade's type table
    /// asks about it (§4.6's `I` row), while a quote's merge is a fact about the print's
    /// own fields and the record of which session it came from belongs to the frame.
    pub session: MarketSession,
    /// Which of the venue's two pushes this is.
    pub source: CandleSource,
    /// The price the print moves the candle to, when it states one.
    pub price: Option<Price>,
    /// The volume this push adds, when it states one.
    pub volume_delta: Option<Quantity>,
    /// The session's cumulative open, high, low, close and volume, which only a quote
    /// states and only a daily candle is written from.
    pub cumulative: Option<CumulativeDay>,
    /// The fields this print is allowed to update (§4.6).
    ///
    /// They are a fact about the push rather than about the candle, and they are read
    /// where the push is: a **quote** is merged with everything it carries — the venue's
    /// own merger passes `UpdateFields::all()` for it — while a **trade** is read against
    /// the trade-type table and its own session, which is where a print that only moves
    /// the volume comes from.
    fields: UpdatedFields,
}

impl CandlePrint {
    /// Reads one quote push as the print it is.
    ///
    /// A quote is merged with everything it carries: the venue's own merger is the one
    /// that passes every field, and the trade-type table is a fact about trades.
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

    /// Returns the fields this print may update.
    pub(crate) const fn fields(&self) -> UpdatedFields {
        self.fields
    }

    /// Reads one trade as the print it is, against the venue's own type table.
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

/// The values one quote push carries, as the caller read them off the wire.
pub(crate) struct QuotePrint {
    pub timestamp: UnixMillis,
    pub session: MarketSession,
    pub price: Price,
    pub volume_delta: Option<Quantity>,
    pub cumulative: Option<CumulativeDay>,
}

/// The values one trade of a push carries, as the caller read them off the wire.
pub(crate) struct TradePrint {
    pub timestamp: UnixMillis,
    pub session: MarketSession,
    pub price: Price,
    pub volume: Quantity,
    pub trade_type: TradeType,
}

/// The session's own counters a quote push carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CumulativeDay {
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Quantity,
}

impl CumulativeDay {
    /// Reads the session's counters as one candle, which is what a daily candle is.
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

/// One trade type as the venue's wire states it.
///
/// The field is a string on the wire, and the venue sends an empty one for the ordinary
/// case, so "the venue named no type" and "the venue named a type this build does not
/// know" are two different facts: the first is the ordinary print and the second is a
/// type outside the venue's own table, and §4.6 states a different answer for each. The
/// type is a value rather than a borrowed string because it is read off one frame and
/// carried into a candle whose lifetime has nothing to do with that frame's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TradeType {
    /// The venue stated no type at all, which is the ordinary print.
    Unnamed,
    /// The venue stated this type.
    Named(&'static str),
    /// The venue stated a type this build does not know.
    Unknown,
}

impl TradeType {
    /// Reads one wire value as a trade type.
    ///
    /// The types §4.6's table names are the ones the venue's own protocol documents, and
    /// they are spelled here because the table is read by them: a value outside the list
    /// is a type this build does not know, which the table answers with "updates nothing"
    /// rather than by treating it as the ordinary print.
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

/// Which of a candle's fields one trade type is allowed to update (§4.6).
///
/// The table is the venue's own and is copied rather than derived: a type outside it
/// updates nothing, and a reader comparing this build's candles with the venue's history
/// is comparing exactly these decisions. `Nothing` is therefore not "unknown, assume the
/// usual case" — it is the venue saying a print of that type is not a trade at all.
///
/// The `I` row is the one that asks a second question, and the venue's own row answers it
/// with two arms: `I` is a print of an **option**, or of the pre and post sessions.
/// Options are out of scope for what this build publishes — the directory it reads is US
/// common stock, ADRs and preferred shares (§6), and every entry is published as
/// `SecurityForm::CommonStock` (§14.2 item 1) — so a share's `I` print updates the volume
/// only, and an extended-session `I` print is the row's other arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdatedFields {
    /// The price and the volume both move: `""`, `A`, `B`, `D`, `E`, `F`, `K`, `S`, `X`
    /// and `1`, and `I` for a print of an option or of the pre or post session.
    PriceAndVolume,
    /// Only the volume moves: `C`, `G`, `H`, `V`, `W`, and `I` on every other session —
    /// a share's regular **or overnight** print.
    VolumeOnly,
    /// Nothing moves: `M`, `P`, and every type the venue has not named to this build.
    Nothing,
}

impl UpdatedFields {
    /// Reads one trade type as the fields it may update (§4.6).
    ///
    /// The venue's ordinary print states no type at all, which the table names with its
    /// `""` row: [`TradeType::of`] reads that empty value as [`TradeType::Unnamed`], so
    /// this is the one place the two spellings of the same fact meet. The session is what
    /// the `I` row's other half is read from, and the venue's own row names the two
    /// sessions it is about: an `I` print of the **pre** or **post** session moves
    /// everything it carries, and one of the regular session or of the overnight session
    /// moves the volume alone. This build holds no option, so the row's option arm is the
    /// one arm it does not need; it is stated here rather than left out, because a
    /// midnight print filed under the overnight session is exactly the case the volume
    /// arm is for.
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
            // Every other type is one the venue's own table does not name to this build,
            // and a print of one of those updates neither the price nor the volume.
            TradeType::Named(_) => Self::Nothing,
        }
    }

    /// Reports whether this print moves the candle's price.
    pub(crate) const fn updates_price(self) -> bool {
        matches!(self, Self::PriceAndVolume)
    }

    /// Reports whether this print moves the candle's volume.
    pub(crate) const fn updates_volume(self) -> bool {
        matches!(self, Self::PriceAndVolume | Self::VolumeOnly)
    }
}
