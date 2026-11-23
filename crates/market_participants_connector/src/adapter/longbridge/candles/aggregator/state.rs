use market_participants_model::decimal::{Price, Quantity, UnixMillis};

use super::super::{
    aggregator::{CandlePrint, UpdatedFields},
    session_table::{
        CandleBucket, SessionTable, calendar_bucket, calendar_unit, fixed_interval_millis,
    },
};
use crate::{adapter::NativeCandlestick, market::timeframe::TimeFrame};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CandleOutcome {
    Updated(NativeCandlestick),
    Rolled {
        confirmed: NativeCandlestick,
        opened: NativeCandlestick,
    },
    Stale,
}

#[derive(Debug)]
pub(crate) struct CandleAggregator {
    timeframe: TimeFrame,
    zone: jiff::tz::TimeZone,
    bucket: i64,
    candle: NativeCandlestick,
    open_updated: bool,
}

impl CandleAggregator {
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

    #[cfg(test)]
    pub(crate) fn candle(&self) -> &NativeCandlestick {
        &self.candle
    }

    pub(crate) fn bucket_of(
        &self,
        print: &CandlePrint,
        table: &SessionTable,
    ) -> Option<CandleBucket> {
        let local = jiff::Timestamp::from_millisecond(print.timestamp)
            .ok()
            .map(|timestamp| jiff::Zoned::new(timestamp, self.zone.clone()))?;

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
                return CandleOutcome::Updated(self.candle.clone());
            }
            let updated = self.merge(print, fields);
            self.candle = updated.clone();
            return CandleOutcome::Updated(updated);
        }

        let confirmed = self.candle.clone();
        let (opened, open_updated) = self.open(print, start, confirmed.close);
        self.bucket = start;
        self.candle = opened.clone();
        self.open_updated = open_updated;
        CandleOutcome::Rolled { confirmed, opened }
    }

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
