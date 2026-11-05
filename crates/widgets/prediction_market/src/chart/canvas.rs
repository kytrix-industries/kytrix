//! The canvas one prediction chart is: its options, its prepared frame, and the
//! cache that keeps a pointer-only frame from preparing anything again.
//!
//! A chart is an element rather than an entity, and the difference matters. The
//! pane's body is already the frame's cache unit, so a chart that were an entity of
//! its own would add a second cache boundary for nothing; what it needs instead is
//! somewhere to keep the things a frame reuses — the box it was laid out in, the
//! domains, the ticks, the paths — and that is [`PreparedChart`], held behind an `Rc`
//! because the element that draws it is rebuilt every frame while the prepared chart
//! is not.
//!
//! The cache key is exactly what preparing *reads*: the snapshot by identity, the box
//! the element was given, the display time zone and the window's text style. The
//! pointer and the pulse phase are deliberately absent, because they change every
//! frame and everything they affect is drawn by [`super::paint`] from the prepared
//! frame. A pointer that moved therefore reuses the same `Rc<PreparedChart>` — same
//! allocation, no geometry rebuilt.

#[cfg(test)]
use std::cell::Cell;
use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui_kit::{
    AnyElement, App, Bounds, IntoElement, Path, PathBuilder, Pixels, Point, Styled, TextStyle,
    Window, canvas, point, px,
};
use smol_str::SmolStr;
use time::UtcOffset;
use widget::time_zone::data_time_zone;

use super::{
    geometry::{
        ChartLayout, Sample, ScreenPoint, TimeDomain, ValueAxis, decimate, percent, time_domain,
        time_ticks, visible_max_percent, x_for_time,
    },
    hover::{HoveredSample, nearest_timestamp},
    paint::{SERIES_HALO_WIDTH, SERIES_STROKE_WIDTH},
    text::{ChartTextCache, format_axis_labels, format_percent_integer},
};
use crate::model::{PredictionMarketSeries, PredictionMarketSnapshot};

/// One point a time axis is labelled and gridded at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TimeTick {
    pub timestamp: i64,
    /// Where the tick is drawn, which is also where its label is centred.
    pub x: Pixels,
    pub label: SmolStr,
}

/// One series folded onto the plot.
///
/// The original samples are kept beside the screen points: the pointer reads a value
/// from them at any time, while the screen points are already decimated and cannot
/// answer for a time they no longer carry. Both paths are built once, here, and a
/// hovering frame paints clones of them under a mask instead of rebuilding them.
pub(crate) struct PreparedSeries {
    pub label: SmolStr,
    /// The index into the theme's series palette.
    pub color: usize,
    /// Every sample of the line, ascending in time.
    pub samples: Arc<[Sample]>,
    /// The decimated points the line is drawn from, including the lead.
    pub screen: Arc<[ScreenPoint]>,
    /// The step path drawn over `screen`, at the series' stroke width.
    pub path: Option<Path<Pixels>>,
    /// The same path at the halo's width, drawn under `path`.
    pub halo: Option<Path<Pixels>>,
    /// The newest point inside the window, which is where the endpoint is drawn.
    pub last: Option<ScreenPoint>,
}

impl PreparedSeries {
    /// Whether the line reaches `timestamp`.
    ///
    /// A step line keeps holding its last value after its last sample, but there is no
    /// point there to mark: a hover dot or a label at a later x would belong to a part
    /// of the plot this line never crossed.
    pub(crate) fn covers(&self, timestamp: i64) -> bool {
        self.samples
            .last()
            .is_some_and(|last| timestamp <= last.timestamp)
    }
}

/// Everything one frame is drawn from, prepared from the element's box and the
/// snapshot under it.
pub(crate) struct PreparedChart {
    pub layout: ChartLayout,
    /// The time window, or `None` for a snapshot with no samples at all.
    pub domain: Option<TimeDomain>,
    pub axis: ValueAxis,
    /// The y-axis label of every one of [`ValueAxis::ticks`], formatted once here so
    /// no paint pass has to format a number.
    pub axis_labels: Arc<[SmolStr]>,
    pub time_ticks: Arc<[TimeTick]>,
    pub series: Arc<[PreparedSeries]>,
    /// Every sample time drawn in the window, sorted and unique, which is what the
    /// pointer snaps to.
    pub hover_times: Arc<[i64]>,
    /// The display time zone the axis labels were formatted in; the hovered time is
    /// formatted in it while the pointer moves.
    pub time_zone: UtcOffset,
}

