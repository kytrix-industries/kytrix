//! The state machine that builds one candle out of the prints the wire hands it.
//!
//! The value is pure — no socket, no clock, no I/O — because the rules it applies are the
//! venue's and the only way to hold them to it is to state instants on the market's own
//! wall clock and read back the candle they produce. Every update is emitted, including the
//! candle that is still open, because the reader the stream feeds de-duplicates by
//! timestamp: the same candle arrives repeatedly and replaces itself, exactly as a live
//! kline stream's does.

use market_participants_model::decimal::{Price, Quantity, UnixMillis};

use super::super::{
    aggregator::{CandlePrint, UpdatedFields},
    session_table::{
        CandleBucket, SessionTable, calendar_bucket, calendar_unit, fixed_interval_millis,
    },
};
use crate::{adapter::NativeCandlestick, market::timeframe::TimeFrame};

/// What one push did to the candle this aggregator is building.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CandleOutcome {
    /// The push updated the candle being built.
    Updated(NativeCandlestick),
    /// The push opened a new candle; the previous one is the first value.
    Rolled {
        confirmed: NativeCandlestick,
        opened: NativeCandlestick,
    },
    /// The push belongs to a candle this aggregator is no longer building.
    Stale,
}

/// The candle one subscribed interval is currently building from the venue's pushes.
///
/// The value holds the bucket it is building *and* the interval it was told to build,
/// because a fixed-length bucket is the session's open plus whole intervals: the bucket
/// is where the candle starts, and the interval is what decides where the next one
/// starts. A calendar-scale timeframe has no interval and its bucket is the market-local
/// midnight of the period (§4.6 step 2).
#[derive(Debug)]
pub(crate) struct CandleAggregator {
    timeframe: TimeFrame,
    /// The zone the venue opens its candles in (§4.8).
    zone: jiff::tz::TimeZone,
    /// Where the candle being built opens, in milliseconds.
    bucket: i64,
    /// The candle being built, whose timestamp is `bucket`.
    candle: NativeCandlestick,
    /// Whether the open candle's `open` is a price a print stated.
    ///
    /// A candle that opened on a volume-only print has no price of its own yet, so it
    /// opens at the previous candle's close — a price this build made up to keep the
    /// chart continuous rather than one the venue traded at. The first print of that
    /// bucket which moves a price therefore **rewrites** the open, which is the venue's
    /// own rule: its merger carries the same fact and calls it `open_updated` (§4.6
    /// step 3).
    open_updated: bool,
}

impl CandleAggregator {
    /// Opens the candle one first-screen history left off at.
    ///
    /// The opening candles of a subscription are the venue's own history, so the last
    /// one of them is the candle the pushes that arrive next belong to: the aggregator
    /// takes it as the candle being built rather than starting an empty one, and a push
    /// inside its bucket updates exactly the candle a reader already has. The venue
    /// stated that candle's open, so a later price print merges into it rather than
    /// rewriting it.
    pub(crate) fn from_first_screen(
        timeframe: TimeFrame,
        zone: jiff::tz::TimeZone,
        candle: NativeCandlestick,
    ) -> Self {
        Self {
            timeframe,
            zone,
            bucket: candle.timestamp,
            candle,
            open_updated: true,
        }
    }

    /// The candle this aggregator is building, which the tests read back to state what a
    /// push did not change.
    #[cfg(test)]
    pub(crate) fn candle(&self) -> &NativeCandlestick {
        &self.candle
    }

    /// Returns where one print's candle starts, if it starts one at all.
    ///
    /// This is the whole of §4.6 step 2: the gates, then the session's own grid for a
    /// fixed-length interval or the market's calendar boundary for a calendar one.
    pub(crate) fn bucket_of(
        &self,
        print: &CandlePrint,
        table: &SessionTable,
    ) -> Option<CandleBucket> {
        let local = jiff::Timestamp::from_millisecond(print.timestamp)
            .ok()
            .map(|timestamp| jiff::Zoned::new(timestamp, self.zone.clone()))?;

        // Gate: a push outside the regular session never builds a candle of a day or
        // longer, so a daily-scale interval only ever has the regular session's own
        // calendar boundary to work with.
        if let Some(unit) = calendar_unit(self.timeframe) {
            if !print.session.is_normal() {
                return None;
            }
            return calendar_bucket(local, unit).map(|date| CandleBucket::Calendar { date, unit });
        }
        let interval = fixed_interval_millis(self.timeframe)?;
        table
            .fixed_bucket(local, interval, &self.zone)
            .map(CandleBucket::Start)
    }

