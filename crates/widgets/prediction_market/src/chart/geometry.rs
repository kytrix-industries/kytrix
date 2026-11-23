use market_participants_model::{decimal::Fraction, prediction::PredictionRange};
use time::{Date, Month, OffsetDateTime, UtcOffset};
use ui_core::math::{Point, Rect, Size};

pub(crate) const LEFT_INSET: f32 = 8.0;
pub(crate) const TOP_STRIP: f32 = 18.0;
pub(crate) const RIGHT_GUTTER: f32 = 40.0;
pub(crate) const BOTTOM_BAND: f32 = 30.0;

pub(crate) const Y_TICK_STEP: f64 = 10.0;
pub(crate) const Y_ROUNDING_STEP: f64 = 5.0;
pub(crate) const Y_MINIMUM_MAX: f64 = 10.0;

pub(crate) const SINGLE_SAMPLE_PADDING_MILLIS: i64 = 30 * 60 * 1_000;

pub(crate) const CLOCK_TICK_SPACING: f32 = 54.0;
const DAY_TICK_SPACING: f32 = 56.0;
const MONTH_TICK_SPACING: f32 = 40.0;
const MONTH_YEAR_TICK_SPACING: f32 = 72.0;
pub(crate) const MINIMUM_TICK_COUNT: usize = 2;

const CLOCK_LABEL_CAP: usize = 12;
const CLOCK_CAP_SPAN_MILLIS: i64 = 12 * 3_600_000;

const MONTH_YEAR_LABEL_SPAN_MILLIS: i64 = 550 * 86_400_000;

const DAY_MILLIS: i64 = 86_400_000;

const PERCENT_SCALE: f64 = 1e16;

const SERIES_EDGE_INSET: f32 = 2.0;

pub(crate) trait RectExt {
    fn right(self) -> f32;
    fn bottom(self) -> f32;
}

impl RectExt for Rect {
    fn right(self) -> f32 {
        self.origin.x + self.size.width
    }

    fn bottom(self) -> f32 {
        self.origin.y + self.size.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    pub bounds: Rect,
    pub plot: Rect,
}

impl ChartLayout {
    pub(crate) fn new(bounds: Rect) -> Self {
        let plot = Rect::from_origin_size(
            Point::new(bounds.origin.x + LEFT_INSET, bounds.origin.y + TOP_STRIP),
            Size::new(
                (bounds.size.width - (LEFT_INSET + RIGHT_GUTTER)).max(0.0),
                (bounds.size.height - (TOP_STRIP + BOTTOM_BAND)).max(0.0),
            ),
        );
        Self { bounds, plot }
    }

    pub(crate) fn top_strip(self) -> Rect {
        Rect::from_origin_size(
            self.bounds.origin,
            Size::new(self.bounds.size.width, TOP_STRIP),
        )
    }