impl PreparedChart {
    /// Folds one snapshot into the boxes, paths and labels a frame draws.
    fn new(
        snapshot: &PredictionMarketSnapshot,
        bounds: Bounds<Pixels>,
        time_zone: UtcOffset,
    ) -> Self {
        let layout = ChartLayout::new(bounds);
        let Some(newest) = snapshot.latest_timestamp() else {
            return Self {
                layout,
                domain: None,
                axis: ValueAxis::new(0.0),
                axis_labels: Arc::default(),
                time_ticks: Arc::default(),
                series: Arc::default(),
                hover_times: Arc::default(),
                time_zone,
            };
        };
        let mut oldest = i64::MAX;
        for series in snapshot.series.iter() {
            for sample in series.points.iter() {
                oldest = oldest.min(sample.timestamp);
            }
        }

        let domain = time_domain(oldest, newest, snapshot.range);
        let samples: Vec<Arc<[Sample]>> = snapshot
            .series
            .iter()
            .map(|series| Arc::<[Sample]>::from(samples_of(series)))
            .collect();
        let highest = samples
            .iter()
            .map(|samples| visible_max_percent(samples, domain))
            .fold(0.0, f64::max);
        let axis = ValueAxis::new(highest);
        let axis_labels: Arc<[SmolStr]> = axis
            .ticks
            .iter()
            .map(|tick| format_percent_integer(*tick))
            .collect();
        let chosen = time_ticks(domain, layout.plot.size.width, time_zone);
        let labels = format_axis_labels(&chosen.timestamps, chosen.label, time_zone);
        let time_ticks: Vec<TimeTick> = chosen
            .timestamps
            .into_iter()
            .zip(labels)
            .map(|(timestamp, label)| TimeTick {
                timestamp,
                x: x_for_time(domain, layout.plot, timestamp),
                label,
            })
            .collect();

        let mut hover_times: Vec<i64> = Vec::new();
        let series: Arc<[PreparedSeries]> = snapshot
            .series
            .iter()
            .zip(samples.iter())
            .map(|(line, samples)| {
                let folded = decimate(samples, domain, &axis, layout.plot);
                hover_times.extend(
                    samples
                        .iter()
                        .filter(|sample| domain.contains(sample.timestamp))
                        .map(|sample| sample.timestamp),
                );
                PreparedSeries {
                    label: line.label.clone(),
                    color: line.color,
                    samples: samples.clone(),
                    path: step_path(&folded.screen, SERIES_STROKE_WIDTH),
                    halo: step_path(&folded.screen, SERIES_HALO_WIDTH),
                    screen: folded.screen.into(),
                    last: folded.last,
                }
            })
            .collect();
        hover_times.sort_unstable();
        hover_times.dedup();

        Self {
            layout,
            domain: Some(domain),
            axis,
            axis_labels,
            time_ticks: time_ticks.into(),
            series,
            hover_times: hover_times.into(),
            time_zone,
        }
    }

    /// The hovered sample for a pointer position in window coordinates.
    pub(crate) fn hover_at(&self, pointer: Point<Pixels>) -> Option<HoveredSample> {
        let domain = self.domain?;
        if !self.layout.plot.contains(&pointer) {
            return None;
        }
        let fraction = f32::from(pointer.x - self.layout.plot.origin.x)
            / f32::from(self.layout.plot.size.width.max(Pixels::ZERO));
        let time = domain.start as f64 + f64::from(fraction) * domain.span() as f64;
        let timestamp = nearest_timestamp(&self.hover_times, time as i64)?;
        Some(HoveredSample {
            timestamp,
            x: x_for_time(domain, self.layout.plot, timestamp),
        })
    }
}

/// The samples of one series, converted to percentage points and ordered in time.
fn samples_of(series: &PredictionMarketSeries) -> Vec<Sample> {
    let mut samples: Vec<Sample> = series
        .points
        .iter()
        .map(|point| Sample {
            timestamp: point.timestamp,
            percent: percent(point.probability),
        })
        .collect();
    // The service publishes points in time order, but the hover's binary search and
    // the lead both assume it; sorting here is where that becomes true rather than a
    // convention every reader has to remember.
    samples.sort_by_key(|sample| sample.timestamp);
    samples
}

