//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing this chart: one axis tick is a text
//! layout, and a frame draws dozens of them plus an OHLCV readout whose runs
//! change with the candle being described. Both are therefore shaped once and
//! reused:
//!
//! - [`ChartTextCache`] keeps the last [`TEXT_CACHE_CAPACITY`] `(text, style)`
//!   layouts, so ticks that did not move are not re-shaped as the chart pans,
//! - [`ReadoutCache`] keeps the single most recent line together with every
//!   input that produced it, so a pointer moving *within* one candle re-uses the
//!   shaped line instead of re-shaping it on every mouse move.
//!
//! Both caches are keyed on the whole [`TextStyle`], which carries the colour and
//! the size as well as the font: a colour or font change invalidates shaped text
//! even when the numbers did not change.
//!
//! # The atlas lease
//!
//! A [`ShapedTextHandle`] names glyph tiles leased from the engine's shared
//! atlas. An atlas generation change (a device-scale change, a glyph-capacity
//! escape, a font-stack switch) or a per-tile eviction leaves those tiles
//! pointing at reused texture, and the engine drops such runs at encode time
//! rather than drawing the wrong pixels. A handle that no longer satisfies its
//! lease is therefore **re-shaped** by both caches, and [`ChartTextCache::has_stale`]
//! / [`ReadoutCache::has_stale`] are what the canvas asks before it decides a
//! frame changed nothing: a canvas kept over a dead lease would lose its text
//! for good.
//!
//! # Multi-coloured text
//!
//! The engine shapes one line per [`TextStyle`], so a line with two colours (the
//! OHLCV readout: muted keys, candle-coloured numbers) is a sequence of shaped
//! runs laid out left to right. [`ReadoutPresentation`] carries them with their
//! x offsets, measured once when the line is shaped.

use std::sync::Arc;

use base_ui::theme::components::ChartColors;
use market_participants_connector::market::timeframe::TimeFrame;
use market_participants_model::{
    decimal::{Price, ProfitOrLoss, Quantity, Tick},
    market::book::ListingCandlestick,
};
use smol_str::{SmolStr, format_smolstr};
use time::UtcOffset;
use ui_core::{
    app::App,
    paint::color::Color,
    text::{ShapedTextHandle, TextStyle},
};

use crate::{model::CandlePrices, time::format_crosshair_time};

/// Font size of every axis and tag label.
pub(crate) const AXIS_FONT_SIZE: f32 = 10.0;
/// Font size of the OHLCV readout line.
pub(crate) const READOUT_FONT_SIZE: f32 = 11.0;
/// Height of one text row: axis ticks, tags, legends and the readout all use it.
pub(crate) const TEXT_ROW_HEIGHT: f32 = 14.0;
const TEXT_CACHE_CAPACITY: usize = 256;
/// The CSS font weight of the readout's keys (`O`/`H`/`L`/`C`/`V`).
const READOUT_KEY_WEIGHT: u16 = 700;

/// What a candle's traded size reads as when it published none.
///
/// A listing that does not publish a traded size is not one whose size is zero,
/// so the label says the number is missing rather than inventing one.
const MISSING_VOLUME: &str = "—";

/// The style `text` is shaped in at `size` and `color`.
pub(crate) fn style_of(size: f32, color: Color) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.color = color;
    style
}

