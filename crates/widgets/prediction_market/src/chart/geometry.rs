//! Where a prediction chart's boxes, domains and ticks come from.
//!
//! Everything here is a function of the frame's inputs and nothing else: the box the
//! element was given, the timestamps and probabilities of one snapshot, and the
//! display time zone. Nothing paints, nothing reads a window, and nothing here holds
//! state — a resize is a new box, never a recomputation to remember.
//!
//! The chart has no pan or zoom, so its two domains are simple and deliberately
//! *not* wall-clock anchored. The x domain is the data extent, trimmed to the
//! selected range: a quiet market ends where its last sample is. The y domain is the
//! probability extent rounded up to a five-point step, with ticks every ten points.
//!
//! A series is folded here too. A snapshot holds up to [`crate::model::MAX_POINTS`]
//! points per line while the plot is a few hundred pixels wide, so [`decimate`]
//! keeps at most one point per integer pixel column, its first and last visible
//! point, and the last sample at or before the window as a lead. The lead is what
//! makes a step line enter the plot at its left edge and what lets the pointer read a
//! value between two samples.

use gpui_kit::{Bounds, Pixels, point, px, size};
use market_participants_model::{decimal::Fraction, prediction::PredictionRange};
use time::{Date, Month, OffsetDateTime, UtcOffset};

/// The room kept left of the plot.
pub(crate) const LEFT_INSET: f32 = 8.0;
/// The strip above the plot that holds the hovered time.
pub(crate) const TOP_STRIP: f32 = 18.0;
/// The right gutter, which holds the y labels.
pub(crate) const RIGHT_GUTTER: f32 = 40.0;
/// The band under the plot that holds the x labels.
pub(crate) const BOTTOM_BAND: f32 = 30.0;

/// The step every y label is drawn at, in percentage points.
pub(crate) const Y_TICK_STEP: f64 = 10.0;
/// The step the y domain is rounded up to, in percentage points.
pub(crate) const Y_ROUNDING_STEP: f64 = 5.0;
/// The smallest top of the y domain, so a quiet market is not drawn flat on a
/// zero-height axis.
pub(crate) const Y_MINIMUM_MAX: f64 = 10.0;

/// The window a single sample is padded out to on both sides.
pub(crate) const SINGLE_SAMPLE_PADDING_MILLIS: i64 = 30 * 60 * 1_000;

/// The least room a clock label needs before the next one, which is the narrowest of
/// the four label tiers.
pub(crate) const CLOCK_TICK_SPACING: f32 = 54.0;
/// The least room an `MMM D` label needs before the next one.
const DAY_TICK_SPACING: f32 = 56.0;
/// The least room an `MMM` label needs before the next one.
const MONTH_TICK_SPACING: f32 = 40.0;
/// The least room an `MMM YYYY` label needs before the next one.
const MONTH_YEAR_TICK_SPACING: f32 = 72.0;
/// A chart too narrow for one label per tier spacing still gets two labels.
pub(crate) const MINIMUM_TICK_COUNT: usize = 2;

/// The most clock labels an axis shows once its window is longer than twelve hours.
///
/// Past that point a clock time can come back around — a full day ends on the hour it
/// began with — so the axis steps up to a coarser clock step instead of drawing a
/// repeated time.
const CLOCK_LABEL_CAP: usize = 12;
/// The window past which clock labels can repeat, in milliseconds.
const CLOCK_CAP_SPAN_MILLIS: i64 = 12 * 3_600_000;

/// The span past which month labels carry their year, so a multi-year axis is not a
/// string of repeated month names. Fifteen months (about 457 days) stays bare, as
/// the reference chart's own 16-label axis does.
const MONTH_YEAR_LABEL_SPAN_MILLIS: i64 = 550 * 86_400_000;

/// One day, in milliseconds.
const DAY_MILLIS: i64 = 86_400_000;

/// The share of [`Fraction::ONE`] one percentage point is, as `f64`; a fraction's
/// inner value is scaled by `1e18`, so a percentage is one `1e16`th of it.
const PERCENT_SCALE: f64 = 1e16;

/// The box a chart is drawn in, and the plot inside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    /// The box the canvas was given, which is also the map's content mask.
    pub bounds: Bounds<Pixels>,
    /// The only box data is drawn in.
    pub plot: Bounds<Pixels>,
}