    /// Applies one print, reporting what it did to the candle.
    ///
    /// The three outcomes are §4.6 step 3: the same bucket updates the candle being
    /// built, a newer bucket confirms it and opens the next one from its close, and an
    /// older bucket is a late push that belongs to a candle this aggregator is no longer
    /// building.
    pub(crate) fn apply(&mut self, print: &CandlePrint, table: &SessionTable) -> CandleOutcome {
        let Some(bucket) = self.bucket_of(print, table) else {
            return CandleOutcome::Stale;
        };
        let Some(start) = bucket
            .start(&self.zone)
            .map(|opened| opened.timestamp().as_millisecond())
        else {
            return CandleOutcome::Stale;
        };
        if start < self.bucket {
            return CandleOutcome::Stale;
        }
        let fields = print.fields();

        if start == self.bucket {
            if !fields.updates_price() && !fields.updates_volume() {
                // A type the table does not name touches nothing, and a candle that is
                // emitted unchanged is what a reader needs to see: the venue's own
                // merger answers the same way, and dropping the frame here would make a
                // test of "nothing moved" indistinguishable from a lost push.
                return CandleOutcome::Updated(self.candle.clone());
            }
            let updated = self.merge(print, fields);
            self.candle = updated.clone();
            return CandleOutcome::Updated(updated);
        }

        // A newer bucket, which this print opens before it is applied to it: §4.6 step 3
        // rolls the open candle and then applies the push that arrived. A print that
        // states no price opens the bucket at the previous candle's close and carries
        // only what it has — its volume — because the venue's own merger appends the
        // candle a push belongs to whether or not that push moved a price.
        let confirmed = self.candle.clone();
        let (opened, open_updated) = self.open(print, start, confirmed.close);
        self.bucket = start;
        self.candle = opened.clone();
        self.open_updated = open_updated;
        CandleOutcome::Rolled { confirmed, opened }
    }

    /// Writes one print into the candle being built.
    ///
    /// A quote of `Day1` is written whole from the venue's own cumulative counters — that
    /// is the session's candle, not an increment of it — and everything else accumulates:
    /// the high and the low follow the price, the close is the price, and the volume is
    /// the volume already there plus what this push added.
    ///
    /// The first price a bucket sees is also its **open** when the bucket was opened by a
    /// print that moved no price: the open such a bucket holds is the previous candle's
    /// close, which is this build's own continuity value rather than a price the venue
    /// traded, so the print that first states a price replaces it. The high and the low are
    /// then that price's order against the value already there — the previous candle's close
    /// stays inside the range exactly as the venue's own merger leaves it, at the end the
    /// price did not move past — which is what keeps them consistent with a candle that
    /// opened there.
    fn merge(&mut self, print: &CandlePrint, fields: UpdatedFields) -> NativeCandlestick {
        let mut candle = self.candle.clone();
        if let Some(cumulative) = print.cumulative {
            self.open_updated = true;
            return cumulative.candle(candle.timestamp);
        }
        if fields.updates_price()
            && let Some(price) = print.price
        {
            if !self.open_updated {
                candle.open = price;
                self.open_updated = true;
            }
            candle.high = candle.high.max(price);
            candle.low = candle.low.min(price);
            candle.close = price;
        }
        if fields.updates_volume()
            && let Some(delta) = print.volume_delta
            && let Some(total) = candle.volume.checked_add(delta)
        {
            candle.volume = total;
        }
        candle
    }

    /// Opens the candle of a newer bucket at one instant.
    ///
    /// The new candle opens at the previous candle's close, which is what makes a chart
    /// of these candles continuous: the design states it (§4.6 step 3), and a print that
    /// arrived after a gap would otherwise leave the gap unstated. The print that opened
    /// the bucket is then applied to it, and the order is the venue's own: a print that
    /// moves a price makes that price the new candle's open, high, low and close — the
    /// continuity value is gone the moment a real trade states one — while a print that
    /// moves only the volume leaves the open and the close where the candle opened and
    /// carries the volume it states, which is the case the venue's merger records as
    /// `open_updated: false`. A print of a `Day1` candle states the whole day instead: the
    /// venue's cumulative counters are that day's candle rather than an increment of one.
    ///
    /// The answer carries whether the open is a price a print stated, which is what
    /// decides whether the next price-moving print of that bucket rewrites it.
    fn open(
        &self,
        print: &CandlePrint,
        start: UnixMillis,
        previous_close: Price,
    ) -> (NativeCandlestick, bool) {
        let mut candle = NativeCandlestick {
            timestamp: start,
            open: previous_close,
            high: previous_close,
            low: previous_close,
            close: previous_close,
            volume: Quantity::default(),
        };
        if let Some(cumulative) = print.cumulative {
            return (cumulative.candle(start), true);
        }
        let fields = print.fields();
        if fields.updates_price()
            && let Some(price) = print.price
        {
            candle.open = price;
            candle.high = price;
            candle.low = price;
            candle.close = price;
        }
        if fields.updates_volume()
            && let Some(delta) = print.volume_delta
            && let Some(total) = candle.volume.checked_add(delta)
        {
            candle.volume = total;
        }
        (candle, fields.updates_price() && print.price.is_some())
    }
}