/// One shaped line and the style it was shaped in.
struct CachedTextLine {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

/// Shaped axis labels, keyed by text and style.
pub(crate) struct ChartTextCache {
    entries: Vec<CachedTextLine>,
}

impl ChartTextCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Shapes `text` in `style`, or returns the line that was shaped before.
    ///
    /// A handle whose atlas lease is gone is dropped and shaped again: the glyphs
    /// it named have been reused, so drawing it would be drawing another line's
    /// pixels.
    pub(crate) fn line(
        &mut self,
        app: &mut App,
        text: &str,
        style: &TextStyle,
    ) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            // Refresh the entry's recency by moving it to the back, which is what
            // makes the capacity below evict the least recently used label.
            let entry = self.entries.remove(index);
            if !entry.handle.is_stale() {
                let handle = Arc::clone(&entry.handle);
                self.entries.push(entry);
                return handle;
            }
        }

        let handle = Arc::new(shape_line(app, text, style));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedTextLine {
            text: SmolStr::new(text),
            style: style.clone(),
            handle: Arc::clone(&handle),
        });
        handle
    }

    /// The width `text` occupies, shaped in `style`.
    pub(crate) fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> f32 {
        self.line(app, text, style).size().width
    }

    /// Whether any line this cache holds can no longer be drawn from the atlas.
    pub(crate) fn has_stale(&self) -> bool {
        self.entries.iter().any(|entry| entry.handle.is_stale())
    }

    /// Drops every line whose atlas lease is gone.
    ///
    /// Called when a frame is about to repaint. The lines that frame asks for are
    /// re-shaped on demand by [`Self::line`], and a line it does not ask for again
    /// would otherwise keep [`Self::has_stale`] true — which would rebuild the canvas
    /// on every later frame for a label nobody draws any more.
    pub(crate) fn drop_stale(&mut self) {
        self.entries.retain(|entry| !entry.handle.is_stale());
    }

    /// The text of every line this cache holds, in insertion order.
    ///
    /// Test-only: the canvas read points see glyph counts rather than words, so a
    /// test that asserts *which* labels a frame shaped reads them here.
    #[cfg(test)]
    pub(crate) fn texts(&self) -> Vec<SmolStr> {
        self.entries
            .iter()
            .map(|entry| entry.text.clone())
            .collect()
    }
}

/// Shapes one line, drawing nothing rather than failing a frame when the engine
/// refuses the text: a label is not worth a dropped frame, and the next frame
/// tries again.
///
/// An empty line is the engine's own minimum: it needs no glyphs, so the only
/// thing that can still refuse it is a missing font stack, which is a broken
/// host rather than a broken chart.
fn shape_line(app: &mut App, text: &str, style: &TextStyle) -> ShapedTextHandle {
    app.shape_text(text, style, None, &[])
        .or_else(|_| app.shape_text("", style, None, &[]))
        .expect("an empty line always shapes")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadoutTone {
    Muted,
    Candle,
}

#[derive(Debug, PartialEq, Eq)]
struct ReadoutSpan {
    text: SmolStr,
    tone: ReadoutTone,
    bold: bool,
}

impl ReadoutSpan {
    fn key(text: &'static str) -> Self {
        Self {
            text: SmolStr::new_static(text),
            tone: ReadoutTone::Muted,
            bold: true,
        }
    }

    fn muted(text: &'static str) -> Self {
        Self {
            text: SmolStr::new_static(text),
            tone: ReadoutTone::Muted,
            bold: false,
        }
    }

    fn candle(text: impl AsRef<str>) -> Self {
        Self {
            text: SmolStr::new(text.as_ref()),
            tone: ReadoutTone::Candle,
            bold: false,
        }
    }
}

/// One shaped run of the readout line, and where it starts.
pub(crate) struct ReadoutRun {
    pub handle: Arc<ShapedTextHandle>,
    /// The run's x offset from the start of the readout line.
    pub x: f32,
}

/// The readout line and the crosshair's time tag, already shaped.
#[derive(Clone)]
pub(crate) struct ReadoutPresentation {
    pub time_label: SmolStr,
    /// The line's runs, left to right.
    pub runs: Arc<[ReadoutRun]>,
    /// The line's total width, which is the last run's offset plus its width.
    pub width: f32,
}

/// The inputs the readout line is shaped from, apart from the candle itself.
pub(crate) struct ReadoutSpec<'a> {
    /// The price increment every price in the line is rounded to.
    pub price_tick: Tick,
    /// The interval the crosshair's date is read with.
    pub interval: TimeFrame,
    /// The zone the crosshair's date is read in.
    pub zone: UtcOffset,
    /// The text the candle's traded size is read in.
    pub volume_unit: &'a str,
    /// The colour of the line's keys and separators.
    pub muted_color: Color,
    /// The colour of the line's numbers.
    pub candle_color: Color,
}