impl ChartLayout {
    /// Places the plot inside `bounds`, reserving the strips around it.
    pub(crate) fn new(bounds: Bounds<Pixels>) -> Self {
        let plot = Bounds::new(
            point(
                bounds.origin.x + px(LEFT_INSET),
                bounds.origin.y + px(TOP_STRIP),
            ),
            size(
                (bounds.size.width - px(LEFT_INSET + RIGHT_GUTTER)).max(Pixels::ZERO),
                (bounds.size.height - px(TOP_STRIP + BOTTOM_BAND)).max(Pixels::ZERO),
            ),
        );
        Self { bounds, plot }
    }

    /// The strip above the plot that holds the hovered time.
    pub(crate) fn top_strip(self) -> Bounds<Pixels> {
        Bounds::new(
            self.bounds.origin,
            size(self.bounds.size.width, px(TOP_STRIP)),
        )
    }

    /// The band under the plot that holds the x labels.
    pub(crate) fn bottom_band(self) -> Bounds<Pixels> {
        Bounds::new(
            point(self.bounds.origin.x, self.plot.bottom()),
            size(self.plot.size.width, px(BOTTOM_BAND)),
        )
    }
}

/// The time a plot covers, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeDomain {
    pub start: i64,
    pub end: i64,
}

impl TimeDomain {
    /// How long the window is, in milliseconds.
    pub(crate) fn span(self) -> i64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether `timestamp` falls inside the window.
    pub(crate) fn contains(self, timestamp: i64) -> bool {
        (self.start..=self.end).contains(&timestamp)
    }
}

/// The time window a snapshot is drawn over.
///
/// The end is always the newest sample. For the whole-life range the start is the
/// oldest one; for a bounded range the start is the larger of the oldest sample and
/// the range's span before the end, so the window keeps its width while live samples
/// extend it, and still shrinks to a market younger than the range.
///
/// A single sample would draw a zero-width axis, so it is padded symmetrically.
pub(crate) fn time_domain(minimum: i64, maximum: i64, range: PredictionRange) -> TimeDomain {
    let (minimum, maximum) = if minimum <= maximum {
        (minimum, maximum)
    } else {
        (maximum, minimum)
    };
    let start = match range.span_millis() {
        Some(span) => minimum.max(maximum.saturating_sub(span)),
        None => minimum,
    };
    if start < maximum {
        TimeDomain {
            start,
            end: maximum,
        }
    } else {
        TimeDomain {
            start: maximum.saturating_sub(SINGLE_SAMPLE_PADDING_MILLIS),
            end: maximum.saturating_add(SINGLE_SAMPLE_PADDING_MILLIS),
        }
    }
}

/// The x a timestamp is drawn at, clamped to the plot's edges.
pub(crate) fn x_for_time(domain: TimeDomain, plot: Bounds<Pixels>, timestamp: i64) -> Pixels {
    let span = domain.span();
    if span <= 0 {
        return plot.origin.x + plot.size.width / 2.0;
    }
    let offset = timestamp as f64 - domain.start as f64;
    let fraction = (offset / span as f64).clamp(0.0, 1.0) as f32;
    plot.origin.x + plot.size.width * fraction
}

/// The probability axis: its top and the values it labels.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ValueAxis {
    /// The top of the axis, in percentage points.
    pub maximum: f64,
    /// The values a grid line and a label are drawn at, ascending from zero.
    pub ticks: Vec<f64>,
}

impl ValueAxis {
    /// Builds the axis of one snapshot's largest probability.
    ///
    /// The top is `ceil(max / 5) * 5` percentage points, and never below ten, so a
    /// market around one or two percent still reads against a normal axis. Ticks are
    /// every ten points from zero up to the largest one below the top: a top of 35
    /// labels 0/10/20/30 and leaves the headroom unlittered, which is what the
    /// reference chart does.
    pub(crate) fn new(max_percent: f64) -> Self {
        let maximum = if max_percent.is_finite() {
            (max_percent / Y_ROUNDING_STEP).ceil() * Y_ROUNDING_STEP
        } else {
            0.0
        }
        .max(Y_MINIMUM_MAX);
        let last = (maximum / Y_TICK_STEP).floor() * Y_TICK_STEP;
        let mut ticks = Vec::with_capacity((maximum / Y_TICK_STEP) as usize + 1);
        let mut value = 0.0;
        while value <= last + f64::EPSILON {
            ticks.push(value);
            value += Y_TICK_STEP;
        }
        Self { maximum, ticks }
    }
}

