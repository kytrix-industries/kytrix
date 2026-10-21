//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing this chart: one axis tick is a text
//! layout, and a frame draws dozens of them plus a hover label whose runs change
//! with the candle under the cursor. Both are therefore shaped once and reused:
//!
//! - [`ChartTextCache`] keeps the last [`TEXT_CACHE_CAPACITY`] `(text, colour)`
//!   layouts, so ticks that did not move are not re-shaped as the chart pans,
//! - [`HoverLabelCache`] keeps the single most recent label together with every
//!   input that produced it, so moving the cursor *within* one candle re-uses the
//!   shaped line instead of re-shaping it on every mouse move.
//!
//! Both caches are keyed on the window's text style as well as on their own
//! inputs: a style or font change invalidates shaped text even when the numbers
//! did not change.

use std::sync::Arc;

use base_ui::style::{components::ChartColors, theme::Theme};
use gpui_kit::{Hsla, Pixels, ShapedLine, SharedString, TextRun, TextStyle, Window, px};
use market_participants_connector::market::timeframe::TimeFrame;
use market_participants_model::{
    decimal::{Price, ProfitOrLoss, Quantity, Tick},
    market::book::ListingCandlestick,
};
use smol_str::{SmolStr, format_smolstr};
use time::UtcOffset;

use crate::{model::CandlePrices, time::format_crosshair_time};

/// Font size of every axis and tag label.
pub(crate) const AXIS_FONT_SIZE: f32 = 10.0;
/// Font size of the hover label.
pub(crate) const HOVER_LABEL_FONT_SIZE: f32 = 11.0;
pub(crate) const HOVER_LABEL_LINE_HEIGHT: f32 = 14.0;
const TEXT_CACHE_CAPACITY: usize = 256;

/// What a candle's traded size reads as when it published none.
///
/// A listing that does not publish a traded size is not one whose size is zero,
/// so the label says the number is missing rather than inventing one.
const MISSING_VOLUME: &str = "—";

/// Resolves [`format_volume`] once and keeps the result.
struct CachedTextLine {
    text: SharedString,
    color: Hsla,
    line: Arc<ShapedLine>,
}

/// Shaped axis labels, keyed by text and colour.
pub(crate) struct ChartTextCache {
    text_style: Option<TextStyle>,
    entries: Vec<CachedTextLine>,
}

impl Default for ChartTextCache {
    fn default() -> Self {
        Self {
            text_style: None,
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }
}

impl ChartTextCache {
    pub(crate) fn clear(&mut self) {
        self.text_style = None;
        self.entries.clear();
    }

    /// Shapes `text` in `color`, or returns the line that was shaped before.
    pub(crate) fn line(
        &mut self,
        text: SharedString,
        color: Hsla,
        window: &Window,
    ) -> Arc<ShapedLine> {
        let text_style = window.text_style();
        if self.text_style.as_ref() != Some(&text_style) {
            self.text_style = Some(text_style.clone());
            self.entries.clear();
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.color == color)
        {
            // Refresh the entry's recency by moving it to the back, which is
            // what makes the capacity below evict the least recently used label.
            let entry = self.entries.remove(index);
            let line = entry.line.clone();
            self.entries.push(entry);
            return line;
        }

        let text_len = text.len();
        let line = Arc::new(window.text_system().shape_line(
            text.clone(),
            px(AXIS_FONT_SIZE),
            &[TextRun {
                len: text_len,
                font: text_style.font(),
                color,
                ..Default::default()
            }],
            None,
        ));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedTextLine {
            text,
            color,
            line: line.clone(),
        });
        line
    }

