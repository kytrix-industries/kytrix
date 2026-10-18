//! The long/short ratio chart: the ratio as a line, with the two sides it implies
//! stacked behind it.
//!
//! Only the ratio is published, and it is the long side over the short side, so
//! the two shares a stacked bar needs are that ratio resolved against a whole.
//! Drawing both is the point: the ratio says how lopsided the market is, and the
//! stack says which way, which a ratio below one makes easy to misread.

use std::sync::Arc;

use gpui_kit::{App, Bounds, Pixels, Point, SharedString};
use i18n::t;
use market_participants_model::listing::rules::ListingPrecision;
use smol_str::SmolStr;

use super::{
    ChartData,
    axes::{AxisScale, ChartLayout, ratio_domain, share_scale, valid_index},
    paint::{FrameKey, Hover, Prepared, Tooltip, TooltipEntry, prepare_ratio, time_labels},
    palette::{Series, SeriesLabel},
    text::{format_axis_time, format_ratio, format_share, format_tooltip_time, labelled_value},
};
use crate::model::{LongShortRatioPoint, StatisticsPoints, ratio_as_f64, ratio_shares};

/// The share axis: a whole is one, so its ticks are the same whatever the data
/// says, and they are written as the percentages the shares are read as.
const SHARE_TICKS: [(f64, &str); 5] = [
    (0.0, "0%"),
    (0.25, "25%"),
    (0.5, "50%"),
    (0.75, "75%"),
    (1.0, "100%"),
];

/// The names one ratio chart shows, resolved from the active locale.
///
/// The three ratio widgets share this chart but not their wording: "long
/// accounts" and "long positions" are different readings of the same series, so
/// the names are given to the chart rather than chosen by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Labels {
    pub long: SmolStr,
    pub short: SmolStr,
    pub ratio: SmolStr,
}

impl Labels {
    /// Resolves the series names from three message keys.
    pub(super) fn resolve(cx: &App, long: &str, short: &str, ratio: &str) -> Self {
        Self {
            long: SmolStr::new(t!(cx, long).as_ref()),
            short: SmolStr::new(t!(cx, short).as_ref()),
            ratio: SmolStr::new(t!(cx, ratio).as_ref()),
        }
    }

    /// The names as a legend, in the order the series are drawn.
    fn legend(&self) -> Box<[SeriesLabel]> {
        vec![
            SeriesLabel {
                series: Series::Long,
                label: self.long.clone(),
            },
            SeriesLabel {
                series: Series::Short,
                label: self.short.clone(),
            },
            SeriesLabel {
                series: Series::Ratio,
                label: self.ratio.clone(),
            },
        ]
        .into_boxed_slice()
    }
}

/// The ratio chart's state.
pub(crate) struct RatioChart {
    points: Arc<[LongShortRatioPoint]>,
    labels: Labels,
    revision: u64,
    shares: AxisScale,
    ratio: AxisScale,
    time_labels: Box<[SmolStr]>,
    tooltips: Arc<[Tooltip]>,
    /// The indices whose ratio is a reading, which is what a hover may land on
    /// and the only points the chart draws.
    valid: Box<[usize]>,
}

impl RatioChart {
    /// Builds an empty chart of `labels`' series.
    pub(super) fn new(cx: &App, long: &str, short: &str, ratio: &str) -> Self {
        Self::build(
            Arc::from(Vec::new()),
            0,
            Labels::resolve(cx, long, short, ratio),
        )
    }

    fn build(points: Arc<[LongShortRatioPoint]>, revision: u64, labels: Labels) -> Self {
        let valid = points
            .iter()
            .enumerate()
            .filter_map(|(index, point)| ratio_shares(point.ratio).map(|_| index))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let shares = share_scale(&SHARE_TICKS);
        let ratio = AxisScale::new(
            ratio_domain(
                points
                    .iter()
                    .filter(|point| ratio_shares(point.ratio).is_some())
                    .map(|point| ratio_as_f64(point.ratio)),
            ),
            |value, _| format_ratio(value),
        );
        let time_labels = time_labels(points.iter().map(|point| point.timestamp), format_axis_time);
        let tooltips = points
            .iter()
            .map(|point| Tooltip {
                title: format_tooltip_time(point.timestamp),
                entries: ratio_shares(point.ratio)
                    .map_or_else(Vec::new, |(long, short)| {
                        vec![
                            TooltipEntry {
                                text: labelled_value(labels.long.as_str(), format_share(long)),
                                color: Some(Series::Long),
                            },
                            TooltipEntry {
                                text: labelled_value(labels.short.as_str(), format_share(short)),
                                color: Some(Series::Short),
                            },
                            TooltipEntry {
                                text: labelled_value(
                                    labels.ratio.as_str(),
                                    format_ratio(ratio_as_f64(point.ratio)),
                                ),
                                color: Some(Series::Ratio),
                            },
                        ]
                    })
                    .into_boxed_slice(),
            })
            .collect::<Vec<_>>()
            .into();
        Self {
            points,
            labels,
            revision,
            shares,
            ratio,
            time_labels,
            tooltips,
            valid,
        }
    }
}

impl ChartData for RatioChart {
    type Slice = [LongShortRatioPoint];

    fn points(snapshot: &StatisticsPoints) -> Option<Arc<Self::Slice>> {
        match snapshot {
            StatisticsPoints::LongShortRatio(points) => Some(points.clone()),
            _ => None,
        }
    }

    fn point_count(&self) -> usize {
        self.points.len()
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn frame_key(&self, bounds: Bounds<Pixels>) -> FrameKey {
        FrameKey::new(bounds, self.revision, true)
    }

    fn scales(&self) -> (Option<&AxisScale>, &AxisScale, Option<&AxisScale>) {
        (Some(&self.shares), &self.ratio, None)
    }

    fn legend(&self, _cx: &App) -> Box<[SeriesLabel]> {
        self.labels.legend()
    }

    fn axis_titles(&self, _cx: &App) -> (Option<SharedString>, SharedString) {
        (None, SharedString::default())
    }

    fn hover_point(
        &self,
        index: usize,
        cursor: Point<Pixels>,
        _layout: ChartLayout,
    ) -> Option<Hover> {
        // A point whose ratio is not a reading has no shares to annotate, so a
        // hover over one moves on to the nearest that has.
        Some(Hover {
            index: valid_index(&self.valid, index)?,
            cursor,
        })
    }

    fn prepare(&self, layout: ChartLayout) -> Prepared {
        prepare_ratio(&self.points, &self.ratio, &self.time_labels, layout)
    }

    fn tooltip(&self, index: usize) -> Option<Tooltip> {
        self.tooltips
            .get(index)
            .filter(|tooltip| !tooltip.entries.is_empty())
            .cloned()
    }

    fn set_snapshot(
        &mut self,
        revision: u64,
        _metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool {
        // A revision that moved is a new window even when its points are the same
        // values: the pane asked for this one, and it is the one it is now showing.
        if revision < self.revision
            || (revision == self.revision && Arc::ptr_eq(&self.points, &points))
        {
            return false;
        }
        let labels = self.labels.clone();
        *self = Self::build(points, revision, labels);
        true
    }
}