/// The y a percentage is drawn at, clamped to the axis.
pub(crate) fn y_for_percent(percent: f64, axis: &ValueAxis, plot: Bounds<Pixels>) -> Pixels {
    let ratio = if axis.maximum > 0.0 && percent.is_finite() {
        (percent / axis.maximum).clamp(0.0, 1.0)
    } else {
        0.0
    };
    plot.bottom() - plot.size.height * ratio as f32
}

/// One sample in domain units: a time and a probability in percentage points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub timestamp: i64,
    pub percent: f64,
}

/// One sample in screen space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenPoint {
    pub x: Pixels,
    pub y: Pixels,
}

/// A series folded onto the plot.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Decimated {
    /// The points a step path is built from, ascending in x.
    pub screen: Vec<ScreenPoint>,
    /// The screen position of the series' newest point inside the window, which is
    /// what the endpoint and its pulse are drawn at.
    pub last: Option<ScreenPoint>,
}

/// A probability as percentage points, for the chart's y axis.
pub(crate) fn percent(probability: Fraction) -> f64 {
    probability.ratio().inner() as f64 / PERCENT_SCALE
}

/// The index of the last sample at or before `start`.
///
/// It is the sample a step line enters the window from: the horizontal run that
/// starts at the plot's left edge carries this sample's value into the window.
pub(crate) fn lead_index(samples: &[Sample], start: i64) -> Option<usize> {
    samples
        .partition_point(|sample| sample.timestamp <= start)
        .checked_sub(1)
}

/// The largest probability drawn inside `domain`, counting the lead sample.
///
/// The lead is counted because it is drawn: the line enters the plot at its value.
pub(crate) fn visible_max_percent(samples: &[Sample], domain: TimeDomain) -> f64 {
    let lead = lead_index(samples, domain.start);
    samples
        .iter()
        .enumerate()
        .filter(|(index, sample)| Some(*index) == lead || domain.contains(sample.timestamp))
        .map(|(_, sample)| sample.percent)
        .filter(|percent| percent.is_finite())
        .fold(0.0, f64::max)
}

/// Folds a series onto the plot.
///
/// At most one point is kept per integer pixel column, plus the first and last
/// visible point — a column that received several samples keeps the last, which is
/// the value a step line holds for that column. The sample at or before the window's
/// start is kept as the lead, so the line enters at the left edge.
pub(crate) fn decimate(
    samples: &[Sample],
    domain: TimeDomain,
    axis: &ValueAxis,
    plot: Bounds<Pixels>,
) -> Decimated {
    if samples.is_empty() || plot.size.width <= Pixels::ZERO || plot.size.height <= Pixels::ZERO {
        return Decimated::default();
    }
    let lead = lead_index(samples, domain.start);
    let first_visible = lead.map_or(0, |index| index + 1);
    let mut first_visible_index: Option<usize> = None;
    let mut last_visible_index: Option<usize> = None;
    let mut columns: Vec<(i32, usize)> = Vec::new();
    for (index, sample) in samples.iter().enumerate().skip(first_visible) {
        if !domain.contains(sample.timestamp) {
            if sample.timestamp > domain.end {
                break;
            }
            continue;
        }
        let x = x_for_time(domain, plot, sample.timestamp);
        let column = (f32::from(x - plot.origin.x)).floor() as i32;
        match columns.last_mut() {
            Some((last_column, last_index)) if *last_column == column => *last_index = index,
            _ => columns.push((column, index)),
        }
        first_visible_index.get_or_insert(index);
        last_visible_index = Some(index);
    }
    let mut chosen: Vec<usize> = Vec::with_capacity(columns.len() + 2);
    if let Some(lead) = lead {
        chosen.push(lead);
    }
    if let Some(first) = first_visible_index
        && columns.first().map(|&(_, index)| index) != Some(first)
    {
        chosen.push(first);
    }
    chosen.extend(columns.iter().map(|&(_, index)| index));
    chosen.dedup();
    let screen: Vec<ScreenPoint> = chosen
        .into_iter()
        .map(|index| {
            let sample = samples[index];
            ScreenPoint {
                x: x_for_time(domain, plot, sample.timestamp),
                y: y_for_percent(sample.percent, axis, plot),
            }
        })
        .collect();
    let last = last_visible_index.map(|index| ScreenPoint {
        x: x_for_time(domain, plot, samples[index].timestamp),
        y: y_for_percent(samples[index].percent, axis, plot),
    });
    Decimated { screen, last }
}