    /// The width `text` occupies, shaped in `color`.
    pub(crate) fn width(&mut self, text: &str, color: Hsla, window: &Window) -> Pixels {
        self.line(SharedString::new(text), color, window).width()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoverLabelTone {
    Muted,
    Candle,
}

#[derive(Debug, PartialEq, Eq)]
struct HoverLabelSpan {
    text: SmolStr,
    tone: HoverLabelTone,
    bold: bool,
}

impl HoverLabelSpan {
    fn key(text: &'static str) -> Self {
        Self {
            text: SmolStr::new_static(text),
            tone: HoverLabelTone::Muted,
            bold: true,
        }
    }

    fn muted(text: &'static str) -> Self {
        Self {
            text: SmolStr::new_static(text),
            tone: HoverLabelTone::Muted,
            bold: false,
        }
    }

    fn candle(text: impl AsRef<str>) -> Self {
        Self {
            text: SmolStr::new(text.as_ref()),
            tone: HoverLabelTone::Candle,
            bold: false,
        }
    }
}

/// The hover label, already shaped.
#[derive(Clone)]
pub(crate) struct HoverLabelPresentation {
    pub time_label: SharedString,
    pub line: Arc<ShapedLine>,
}

/// Every input that decides what the hover label looks like.
struct HoverLabelCacheInput<'a> {
    candle: &'a ListingCandlestick,
    previous_close: Option<Price>,
    price_tick: Tick,
    interval: TimeFrame,
    /// The zone the crosshair's date is read in.
    zone: UtcOffset,
    volume_unit: &'a SharedString,
    text_style: &'a TextStyle,
    muted_color: Hsla,
    candle_color: Hsla,
}

struct HoverLabelCacheEntry {
    candle: ListingCandlestick,
    previous_close: Option<Price>,
    price_tick: Tick,
    interval: TimeFrame,
    zone: UtcOffset,
    volume_unit: SharedString,
    text_style: TextStyle,
    muted_color: Hsla,
    candle_color: Hsla,
    presentation: HoverLabelPresentation,
}

impl HoverLabelCacheEntry {
    fn matches(&self, input: &HoverLabelCacheInput<'_>) -> bool {
        self.candle == *input.candle
            && self.previous_close == input.previous_close
            && self.price_tick == input.price_tick
            && self.interval == input.interval
            && self.zone == input.zone
            && self.volume_unit == *input.volume_unit
            && self.text_style == *input.text_style
            && self.muted_color == input.muted_color
            && self.candle_color == input.candle_color
    }
}

/// The hover label of the candle the cursor is over, shaped at most once per
/// candle.
#[derive(Default)]
pub(crate) struct HoverLabelCache {
    entry: Option<HoverLabelCacheEntry>,
}

impl HoverLabelCache {
    pub(crate) fn clear(&mut self) {
        self.entry = None;
    }