/// The step-after path through `screen`.
///
/// A step line runs horizontally at each sample's value up to the next sample's x,
/// then jumps vertically to the next value, so the two segments are inserted per
/// pair. A series with a single point has no segment to draw; it is drawn as its
/// endpoint instead.
fn step_path(screen: &[ScreenPoint], width: f32) -> Option<Path<Pixels>> {
    let (first, rest) = screen.split_first()?;
    if rest.is_empty() {
        return None;
    }
    let mut builder = PathBuilder::stroke(px(width));
    builder.move_to(point(first.x, first.y));
    for pair in screen.windows(2) {
        let current = pair[0].y;
        let next = pair[1];
        builder.line_to(point(next.x, current));
        builder.line_to(point(next.x, next.y));
    }
    builder.build().ok()
}

/// The pointer position a hover test drives.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ChartCanvasOptions {
    /// The pointer in window coordinates, or `None` while it is outside the chart.
    pub pointer: Option<Point<Pixels>>,
    /// The endpoint pulse's phase, in `0..2` cycles, while one is running.
    pub pulse_phase: Option<f32>,
}

/// The inputs one prepared chart was built from.
struct PreparedKey {
    snapshot: Arc<PredictionMarketSnapshot>,
    bounds: Bounds<Pixels>,
    time_zone: UtcOffset,
    text_style: TextStyle,
}

impl PreparedKey {
    /// Whether a prepared chart was built from these inputs.
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
            && self.bounds == other.bounds
            && self.time_zone == other.time_zone
            && self.text_style == other.text_style
    }
}

/// The frame one chart prepared last, kept until its inputs change.
#[derive(Default)]
struct PreparedCache {
    key: Option<PreparedKey>,
    prepared: Option<Rc<PreparedChart>>,
}

/// Everything a chart reuses between frames.
pub(crate) struct ChartPaintCaches {
    prepared: RefCell<PreparedCache>,
    /// The shaped labels. Shared with the paint pass, which is the only writer.
    text: RefCell<ChartTextCache>,
    /// How many prepared charts were built, for the test that pins the cache key.
    #[cfg(test)]
    builds: Cell<usize>,
}

impl Default for ChartPaintCaches {
    fn default() -> Self {
        Self {
            prepared: RefCell::new(PreparedCache::default()),
            text: RefCell::new(ChartTextCache::default()),
            #[cfg(test)]
            builds: Cell::new(0),
        }
    }
}

impl ChartPaintCaches {
    /// The shaped labels the paint pass reads and writes.
    pub(crate) fn text_cache(&self) -> &RefCell<ChartTextCache> {
        &self.text
    }

    /// How many prepared charts this cache has built.
    #[cfg(test)]
    pub(crate) fn prepared_builds(&self) -> usize {
        self.builds.get()
    }
}

/// The element one chart draws.
///
/// The element is built from an `Arc` snapshot, the options and the shared cache, so
/// the prepaint closure and the paint closure share the same cache and the same
/// prepared frame without the chart being an entity.
pub(crate) fn chart_element(
    snapshot: Arc<PredictionMarketSnapshot>,
    options: ChartCanvasOptions,
    caches: Rc<ChartPaintCaches>,
) -> AnyElement {
    let prepaint_caches = caches.clone();
    let paint_caches = caches;
    canvas(
        move |bounds, window, cx| prepare_chart(&snapshot, bounds, window, cx, &prepaint_caches),
        move |_, prepared: Rc<PreparedChart>, window, cx| {
            super::paint::paint(&prepared, &options, &paint_caches, window, cx);
        },
    )
    .size_full()
    .into_any_element()
}

/// The prepared chart of `bounds`, rebuilt only when its inputs change.
///
/// The returned `Rc` is the same allocation on a hit, which is what lets a test prove
/// a pointer-only frame prepared nothing.
fn prepare_chart(
    snapshot: &Arc<PredictionMarketSnapshot>,
    bounds: Bounds<Pixels>,
    window: &Window,
    cx: &mut App,
    caches: &ChartPaintCaches,
) -> Rc<PreparedChart> {
    let key = PreparedKey {
        snapshot: snapshot.clone(),
        bounds,
        time_zone: data_time_zone(cx),
        text_style: window.text_style(),
    };
    {
        let cache = caches.prepared.borrow();
        if let (Some(cached), Some(prepared)) = (cache.key.as_ref(), cache.prepared.as_ref())
            && cached.matches(&key)
        {
            return prepared.clone();
        }
    }
    let prepared = Rc::new(PreparedChart::new(snapshot, bounds, key.time_zone));
    #[cfg(test)]
    caches.builds.set(caches.builds.get() + 1);
    let mut cache = caches.prepared.borrow_mut();
    cache.key = Some(key);
    cache.prepared = Some(prepared.clone());
    prepared
}