struct ReadoutCacheEntry {
    candle: ListingCandlestick,
    previous_close: Option<Price>,
    price_tick: Tick,
    interval: TimeFrame,
    zone: UtcOffset,
    volume_unit: SmolStr,
    muted_color: Color,
    candle_color: Color,
    text_scale: f32,
    presentation: ReadoutPresentation,
}

impl ReadoutCacheEntry {
    fn matches(
        &self,
        candle: &ListingCandlestick,
        previous_close: Option<Price>,
        spec: &ReadoutSpec<'_>,
        text_scale: f32,
    ) -> bool {
        self.candle == *candle
            && self.previous_close == previous_close
            && self.price_tick == spec.price_tick
            && self.interval == spec.interval
            && self.zone == spec.zone
            && self.volume_unit == spec.volume_unit
            && self.muted_color == spec.muted_color
            && self.candle_color == spec.candle_color
            && self.text_scale == text_scale
    }
}

/// The readout of the candle being described, shaped at most once per candle.
#[derive(Default)]
pub(crate) struct ReadoutCache {
    entry: Option<ReadoutCacheEntry>,
}

impl ReadoutCache {
    pub(crate) fn clear(&mut self) {
        self.entry = None;
    }

    /// Whether the shaped line this cache holds can no longer be drawn.
    pub(crate) fn has_stale(&self) -> bool {
        self.entry.as_ref().is_some_and(|entry| {
            entry
                .presentation
                .runs
                .iter()
                .any(|run| run.handle.is_stale())
        })
    }

    /// Drops a line whose atlas lease is gone, so a repainting frame re-shapes it.
    ///
    /// [`Self::presentation`] already re-shapes a stale line when it is asked for one;
    /// this is for the frames that do not ask (a chart with no candle in range would
    /// otherwise keep [`Self::has_stale`] true and rebuild the canvas every frame).
    pub(crate) fn drop_stale(&mut self) {
        if self.has_stale() {
            self.entry = None;
        }
    }

    /// The text of the line this cache holds, run by run.
    ///
    /// Test-only: the canvas read points see glyph counts rather than words, and a
    /// test that asserts *which* candle the readout described needs the words.
    #[cfg(test)]
    pub(crate) fn described_text(&self) -> Option<SmolStr> {
        let entry = self.entry.as_ref()?;
        let mut text = String::new();
        for run in entry.presentation.runs.iter() {
            text.push_str(run.handle.line().text.as_str());
        }
        Some(SmolStr::new(text))
    }

    /// The crosshair's time tag the cached line was shaped with (test read point).
    #[cfg(test)]
    pub(crate) fn described_time_label(&self) -> Option<SmolStr> {
        Some(self.entry.as_ref()?.presentation.time_label.clone())
    }

    /// Shapes the readout for `candle`, reading its traded size in
    /// [`ReadoutSpec::volume_unit`].
    ///
    /// The unit is the text the candle's own quantity is labelled with — an asset
    /// name, a contract count — so the size the label shows says what it counts
    /// instead of leaving the reader to assume one.
    pub(crate) fn presentation(
        &mut self,
        app: &mut App,
        candle: &ListingCandlestick,
        previous: Option<&ListingCandlestick>,
        spec: &ReadoutSpec<'_>,
    ) -> ReadoutPresentation {
        let previous_close = previous.map(|candle| CandlePrices::of(candle).close());
        let text_scale = app.text_scale();
        if let Some(entry) = self
            .entry
            .as_ref()
            .filter(|entry| entry.matches(candle, previous_close, spec, text_scale))
            && !entry
                .presentation
                .runs
                .iter()
                .any(|run| run.handle.is_stale())
        {
            return entry.presentation.clone();
        }

        let (runs, width) = shape_readout(app, candle, previous, spec);
        let presentation = ReadoutPresentation {
            time_label: format_crosshair_time(candle.timestamp, spec.interval, spec.zone),
            runs,
            width,
        };
        self.entry = Some(ReadoutCacheEntry {
            candle: candle.clone(),
            previous_close,
            price_tick: spec.price_tick,
            interval: spec.interval,
            zone: spec.zone,
            volume_unit: SmolStr::new(spec.volume_unit),
            muted_color: spec.muted_color,
            candle_color: spec.candle_color,
            text_scale,
            presentation: presentation.clone(),
        });
        presentation
    }
}