    /// Shapes the label of `candle`, reading its traded size in `volume_unit`.
    ///
    /// The unit is the text the candle's own quantity is labelled with — an asset
    /// name, a contract count — so the size the label shows says what it counts
    /// instead of leaving the reader to assume one.
    pub(crate) fn presentation(
        &mut self,
        candle: &ListingCandlestick,
        previous: Option<&ListingCandlestick>,
        price_tick: Tick,
        interval: TimeFrame,
        zone: UtcOffset,
        volume_unit: &SharedString,
        theme: &Theme,
        window: &Window,
    ) -> HoverLabelPresentation {
        let previous_close = previous.map(|candle| CandlePrices::of(candle).close());
        let text_style = window.text_style();
        let muted_color = theme.roles.fg.muted.hsla;
        let candle_color = candle_color(candle, &theme.components.chart);
        let cache_input = HoverLabelCacheInput {
            candle,
            previous_close,
            price_tick,
            interval,
            zone,
            volume_unit,
            text_style: &text_style,
            muted_color,
            candle_color,
        };
        if let Some(entry) = self
            .entry
            .as_ref()
            .filter(|entry| entry.matches(&cache_input))
        {
            return entry.presentation.clone();
        }

        let presentation = HoverLabelPresentation {
            time_label: SharedString::new(
                format_crosshair_time(candle.timestamp, interval, zone).as_str(),
            ),
            line: Arc::new(shape_hover_label(
                candle,
                previous,
                price_tick,
                volume_unit,
                muted_color,
                candle_color,
                &text_style,
                window,
            )),
        };
        self.entry = Some(HoverLabelCacheEntry {
            candle: candle.clone(),
            previous_close,
            price_tick,
            interval,
            zone,
            volume_unit: volume_unit.clone(),
            text_style,
            muted_color,
            candle_color,
            presentation: presentation.clone(),
        });
        presentation
    }
}

fn hover_label_spans(
    candle: &ListingCandlestick,
    previous: Option<&ListingCandlestick>,
    price_tick: Tick,
    volume_unit: &str,
) -> Vec<HoverLabelSpan> {
    let prices = CandlePrices::of(candle);
    let mut spans = Vec::with_capacity(23);
    spans.push(HoverLabelSpan::key("O"));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::candle(
        prices.open().format_and_round_tick(price_tick),
    ));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::key("H"));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::candle(
        prices.high().format_and_round_tick(price_tick),
    ));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::key("L"));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::candle(
        prices.low().format_and_round_tick(price_tick),
    ));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::key("C"));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::candle(
        prices.close().format_and_round_tick(price_tick),
    ));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::key("V"));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::candle(format_traded_size(
        candle,
        volume_unit,
    )));
    spans.push(HoverLabelSpan::muted(" "));
    spans.push(HoverLabelSpan::candle(format_price_change(
        candle, price_tick,
    )));
    if let Some(amplitude) = format_amplitude(candle, previous) {
        spans.push(HoverLabelSpan::muted(" "));
        spans.push(HoverLabelSpan::candle(amplitude));
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

fn shape_hover_label(
    candle: &ListingCandlestick,
    previous: Option<&ListingCandlestick>,
    price_tick: Tick,
    volume_unit: &str,
    muted_color: Hsla,
    candle_color: Hsla,
    text_style: &TextStyle,
    window: &Window,
) -> ShapedLine {
    let spans = hover_label_spans(candle, previous, price_tick, volume_unit);
    let text_capacity = spans.iter().map(|span| span.text.len()).sum();
    let mut text = String::with_capacity(text_capacity);
    let mut runs = Vec::with_capacity(spans.len());
    let font = text_style.font();
    let bold_font = font.clone().bold();
    for span in spans {
        let color = match span.tone {
            HoverLabelTone::Muted => muted_color,
            HoverLabelTone::Candle => candle_color,
        };
        runs.push(TextRun {
            len: span.text.len(),
            font: if span.bold {
                bold_font.clone()
            } else {
                font.clone()
            },
            color,
            ..Default::default()
        });
        text.push_str(&span.text);
    }

    window.text_system().shape_line(
        SharedString::new(text.as_str()),
        px(HOVER_LABEL_FONT_SIZE),
        &runs,
        None,
    )
}

/// The colour a candle's numbers are drawn in: its own direction, not the
/// previous candle's.
fn candle_color(candle: &ListingCandlestick, chart: &ChartColors) -> Hsla {
    if CandlePrices::of(candle).is_up() {
        chart.bullish.hsla
    } else {
        chart.bearish.hsla
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

fn format_price_change(candle: &ListingCandlestick, price_tick: Tick) -> SmolStr {
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
fn format_amplitude(
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

    fn hover_label_text(
        candle: &ListingCandlestick,
        previous: Option<&ListingCandlestick>,
        tick: Tick,
        volume_unit: &str,
    ) -> String {
        hover_label_spans(candle, previous, tick, volume_unit)
            .into_iter()
            .map(|span| span.text)
            .map(|text| text.as_str().to_owned())
            .collect()
    }

    fn tick(value: &str) -> Tick {
        Tick::from_str(value).expect("price tick should parse")
    }

    #[test]
    fn hover_label_should_format_ohlcv_change_and_amplitude() {
        let previous = candle_with_ohlcv(1, "95", "105", "90", "100", "10");
        let candle = candle_with_ohlcv(2, "100", "110", "90", "105", "1234");

        assert_eq!(
            hover_label_text(&candle, Some(&previous), tick("0.01"), "BTC"),
            "O 100.00 H 110.00 L 90.00 C 105.00 V 1.234K BTC 5.00 20.00%"
        );
    }

    /// A traded size is read in the unit it was published with, and a candle that
    /// published none says so instead of showing a number nobody published.
    #[test]
    fn hover_label_should_read_the_traded_size_in_its_own_unit() {
        let candle = candle_with_ohlcv(1, "100", "110", "90", "105", "12");
        let mut missing = candle.clone();
        missing.traded_quantity = None;
        let mut contracts = candle.clone();
        contracts.traded_quantity = Some(MarketQuantity {
            value: Quantity::from(12_u64),
            unit: QuantityUnit::Contracts,
        });

        assert!(hover_label_text(&candle, None, tick("0.01"), "BTC").contains("V 12.000 BTC"));
        assert!(
            hover_label_text(&contracts, None, tick("0.01"), "Contracts")
                .contains("V 12.000 Contracts")
        );
        assert!(hover_label_text(&missing, None, tick("0.01"), "BTC").contains("V —"));
    }

    #[test]
    fn hover_label_should_mark_every_numeric_span_as_candle_colored() {
        let previous = candle_with_ohlcv(1, "95", "105", "90", "100", "10");
        let candle = candle_with_ohlcv(2, "100", "110", "90", "105", "1234");
        let spans = hover_label_spans(&candle, Some(&previous), tick("0.01"), "BTC");
        let numeric_spans = spans
            .iter()
            .filter(|span| span.tone == HoverLabelTone::Candle)
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
    fn hover_label_should_bold_only_ohlcv_keys() {
        let previous = candle_with_ohlcv(1, "95", "105", "90", "100", "10");
        let candle = candle_with_ohlcv(2, "100", "110", "90", "105", "1234");
        let spans = hover_label_spans(&candle, Some(&previous), tick("0.01"), "BTC");
        let bold_spans = spans
            .iter()
            .filter(|span| span.bold)
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(bold_spans, ["O", "H", "L", "C", "V"]);
    }

    #[test]
    fn hover_label_should_omit_amplitude_when_previous_candle_is_absent() {
        let candle = candle_with_ohlcv(1, "100", "110", "90", "105", "1234");

        assert_eq!(
            hover_label_text(&candle, None, tick("0.01"), "BTC"),
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
        let theme = Theme::default();
        let chart = &theme.components.chart;
        let rising = candle_with_ohlcv(1, "100", "110", "90", "105", "1");
        let falling = candle_with_ohlcv(2, "105", "110", "90", "100", "1");
        let flat = candle_with_ohlcv(3, "100", "100", "100", "100", "1");

        assert_eq!(
            [
                candle_color(&rising, chart),
                candle_color(&falling, chart),
                candle_color(&flat, chart),
            ],
            [chart.bullish.hsla, chart.bearish.hsla, chart.bullish.hsla]
        );
    }
}