/// What a time-axis label says, chosen together with the step that produces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeLabelKind {
    /// The time of day, e.g. `8:00 AM`.
    Clock,
    /// The date, e.g. `Jan 5`.
    Day,
    /// The month, e.g. `Jan`.
    Month,
    /// The month and the year, e.g. `Jan 2026`.
    MonthYear,
}

impl TimeLabelKind {
    /// The least room one label of this kind leaves for the next.
    const fn spacing(self) -> f32 {
        match self {
            Self::Clock => CLOCK_TICK_SPACING,
            Self::Day => DAY_TICK_SPACING,
            Self::Month => MONTH_TICK_SPACING,
            Self::MonthYear => MONTH_YEAR_TICK_SPACING,
        }
    }
}

/// The fixed-length step a time axis is labelled at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeStep {
    Minute1,
    Minute5,
    Minute10,
    Minute30,
    Hour1,
    Hour3,
    Hour6,
    Hour12,
    Day1,
    Day3,
    Week1,
    Week2,
    Month1,
    Month3,
    Month6,
    Year1,
}

impl TimeStep {
    /// The steps a window of a day or less is labelled with, ascending.
    const SUB_DAY: [Self; 8] = [
        Self::Minute1,
        Self::Minute5,
        Self::Minute10,
        Self::Minute30,
        Self::Hour1,
        Self::Hour3,
        Self::Hour6,
        Self::Hour12,
    ];

    /// The steps a window longer than a day is labelled with, ascending.
    ///
    /// A calendar axis never starts below a day: a step that labels `Jan 5` cannot
    /// run more than once a day without repeating itself.
    const CALENDAR: [Self; 8] = [
        Self::Day1,
        Self::Day3,
        Self::Week1,
        Self::Week2,
        Self::Month1,
        Self::Month3,
        Self::Month6,
        Self::Year1,
    ];

    /// The step's length when it is a fixed number of milliseconds.
    const fn fixed_millis(self) -> Option<i64> {
        match self {
            Self::Minute1 => Some(60_000),
            Self::Minute5 => Some(300_000),
            Self::Minute10 => Some(600_000),
            Self::Minute30 => Some(1_800_000),
            Self::Hour1 => Some(3_600_000),
            Self::Hour3 => Some(10_800_000),
            Self::Hour6 => Some(21_600_000),
            Self::Hour12 => Some(43_200_000),
            Self::Day1 => Some(DAY_MILLIS),
            Self::Day3 => Some(3 * DAY_MILLIS),
            Self::Week1 => Some(7 * DAY_MILLIS),
            Self::Week2 => Some(14 * DAY_MILLIS),
            Self::Month1 | Self::Month3 | Self::Month6 | Self::Year1 => None,
        }
    }

    /// The step's length when it is a whole number of calendar months.
    const fn months(self) -> Option<i64> {
        match self {
            Self::Month1 => Some(1),
            Self::Month3 => Some(3),
            Self::Month6 => Some(6),
            Self::Year1 => Some(12),
            _ => None,
        }
    }

    /// What a tick of this step says, given the window it labels.
    ///
    /// The unit follows the step, not the raw span: a day step labels the date and a
    /// month step the month. The one span-dependent choice is the month step's year,
    /// which only a multi-year window needs to keep its months apart.
    const fn label_kind(self, span: i64) -> TimeLabelKind {
        match self {
            Self::Minute1
            | Self::Minute5
            | Self::Minute10
            | Self::Minute30
            | Self::Hour1
            | Self::Hour3
            | Self::Hour6
            | Self::Hour12 => TimeLabelKind::Clock,
            Self::Day1 | Self::Day3 | Self::Week1 | Self::Week2 => TimeLabelKind::Day,
            Self::Month1 | Self::Month3 | Self::Month6 => {
                if span > MONTH_YEAR_LABEL_SPAN_MILLIS {
                    TimeLabelKind::MonthYear
                } else {
                    TimeLabelKind::Month
                }
            }
            Self::Year1 => TimeLabelKind::MonthYear,
        }
    }