fn readout_spans(
    candle: &ListingCandlestick,
    previous: Option<&ListingCandlestick>,
    price_tick: Tick,
    volume_unit: &str,
) -> Vec<ReadoutSpan> {
    let prices = CandlePrices::of(candle);
    let mut spans = Vec::with_capacity(23);
    spans.push(ReadoutSpan::key("O"));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::candle(
        prices.open().format_and_round_tick(price_tick),
    ));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::key("H"));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::candle(
        prices.high().format_and_round_tick(price_tick),
    ));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::key("L"));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::candle(
        prices.low().format_and_round_tick(price_tick),
    ));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::key("C"));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::candle(
        prices.close().format_and_round_tick(price_tick),
    ));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::key("V"));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::candle(format_traded_size(candle, volume_unit)));
    spans.push(ReadoutSpan::muted(" "));
    spans.push(ReadoutSpan::candle(format_price_change(candle, price_tick)));
    if let Some(amplitude) = format_amplitude(candle, previous) {
        spans.push(ReadoutSpan::muted(" "));
        spans.push(ReadoutSpan::candle(amplitude));
    }
    spans
}

/// The traded size of one candle, read in the unit it was published with.
fn format_traded_size(candle: &ListingCandlestick, volume_unit: &str) -> SmolStr {
    let Some(quantity) = &candle.traded_quantity else {
        return SmolStr::new_static(MISSING_VOLUME);
    };

    format_smolstr!(
        "{} {volume_unit}",
        format_volume_inner(quantity.value.inner())
    )
}

/// Shapes every span of the readout and lays them out left to right.
///
/// Returns the runs with their x offsets and the line's total width.
fn shape_readout(
    app: &mut App,
    candle: &ListingCandlestick,
    previous: Option<&ListingCandlestick>,
    spec: &ReadoutSpec<'_>,
) -> (Arc<[ReadoutRun]>, f32) {
    let spans = readout_spans(candle, previous, spec.price_tick, spec.volume_unit);
    let muted = style_of(READOUT_FONT_SIZE, spec.muted_color);
    let key = TextStyle {
        weight: READOUT_KEY_WEIGHT,
        ..muted.clone()
    };
    let candle_style = style_of(READOUT_FONT_SIZE, spec.candle_color);

    let mut runs = Vec::with_capacity(spans.len());
    let mut x = 0.0;
    for span in spans {
        let style = match (span.tone, span.bold) {
            (ReadoutTone::Muted, true) => &key,
            (ReadoutTone::Muted, false) => &muted,
            (ReadoutTone::Candle, _) => &candle_style,
        };
        let handle = Arc::new(shape_line(app, &span.text, style));
        let width = handle.size().width;
        runs.push(ReadoutRun { handle, x });
        x += width;
    }
    (runs.into(), x)
}

/// The colour a candle's numbers are drawn in: its own direction, not the
/// previous candle's.
pub(crate) fn candle_color(candle: &ListingCandlestick, chart: &ChartColors) -> Color {
    if CandlePrices::of(candle).is_up() {
        chart.bullish
    } else {
        chart.bearish
    }
}

/// Formats a chart coordinate as a price, rounded to the market's tick.
pub(crate) fn format_price(inner: f64, tick: Tick) -> SmolStr {
    let inner = inner.clamp(0.0, u128::MAX as f64).round() as u128;
    Price::from_inner(inner).format_and_round_tick(tick)
}

/// Formats a signed chart coordinate, rounded to the market's tick.
///
/// The MACD sub plot spans `[-m, +m]`, so its axis and legend need a formatter
/// that keeps the sign: [`format_price`] clamps at zero and would label every
/// negative tick as `0`. RSI, KDJ and volume are non-negative and do not use it.
pub(crate) fn format_signed_price(inner: f64, tick: Tick) -> SmolStr {
    let inner = inner.clamp(i128::MIN as f64, i128::MAX as f64).round() as i128;
    ProfitOrLoss::from_inner(inner).format_and_round_tick(tick)
}

