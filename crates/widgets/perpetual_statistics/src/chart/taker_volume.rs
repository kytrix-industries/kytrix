//! The taker-volume chart: buy and sell volume as paired bars.
//!
//! Both series are the same interval's volume split by which side crossed the spread, so
//! they share one axis and stand side by side rather than over each other: a stacked bar
//! would say the interval traded their sum, which is exactly what the two sides are not.

use std::sync::Arc;

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_model::listing::rules::ListingPrecision;
use smol_str::SmolStr;
use time::UtcOffset;

use super::{
    axes::{AxisScale, ChartLayout, bar_domain},
    canvas::ChartData,
    paint::{Prepared, Tooltip, TooltipEntry, prepare_taker_volume, time_labels},
    palette::{Series, SeriesLabel},
    text::{
        format_axis_time, format_compact, format_quantity, format_tooltip_time, label_with_unit,
        labelled_value,
    },
};
use crate::model::{StatisticsPoints, TakerVolumePoint, quantity_as_f64};

/// The names this chart shows, resolved from the active language.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Labels {
    buy: SmolStr,
    sell: SmolStr,
    volume: SmolStr,
}

impl Labels {
    /// Resolves the series names from the active language.
    fn resolve(translate: &Translate) -> Self {
        Self {
            buy: t_smol!(translate, "widget.perpetual_statistics.buy_volume"),
            sell: t_smol!(translate, "widget.perpetual_statistics.sell_volume"),
            volume: t_smol!(translate, "widget.perpetual_statistics.volume"),
        }
    }
}

/// The taker-volume chart's state.
pub(crate) struct TakerVolumeChart {
    points: Arc<[TakerVolumePoint]>,
    metadata: Option<ListingPrecision>,
    /// The asset the volume is measured in, which a tooltip names.
    base: SmolStr,
    labels: Labels,
    locale: LanguageCode,
    revision: u64,
    volume: AxisScale,
    time_labels: Box<[SmolStr]>,
    tooltips: Arc<[Tooltip]>,
    legend: Box<[SeriesLabel]>,
    volume_title: SmolStr,
    /// The zone the axis labels and tooltips were written in.
    zone: UtcOffset,
}

impl TakerVolumeChart {
    /// Builds an empty chart of `instrument`'s taker volume.
    pub(crate) fn new(translate: &Translate, base: &str, zone: UtcOffset) -> Self {
        Self::build(
            Arc::from(Vec::new()),
            None,
            0,
            SmolStr::new(base),
            Labels::resolve(translate),
            translate.language(),
            zone,
        )
    }

    fn build(
        points: Arc<[TakerVolumePoint]>,
        metadata: Option<ListingPrecision>,
        revision: u64,
        base: SmolStr,
        labels: Labels,
        locale: LanguageCode,
        zone: UtcOffset,
    ) -> Self {
        let quantity_tick = metadata.map(|metadata| metadata.quantity_tick);
        let volume = AxisScale::new(
            bar_domain(points.iter().flat_map(|point| {
                [
                    quantity_as_f64(point.buy_volume),
                    quantity_as_f64(point.sell_volume),
                ]
            })),
            |value, step| match quantity_tick {
                Some(tick) => format_quantity(value, tick),
                None => format_compact(value, step),
            },
        );
        let time_labels = time_labels(points.iter().map(|point| point.timestamp), |timestamp| {
            format_axis_time(timestamp, zone)
        });
        let buy_label = label_with_unit(labels.buy.as_str(), base.as_str());
        let sell_label = label_with_unit(labels.sell.as_str(), base.as_str());
        let tooltips = points
            .iter()
            .map(|point| Tooltip {
                title: format_tooltip_time(point.timestamp, zone),
                entries: vec![
                    TooltipEntry {
                        text: labelled_value(
                            buy_label.as_str(),
                            quantity_tick.map_or_else(
                                || format_compact(quantity_as_f64(point.buy_volume), 0.0),
                                |tick| format_quantity(quantity_as_f64(point.buy_volume), tick),
                            ),
                        ),
                        color: Some(Series::Long),
                    },
                    TooltipEntry {
                        text: labelled_value(
                            sell_label.as_str(),
                            quantity_tick.map_or_else(
                                || format_compact(quantity_as_f64(point.sell_volume), 0.0),
                                |tick| format_quantity(quantity_as_f64(point.sell_volume), tick),
                            ),
                        ),
                        color: Some(Series::Short),
                    },
                ]
                .into_boxed_slice(),
            })
            .collect::<Vec<_>>()
            .into();
        Self {
            points,
            metadata,
            legend: vec![
                SeriesLabel {
                    series: Series::Long,
                    label: labels.buy.clone(),
                },
                SeriesLabel {
                    series: Series::Short,
                    label: labels.sell.clone(),
                },
            ]
            .into_boxed_slice(),
            volume_title: label_with_unit(labels.volume.as_str(), base.as_str()),
            base,
            labels,
            locale,
            revision,
            volume,
            time_labels,
            tooltips,
            zone,
        }
    }
}

impl ChartData for TakerVolumeChart {
    type Slice = [TakerVolumePoint];

    fn points(snapshot: &StatisticsPoints) -> Option<Arc<Self::Slice>> {
        match snapshot {
            StatisticsPoints::TakerVolume(points) => Some(points.clone()),
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
        (None, &self.volume)
    }

    fn legend(&self) -> &[SeriesLabel] {
        &self.legend
    }

    fn axis_titles(&self) -> (&str, &str) {
        ("", self.volume_title.as_str())
    }

    fn prepare(&self, layout: ChartLayout) -> Prepared {
        prepare_taker_volume(&self.points, &self.volume, &self.time_labels, layout)
    }

    fn hover_point(&self, index: usize) -> Option<usize> {
        (index < self.points.len()).then_some(index)
    }

    fn tooltip(&self, index: usize) -> Option<Tooltip> {
        self.tooltips.get(index).cloned()
    }

    /// Rewrites the labels in another display zone.
    fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        if self.zone == zone {
            return false;
        }
        let (points, metadata, base, labels, locale, revision) = (
            Arc::clone(&self.points),
            self.metadata,
            self.base.clone(),
            self.labels.clone(),
            self.locale,
            self.revision,
        );
        *self = Self::build(points, metadata, revision, base, labels, locale, zone);
        true
    }

    fn set_locale(&mut self, translate: &Translate) -> bool {
        if self.locale == translate.language() {
            return false;
        }
        let labels = Labels::resolve(translate);
        if labels == self.labels {
            self.locale = translate.language();
            return false;
        }
        let (points, metadata, base, locale, revision, zone) = (
            Arc::clone(&self.points),
            self.metadata,
            self.base.clone(),
            translate.language(),
            self.revision,
            self.zone,
        );
        *self = Self::build(points, metadata, revision, base, labels, locale, zone);
        true
    }

    fn set_snapshot(
        &mut self,
        revision: u64,
        metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool {
        if revision < self.revision
            || (revision == self.revision
                && Arc::ptr_eq(&self.points, &points)
                && metadata == self.metadata)
        {
            return false;
        }
        let (base, labels, locale, zone) = (
            self.base.clone(),
            self.labels.clone(),
            self.locale,
            self.zone,
        );
        *self = Self::build(points, metadata, revision, base, labels, locale, zone);
        true
    }
}