    /// The next coarser step, which is where an axis whose aligned ticks overflow
    /// steps up to. A year has nothing above it.
    const fn next(self) -> Option<Self> {
        match self {
            Self::Minute1 => Some(Self::Minute5),
            Self::Minute5 => Some(Self::Minute10),
            Self::Minute10 => Some(Self::Minute30),
            Self::Minute30 => Some(Self::Hour1),
            Self::Hour1 => Some(Self::Hour3),
            Self::Hour3 => Some(Self::Hour6),
            Self::Hour6 => Some(Self::Hour12),
            Self::Hour12 => Some(Self::Day1),
            Self::Day1 => Some(Self::Day3),
            Self::Day3 => Some(Self::Week1),
            Self::Week1 => Some(Self::Week2),
            Self::Week2 => Some(Self::Month1),
            Self::Month1 => Some(Self::Month3),
            Self::Month3 => Some(Self::Month6),
            Self::Month6 => Some(Self::Year1),
            Self::Year1 => None,
        }
    }
}

/// The timestamps a time axis is labelled at, and the unit their labels are in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimeTicks {
    pub label: TimeLabelKind,
    pub timestamps: Vec<i64>,
}

/// The timestamps a time axis is labelled at.
///
/// The step and the unit of its labels are chosen together, because a step only keeps
/// its labels disjoint if it also leaves them the room their own format needs. A
/// window longer than a day starts at a day step, so the axis never selects an hour
/// step whose labels cannot say anything finer than `Jan 5`.
///
/// The smallest step whose *estimated* number of ticks fits the plot's room for that
/// label tier is chosen and only then materialized: the estimate is arithmetic
/// (`span / step`, or the exact distance between month indices), so no finer step's
/// full tick list is ever built. If the chosen step's aligned ticks still overflow,
/// the axis steps up once. Only a year step, which has nothing above it, strides its
/// ticks instead.
pub(crate) fn time_ticks(domain: TimeDomain, plot_width: Pixels, zone: UtcOffset) -> TimeTicks {
    let span = domain.span();
    if span <= 0 {
        return TimeTicks {
            label: TimeLabelKind::Clock,
            timestamps: Vec::new(),
        };
    }
    let candidates = if span <= DAY_MILLIS {
        TimeStep::SUB_DAY.as_slice()
    } else {
        TimeStep::CALENDAR.as_slice()
    };
    let mut chosen = *candidates
        .last()
        .expect("the step candidates are never empty");
    let mut maximum = maximum_tick_count(plot_width, chosen.label_kind(span), span);
    for &step in candidates {
        let limit = maximum_tick_count(plot_width, step.label_kind(span), span);
        if estimated_tick_count(step, domain, zone) <= limit {
            chosen = step;
            maximum = limit;
            break;
        }
    }
    let mut timestamps = ticks_for(chosen, domain, zone);
    if timestamps.len() > maximum {
        match chosen.next() {
            Some(next) => {
                chosen = next;
                timestamps = ticks_for(next, domain, zone);
            }
            None => {
                let stride = timestamps.len().div_ceil(maximum.max(1));
                timestamps = timestamps.into_iter().step_by(stride.max(1)).collect();
            }
        }
    }
    TimeTicks {
        label: chosen.label_kind(span),
        timestamps,
    }
}

/// The most labels a plot has room for at one label tier's spacing.
///
/// `n` labels occupy `n - 1` gaps, so the count is one more than the whole gaps the
/// plot fits. A clock axis longer than twelve hours is capped as well, so it steps up
/// instead of running out of distinct times.
fn maximum_tick_count(plot_width: Pixels, label: TimeLabelKind, span: i64) -> usize {
    let count =
        ((f32::from(plot_width) / label.spacing()).floor() as usize + 1).max(MINIMUM_TICK_COUNT);
    if label == TimeLabelKind::Clock && span > CLOCK_CAP_SPAN_MILLIS {
        count.min(CLOCK_LABEL_CAP)
    } else {
        count
    }
}