/// Formats a chart coordinate as a volume with a `K` / `M` suffix.
pub(crate) fn format_volume(inner: f64) -> SmolStr {
    let scale = Quantity::from(1_u64).inner();
    let inner = inner.clamp(0.0, u128::MAX as f64);
    // Fixed-point thresholds above 2^53 are not exact f64 integers, so choose
    // the suffix before converting the chart coordinate back to u128.
    let (divisor, suffix) = volume_unit_from_chart_value(inner, scale);
    format_volume_with_unit(inner.round() as u128, scale, divisor, suffix)
}

fn format_volume_inner(inner: u128) -> SmolStr {
    let scale = Quantity::from(1_u64).inner();
    let (divisor, suffix) = volume_unit_from_inner(inner, scale);
    format_volume_with_unit(inner, scale, divisor, suffix)
}

pub(crate) fn format_price_change(candle: &ListingCandlestick, price_tick: Tick) -> SmolStr {
    let prices = CandlePrices::of(candle);
    let close = prices.close().inner();
    let open = prices.open().inner();
    let change = if close >= open {
        i128::try_from(close - open).unwrap_or(i128::MAX)
    } else {
        i128::try_from(open - close).map_or(i128::MIN, |change| -change)
    };
    ProfitOrLoss::from_inner(change).format_and_round_tick(price_tick)
}

/// How far the candle moved between its low and its high, against the previous
/// close. Absent when there is no previous candle to measure against.
pub(crate) fn format_amplitude(
    candle: &ListingCandlestick,
    previous: Option<&ListingCandlestick>,
) -> Option<SmolStr> {
    let prices = CandlePrices::of(candle);
    let previous_close = CandlePrices::of(previous?).close().inner();
    if previous_close == 0 {
        return None;
    }

    let price_range = prices.high().inner().saturating_sub(prices.low().inner());
    let percentage = price_range as f64 / previous_close as f64 * 100.0;
    percentage
        .is_finite()
        .then(|| format_smolstr!("{percentage:.2}%"))
}

fn volume_unit_from_chart_value(inner: f64, scale: u128) -> (u128, &'static str) {
    if inner >= (scale * 1_000_000) as f64 {
        (1_000_000, "M")
    } else if inner >= (scale * 1_000) as f64 {
        (1_000, "K")
    } else {
        (1, "")
    }
}

fn volume_unit_from_inner(inner: u128, scale: u128) -> (u128, &'static str) {
    let thousand = scale * 1_000;
    let million = scale * 1_000_000;
    if inner >= million {
        (1_000_000_u128, "M")
    } else if inner >= thousand {
        (1_000_u128, "K")
    } else {
        (1_u128, "")
    }
}