    pub(crate) fn bottom_band(self) -> Rect {
        Rect::from_origin_size(
            Point::new(self.bounds.origin.x, self.plot.bottom()),
            Size::new(self.plot.size.width, BOTTOM_BAND),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeDomain {
    pub start: i64,
    pub end: i64,
}

impl TimeDomain {
    pub(crate) fn span(self) -> i64 {
        self.end.saturating_sub(self.start)
    }

    pub(crate) fn contains(self, timestamp: i64) -> bool {
        (self.start..=self.end).contains(&timestamp)
    }
}

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

pub(crate) fn x_for_time(domain: TimeDomain, plot: Rect, timestamp: i64) -> f32 {
    let span = domain.span();
    if span <= 0 {
        return plot.origin.x + plot.size.width / 2.0;
    }
    let offset = timestamp as f64 - domain.start as f64;
    let fraction = (offset / span as f64).clamp(0.0, 1.0) as f32;
    plot.origin.x + plot.size.width * fraction
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ValueAxis {
    pub maximum: f64,
    pub ticks: Vec<f64>,
}

impl ValueAxis {
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

pub(crate) fn y_for_percent(percent: f64, axis: &ValueAxis, plot: Rect) -> f32 {
    let ratio = if axis.maximum > 0.0 && percent.is_finite() {
        (percent / axis.maximum).clamp(0.0, 1.0)
    } else {
        0.0
    };
    plot.bottom() - plot.size.height * ratio as f32
}

pub(crate) fn y_for_series(percent: f64, axis: &ValueAxis, plot: Rect) -> f32 {
    let y = y_for_percent(percent, axis, plot);
    if plot.size.height <= SERIES_EDGE_INSET * 2.0 {
        return y;
    }
    let top = plot.origin.y + SERIES_EDGE_INSET;
    let bottom = plot.bottom() - SERIES_EDGE_INSET;
    let fraction = (y - plot.origin.y) / plot.size.height;
    top + (bottom - top) * fraction
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub timestamp: i64,
    pub percent: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Decimated {
    pub screen: Vec<ScreenPoint>,
    pub last: Option<ScreenPoint>,
}

pub(crate) fn percent(probability: Fraction) -> f64 {
    probability.ratio().inner() as f64 / PERCENT_SCALE
}

pub(crate) fn lead_index(samples: &[Sample], start: i64) -> Option<usize> {
    samples
        .partition_point(|sample| sample.timestamp <= start)
        .checked_sub(1)
}

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

pub(crate) fn decimate(
    samples: &[Sample],
    domain: TimeDomain,
    axis: &ValueAxis,
    plot: Rect,
) -> Decimated {
    if samples.is_empty() || plot.size.width <= 0.0 || plot.size.height <= 0.0 {
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
        let column = (x - plot.origin.x).floor() as i32;
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
    let mut screen: Vec<ScreenPoint> = Vec::with_capacity(chosen.len());
    let mut drawn_y: Option<f32> = None;
    for index in chosen {
        let sample = samples[index];
        let y = y_for_series(sample.percent, axis, plot);
        let y = match drawn_y {
            Some(previous) if (y - previous).abs() < 1.0 => previous,
            _ => y,
        };
        drawn_y = Some(y);
        screen.push(ScreenPoint {
            x: x_for_time(domain, plot, sample.timestamp),
            y,
        });
    }
    let last = last_visible_index.and_then(|_| screen.last().copied());
    Decimated { screen, last }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeLabelKind {
    Clock,
    Day,
    Month,
    MonthYear,
}

impl TimeLabelKind {
    const fn spacing(self) -> f32 {
        match self {
            Self::Clock => CLOCK_TICK_SPACING,
            Self::Day => DAY_TICK_SPACING,
            Self::Month => MONTH_TICK_SPACING,
            Self::MonthYear => MONTH_YEAR_TICK_SPACING,
        }
    }
}

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

    const fn months(self) -> Option<i64> {
        match self {
            Self::Month1 => Some(1),
            Self::Month3 => Some(3),
            Self::Month6 => Some(6),
            Self::Year1 => Some(12),
            _ => None,
        }
    }

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimeTicks {
    pub label: TimeLabelKind,
    pub timestamps: Vec<i64>,
}

pub(crate) fn time_ticks(domain: TimeDomain, plot_width: f32, zone: UtcOffset) -> TimeTicks {
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

fn maximum_tick_count(plot_width: f32, label: TimeLabelKind, span: i64) -> usize {
    let count = ((plot_width / label.spacing()).floor() as usize + 1).max(MINIMUM_TICK_COUNT);
    if label == TimeLabelKind::Clock && span > CLOCK_CAP_SPAN_MILLIS {
        count.min(CLOCK_LABEL_CAP)
    } else {
        count
    }
}

fn estimated_tick_count(step: TimeStep, domain: TimeDomain, zone: UtcOffset) -> usize {
    if let Some(months) = step.months() {
        return estimated_month_ticks(months, domain, zone);
    }
    match step.fixed_millis() {
        Some(millis) if millis > 0 => (domain.span() / millis) as usize + 1,
        _ => usize::MAX,
    }
}

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

fn month_index(datetime: OffsetDateTime) -> i64 {
    i64::from(datetime.year()) * 12 + i64::from(datetime.month() as u8) - 1
}

fn ticks_for(step: TimeStep, domain: TimeDomain, zone: UtcOffset) -> Vec<i64> {
    if let Some(months) = step.months() {
        return month_ticks(months, domain, zone);
    }
    match step.fixed_millis() {
        Some(millis) => fixed_ticks(millis, domain, zone),
        None => Vec::new(),
    }
}

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

pub(crate) fn local_datetime(timestamp: i64, zone: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .map(|datetime| datetime.to_offset(zone))
}