/// The most ticks a step can place inside `domain`, without materializing them.
///
/// The estimate is an upper bound: a fixed step's aligned ticks are at least one step
/// apart, and month alignment can only drop boundaries, never add one.
fn estimated_tick_count(step: TimeStep, domain: TimeDomain, zone: UtcOffset) -> usize {
    if let Some(months) = step.months() {
        return estimated_month_ticks(months, domain, zone);
    }
    match step.fixed_millis() {
        Some(millis) if millis > 0 => (domain.span() / millis) as usize + 1,
        _ => usize::MAX,
    }
}

/// The most month boundaries a month step can place inside `domain`.
fn estimated_month_ticks(months_per_step: i64, domain: TimeDomain, zone: UtcOffset) -> usize {
    let (Some(start), Some(end)) = (
        local_datetime(domain.start, zone),
        local_datetime(domain.end, zone),
    ) else {
        return usize::MAX;
    };
    let aligned = month_index(start).div_euclid(months_per_step) * months_per_step;
    let count = (month_index(end) - aligned).div_euclid(months_per_step) + 1;
    usize::try_from(count.max(0)).unwrap_or(usize::MAX)
}

/// The month an instant falls in, counted from January 1970.
fn month_index(datetime: OffsetDateTime) -> i64 {
    i64::from(datetime.year()) * 12 + i64::from(datetime.month() as u8) - 1
}

/// The timestamps one step produces inside `domain`.
fn ticks_for(step: TimeStep, domain: TimeDomain, zone: UtcOffset) -> Vec<i64> {
    if let Some(months) = step.months() {
        return month_ticks(months, domain, zone);
    }
    match step.fixed_millis() {
        Some(millis) => fixed_ticks(millis, domain, zone),
        None => Vec::new(),
    }
}

/// The boundaries of a fixed step, aligned to the display time zone.
///
/// A local timestamp is the UTC one shifted by the offset, so the aligned instant is
/// the first local multiple of the step at or after the window's start.
fn fixed_ticks(step_millis: i64, domain: TimeDomain, zone: UtcOffset) -> Vec<i64> {
    if step_millis <= 0 {
        return Vec::new();
    }
    let offset = i64::from(zone.whole_seconds()) * 1_000;
    let local_start = domain.start.saturating_add(offset);
    let aligned = local_start.div_euclid(step_millis) * step_millis - offset;
    let mut tick = if aligned < domain.start {
        aligned.saturating_add(step_millis)
    } else {
        aligned
    };
    let mut ticks = Vec::new();
    while tick <= domain.end {
        ticks.push(tick);
        let Some(next) = tick.checked_add(step_millis) else {
            break;
        };
        tick = next;
    }
    ticks
}

/// The first of every `months_per_step` months inside `domain`.
fn month_ticks(months_per_step: i64, domain: TimeDomain, zone: UtcOffset) -> Vec<i64> {
    if months_per_step <= 0 {
        return Vec::new();
    }
    let Some(start) = local_datetime(domain.start, zone) else {
        return Vec::new();
    };
    let mut index = month_index(start).div_euclid(months_per_step) * months_per_step;
    let mut ticks = Vec::new();
    while let Some(boundary) = month_start(index, zone) {
        if boundary > domain.end {
            break;
        }
        if boundary >= domain.start {
            ticks.push(boundary);
        }
        let Some(next) = index.checked_add(months_per_step) else {
            break;
        };
        index = next;
    }
    ticks
}

/// The timestamp of the first instant of month `index`, counted from January 1970.
fn month_start(index: i64, zone: UtcOffset) -> Option<i64> {
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = u8::try_from(index.rem_euclid(12) + 1).ok()?;
    let month = Month::try_from(month).ok()?;
    let date = Date::from_calendar_date(year, month, 1).ok()?;
    date.midnight()
        .assume_offset(zone)
        .unix_timestamp()
        .checked_mul(1_000)
}

/// A millisecond timestamp in the display time zone.
pub(crate) fn local_datetime(timestamp: i64, zone: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .map(|datetime| datetime.to_offset(zone))
}
