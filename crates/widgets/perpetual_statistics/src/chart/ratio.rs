//! The long/short ratio chart: the ratio as a line, with the two sides it implies
//! stacked behind it.
//!
//! Only the ratio is published, and it is the long side over the short side, so the two
//! shares a stacked bar needs are that ratio resolved against a whole. Drawing both is
//! the point: the ratio says how lopsided the market is, and the stack says which way,
//! which a ratio below one makes easy to misread.

use std::sync::Arc;

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_model::listing::rules::ListingPrecision;
use smol_str::SmolStr;
use time::UtcOffset;

use super::{
    axes::{AxisScale, ChartLayout, ratio_domain, share_scale, valid_index},
    canvas::ChartData,
    paint::{Prepared, Tooltip, TooltipEntry, prepare_ratio, time_labels},
    palette::{Series, SeriesLabel},
    text::{format_axis_time, format_ratio, format_share, format_tooltip_time, labelled_value},
};
use crate::model::{LongShortRatioPoint, StatisticsPoints, ratio_as_f64, ratio_shares};

/// The share axis: a whole is one, so its ticks are the same whatever the data says, and
/// they are written as the percentages the shares are read as.
const SHARE_TICKS: [(f64, &str); 5] = [
    (0.0, "0%"),
    (0.25, "25%"),
    (0.5, "50%"),
    (0.75, "75%"),
    (1.0, "100%"),
];

/// The names one ratio chart shows, resolved from the active language.
///
/// The three ratio widgets share this chart but not their wording: "long accounts" and
/// "long positions" are different readings of the same series, so the names are given to
/// the chart rather than chosen by it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Labels {
    long: SmolStr,
    short: SmolStr,
    ratio: SmolStr,
}

impl Labels {
    /// Resolves the series names from three message keys.
    fn resolve(translate: &Translate, keys: [&'static str; 3]) -> Self {
        Self {
            long: t_smol!(translate, keys[0]),
            short: t_smol!(translate, keys[1]),
            ratio: t_smol!(translate, keys[2]),
        }
    }
}

/// The ratio chart's state.
pub(crate) struct RatioChart {
    points: Arc<[LongShortRatioPoint]>,
    /// The three message keys the labels were resolved from, kept so a language change
    /// can resolve them again.
    keys: [&'static str; 3],
    labels: Labels,
    locale: LanguageCode,
    revision: u64,
    shares: AxisScale,
    ratio: AxisScale,
    time_labels: Box<[SmolStr]>,
    tooltips: Arc<[Tooltip]>,
    legend: Box<[SeriesLabel]>,
    /// The indices whose ratio is a reading, which is what a hover may land on and the
    /// only points the chart draws.
    valid: Box<[usize]>,
    /// The zone the axis labels and tooltips were written in.
    zone: UtcOffset,
}

impl RatioChart {
    /// Builds an empty chart of `labels`' series.
    pub(crate) fn new(
        translate: &Translate,
        long: &'static str,
        short: &'static str,
        ratio: &'static str,
        zone: UtcOffset,
    ) -> Self {
        let keys = [long, short, ratio];
        Self::build(
            Arc::from(Vec::new()),
            0,
            keys,
            Labels::resolve(translate, keys),
            translate.language(),
            zone,
        )
    }

    fn build(
        points: Arc<[LongShortRatioPoint]>,
        revision: u64,
        keys: [&'static str; 3],
        labels: Labels,
        locale: LanguageCode,
        zone: UtcOffset,
    ) -> Self {
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
        let time_labels = time_labels(points.iter().map(|point| point.timestamp), |timestamp| {
            format_axis_time(timestamp, zone)
        });
        let tooltips = points
            .iter()
            .map(|point| Tooltip {
                title: format_tooltip_time(point.timestamp, zone),
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
            keys,
            legend: vec![
                SeriesLabel {
                    series: Series::Long,
                    label: labels.long.clone(),
                },
                SeriesLabel {
                    series: Series::Short,
                    label: labels.short.clone(),
                },
                SeriesLabel {
                    series: Series::Ratio,
                    label: labels.ratio.clone(),
                },
            ]
            .into_boxed_slice(),
            labels,
            locale,
            revision,
            shares,
            ratio,
            time_labels,
            tooltips,
            valid,
            zone,
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

    fn scales(&self) -> (Option<&AxisScale>, &AxisScale) {
        (Some(&self.shares), &self.ratio)
    }

    fn legend(&self) -> &[SeriesLabel] {
        &self.legend
    }

    fn axis_titles(&self) -> (&str, &str) {
        ("", "")
    }

    fn prepare(&self, layout: ChartLayout) -> Prepared {
        prepare_ratio(&self.points, &self.ratio, &self.time_labels, layout)
    }

    fn hover_point(&self, index: usize) -> Option<usize> {
        // A point whose ratio is not a reading has no shares to annotate, so a hover over
        // one moves on to the nearest that has.
        valid_index(&self.valid, index)
    }

    fn tooltip(&self, index: usize) -> Option<Tooltip> {
        self.tooltips
            .get(index)
            .filter(|tooltip| !tooltip.entries.is_empty())
            .cloned()
    }

    /// Rewrites the labels in another display zone.
    fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        if self.zone == zone {
            return false;
        }
        let (points, revision, keys, labels, locale) = (
            Arc::clone(&self.points),
            self.revision,
            self.keys,
            self.labels.clone(),
            self.locale,
        );
        *self = Self::build(points, revision, keys, labels, locale, zone);
        true
    }

    fn set_locale(&mut self, translate: &Translate) -> bool {
        if self.locale == translate.language() {
            return false;
        }
        let labels = Labels::resolve(translate, self.keys);
        if labels == self.labels {
            self.locale = translate.language();
            return false;
        }
        let (points, revision, keys, locale, zone) = (
            Arc::clone(&self.points),
            self.revision,
            self.keys,
            translate.language(),
            self.zone,
        );
        *self = Self::build(points, revision, keys, labels, locale, zone);
        true
    }

    fn set_snapshot(
        &mut self,
        revision: u64,
        _metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool {
        // A revision that moved is a new window even when its points are the same values:
        // the pane asked for this one, and it is the one it is now showing.
        if revision < self.revision
            || (revision == self.revision && Arc::ptr_eq(&self.points, &points))
        {
            return false;
        }
        let (keys, labels, locale, zone) = (self.keys, self.labels.clone(), self.locale, self.zone);
        *self = Self::build(points, revision, keys, labels, locale, zone);
        true
    }
}