fn format_volume_with_unit(inner: u128, scale: u128, divisor: u128, suffix: &str) -> SmolStr {
    let quantum = scale / 1_000 * divisor;
    let quotient = inner / quantum;
    let remainder = inner % quantum;
    let rounded = quotient + u128::from(remainder >= quantum - remainder);

    format_smolstr!("{}.{:03}{suffix}", rounded / 1_000, rounded % 1_000)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use base_ui::theme::{appearance::Appearance, resolved::Theme};
    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::venue::PriceBasis,
        market::{
            book::ListingCandlestick,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };

    use super::*;

    fn candle_with_ohlcv(
        timestamp: i64,
        open: &str,
        high: &str,
        low: &str,
        close: &str,
        volume: &str,
    ) -> ListingCandlestick {
        let price = |value: &str| MarketPrice {
            value: Price::from_str(value).expect("price should parse"),
            quote_asset: test_support::currency("USD"),
            basis: PriceBasis::PerAssetUnit,
        };
        ListingCandlestick {
            listing: test_support::listing_key(&test_support::spot_route(), "BTCUSDT"),
            open: price(open),
            high: price(high),
            low: price(low),
            close: price(close),
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from_str(volume).expect("volume should parse"),
                unit: QuantityUnit::AssetUnits {
                    asset: test_support::token(test_support::VENUE, "BTC"),
                },
            }),
            traded_notional: None,
            timestamp,
        }
    }

    fn readout_text(
        candle: &ListingCandlestick,
        previous: Option<&ListingCandlestick>,
        tick: Tick,
        volume_unit: &str,
    ) -> String {
        readout_spans(candle, previous, tick, volume_unit)
            .into_iter()
            .map(|span| span.text)
            .map(|text| text.as_str().to_owned())
            .collect()
    }

    fn tick(value: &str) -> Tick {
        Tick::from_str(value).expect("price tick should parse")
    }

    #[test]
    fn readout_should_format_ohlcv_change_and_amplitude() {
        let previous = candle_with_ohlcv(1, "95", "105", "90", "100", "10");
        let candle = candle_with_ohlcv(2, "100", "110", "90", "105", "1234");

        assert_eq!(
            readout_text(&candle, Some(&previous), tick("0.01"), "BTC"),
            "O 100.00 H 110.00 L 90.00 C 105.00 V 1.234K BTC 5.00 20.00%"
        );
    }

    /// A traded size is read in the unit it was published with, and a candle that
    /// published none says so instead of showing a number nobody published.
    #[test]
    fn readout_should_read_the_traded_size_in_its_own_unit() {
        let candle = candle_with_ohlcv(1, "100", "110", "90", "105", "12");
        let mut missing = candle.clone();
        missing.traded_quantity = None;
        let mut contracts = candle.clone();
        contracts.traded_quantity = Some(MarketQuantity {
            value: Quantity::from(12_u64),
            unit: QuantityUnit::Contracts,
        });

        assert!(readout_text(&candle, None, tick("0.01"), "BTC").contains("V 12.000 BTC"));
        assert!(
            readout_text(&contracts, None, tick("0.01"), "Contracts")
                .contains("V 12.000 Contracts")
        );
        assert!(readout_text(&missing, None, tick("0.01"), "BTC").contains("V —"));
    }

    #[test]
    fn readout_should_mark_every_numeric_span_as_candle_colored() {
        let previous = candle_with_ohlcv(1, "95", "105", "90", "100", "10");
        let candle = candle_with_ohlcv(2, "100", "110", "90", "105", "1234");
        let spans = readout_spans(&candle, Some(&previous), tick("0.01"), "BTC");
        let numeric_spans = spans
            .iter()
            .filter(|span| span.tone == ReadoutTone::Candle)
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            numeric_spans,
            [
                "100.00",
                "110.00",
                "90.00",
                "105.00",
                "1.234K BTC",
                "5.00",
                "20.00%"
            ]
        );
    }

    #[test]
    fn readout_should_bold_only_ohlcv_keys() {
        let previous = candle_with_ohlcv(1, "95", "105", "90", "100", "10");
        let candle = candle_with_ohlcv(2, "100", "110", "90", "105", "1234");
        let spans = readout_spans(&candle, Some(&previous), tick("0.01"), "BTC");
        let bold_spans = spans
            .iter()
            .filter(|span| span.bold)
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(bold_spans, ["O", "H", "L", "C", "V"]);
    }

    #[test]
    fn readout_should_omit_amplitude_when_previous_candle_is_absent() {
        let candle = candle_with_ohlcv(1, "100", "110", "90", "105", "1234");

        assert_eq!(
            readout_text(&candle, None, tick("0.01"), "BTC"),
            "O 100.00 H 110.00 L 90.00 C 105.00 V 1.234K BTC 5.00"
        );
    }

    #[test]
    fn price_change_should_round_to_market_tick() {
        let candle = candle_with_ohlcv(1, "100.004", "101", "99", "99.998", "1");

        assert_eq!(format_price_change(&candle, tick("0.01")), "-0.01");
    }

    #[test]
    fn amplitude_should_use_previous_close() {
        let previous = candle_with_ohlcv(1, "75", "85", "70", "80", "1");
        let candle = candle_with_ohlcv(2, "90", "110", "90", "100", "1");

        assert_eq!(
            format_amplitude(&candle, Some(&previous)),
            Some(SmolStr::new_static("25.00%"))
        );
    }

    #[test]
    fn amplitude_should_be_absent_without_previous_candle() {
        let candle = candle_with_ohlcv(1, "90", "110", "90", "100", "1");

        assert_eq!(format_amplitude(&candle, None), None);
    }

    #[test]
    fn amplitude_should_be_absent_when_previous_close_is_zero() {
        let previous = candle_with_ohlcv(1, "0", "0", "0", "0", "1");
        let candle = candle_with_ohlcv(2, "90", "110", "90", "100", "1");

        assert_eq!(format_amplitude(&candle, Some(&previous)), None);
    }

    #[test]
    fn format_price_should_round_and_preserve_tick_precision() {
        let price = Price::from_str("4523.89562").expect("price should parse");

        assert_eq!(format_price(price.inner() as f64, tick("0.01")), "4523.90");
    }

    /// The MACD axis has a negative half, so the signed formatter has to keep the
    /// sign. The plain one deliberately keeps clamping prices at zero.
    #[test]
    fn format_signed_price_should_keep_a_negative_sign() {
        let price = Price::from_str("1.234").expect("price should parse");
        let inner = price.inner() as f64;

        assert_eq!(format_signed_price(-inner, tick("0.01")), "-1.23");
        assert_eq!(format_signed_price(inner, tick("0.01")), "1.23");
    }

    #[test]
    fn format_price_should_clamp_what_format_signed_price_keeps() {
        let price = Price::from_str("1.234").expect("price should parse");
        let inner = -(price.inner() as f64);

        assert_eq!(
            format_price(inner, tick("0.01")),
            "0.00",
            "the price formatter clamps at zero"
        );
        assert_eq!(format_signed_price(inner, tick("0.01")), "-1.23");
    }

    #[test]
    fn format_volume_should_use_three_decimal_places_below_one_thousand() {
        let volume = Quantity::from_str("123.4564").expect("volume should parse");

        assert_eq!(format_volume_inner(volume.inner()), "123.456");
    }

    #[test]
    fn format_volume_should_round_half_up_to_three_decimal_places() {
        let volume = Quantity::from_str("123.4565").expect("volume should parse");

        assert_eq!(format_volume_inner(volume.inner()), "123.457");
    }

    #[test]
    fn format_volume_should_use_thousands_suffix() {
        let volume = Quantity::from(123_456_u64);

        assert_eq!(format_volume_inner(volume.inner()), "123.456K");
    }

    #[test]
    fn format_volume_should_use_millions_suffix() {
        let volume = Quantity::from(123_456_000_u64);

        assert_eq!(format_volume_inner(volume.inner()), "123.456M");
    }

    #[test]
    fn format_volume_should_switch_to_thousands_at_exact_threshold() {
        let thousand = Quantity::from(1_000_u64);

        assert_eq!(format_volume_inner(thousand.inner()), "1.000K");
    }

    #[test]
    fn format_volume_should_keep_no_suffix_below_one_thousand() {
        let volume = Quantity::from_str("999.999").expect("volume should parse");

        assert_eq!(format_volume_inner(volume.inner()), "999.999");
    }

    #[test]
    fn format_volume_should_switch_to_millions_at_exact_threshold() {
        let million = Quantity::from(1_000_000_u64);

        assert_eq!(format_volume_inner(million.inner()), "1.000M");
    }

    #[test]
    fn format_volume_should_keep_thousands_suffix_below_one_million() {
        let volume = Quantity::from(999_999_u64);

        assert_eq!(format_volume_inner(volume.inner()), "999.999K");
    }

    #[test]
    fn format_volume_should_keep_millions_after_chart_float_conversion() {
        let million = Quantity::from(1_000_000_u64);

        assert_eq!(format_volume(million.inner() as f64), "1.000M");
    }

    #[test]
    fn format_volume_should_support_maximum_quantity() {
        assert_eq!(format_volume_inner(u128::MAX), "340282366920938.463M");
    }

    #[test]
    fn candle_color_should_follow_the_candle_direction() {
        let theme = Theme::of(Appearance::Light);
        let chart = theme.components().chart;
        let rising = candle_with_ohlcv(1, "100", "110", "90", "105", "1");
        let falling = candle_with_ohlcv(2, "105", "110", "90", "100", "1");
        let flat = candle_with_ohlcv(3, "100", "100", "100", "100", "1");

        assert_eq!(
            [
                candle_color(&rising, &chart),
                candle_color(&falling, &chart),
                candle_color(&flat, &chart),
            ],
            [chart.bullish, chart.bearish, chart.bullish]
        );
    }

    /// A fresh cache has nothing to lose to an atlas generation change.
    #[test]
    fn an_empty_text_cache_has_no_stale_line() {
        assert!(!ChartTextCache::new().has_stale());
        assert!(!ReadoutCache::default().has_stale());
    }

    /// The inputs one harness readout is shaped from.
    fn readout_spec(volume_unit: &str) -> ReadoutSpec<'_> {
        ReadoutSpec {
            price_tick: tick("0.01"),
            interval: TimeFrame::Minute1,
            zone: UtcOffset::UTC,
            volume_unit,
            muted_color: Color::BLACK,
            candle_color: Color::WHITE,
        }
    }

    /// The words a shaped readout line holds, run by run.
    fn presentation_text(presentation: &ReadoutPresentation) -> String {
        presentation
            .runs
            .iter()
            .map(|run| run.handle.line().text.as_str())
            .collect()
    }

    /// The unit a size is read in is part of the cache key: the same candle with two
    /// different units must not reuse the first presentation.
    ///
    /// Without this, a chart whose quantity unit changes (a relabelled asset, a
    /// contract renamed) would keep printing the old unit until the candle changed.
    #[test]
    fn the_readout_cache_should_key_on_the_volume_unit() {
        let mut app = ui_core::test_support::app::TestApp::new();
        let candle = candle_with_ohlcv(1, "100", "110", "90", "105", "1234");
        let previous = candle_with_ohlcv(0, "95", "105", "90", "100", "10");
        let mut cache = ReadoutCache::default();

        let first = cache.presentation(
            app.app_mut(),
            &candle,
            Some(&previous),
            &readout_spec("BTC"),
        );
        let second = cache.presentation(
            app.app_mut(),
            &candle,
            Some(&previous),
            &readout_spec("Contracts"),
        );

        let first_text = presentation_text(&first);
        let second_text = presentation_text(&second);
        assert!(
            first_text.contains("BTC"),
            "the first presentation reads the size in BTC: {first_text}"
        );
        assert!(
            second_text.contains("Contracts"),
            "the second presentation reads the size in Contracts: {second_text}"
        );
        assert_ne!(
            first_text, second_text,
            "a different unit must produce a different readout text"
        );
        assert!(
            !Arc::ptr_eq(&first.runs, &second.runs),
            "the cache must re-shape rather than return the first entry"
        );
    }

    /// The cache returns the very same shaped runs when every input matches.
    ///
    /// This is what makes a pointer moving *within* one candle cheap: the second call
    /// must not re-shape. It is also the only test that pins the hit path — the unit
    /// test above pins the miss path.
    #[test]
    fn the_readout_cache_should_reuse_the_same_line_for_the_same_candle() {
        let mut app = ui_core::test_support::app::TestApp::new();
        let candle = candle_with_ohlcv(1, "100", "110", "90", "105", "1234");
        let previous = candle_with_ohlcv(0, "95", "105", "90", "100", "10");
        let mut cache = ReadoutCache::default();

        let first = cache.presentation(
            app.app_mut(),
            &candle,
            Some(&previous),
            &readout_spec("BTC"),
        );
        let second = cache.presentation(
            app.app_mut(),
            &candle,
            Some(&previous),
            &readout_spec("BTC"),
        );

        assert!(
            Arc::ptr_eq(&first.runs, &second.runs),
            "identical inputs must reuse the shaped line instead of re-shaping it"
        );
        assert_eq!(
            first.time_label, second.time_label,
            "the reused presentation carries the same time label"
        );
    }
}
