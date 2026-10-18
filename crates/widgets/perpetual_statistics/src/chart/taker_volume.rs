//! The taker-volume chart: buy and sell volume as paired bars.
//!
//! Both series are the same interval's volume split by which side crossed the
//! spread, so they share one axis and stand side by side rather than over each
//! other: a stacked bar would say the interval traded their sum, which is exactly
//! what the two sides are not.

use std::sync::Arc;

use gpui_kit::{App, Bounds, Pixels, Point, SharedString};
use i18n::t;
use market_participants_model::listing::rules::ListingPrecision;
use smol_str::SmolStr;

use super::{
    ChartData,
    axes::{AxisScale, ChartLayout, bar_domain},
    paint::{FrameKey, Hover, Prepared, Tooltip, TooltipEntry, prepare_taker_volume, time_labels},
    palette::{Series, SeriesLabel},
    text::{
        format_axis_time, format_compact, format_quantity, format_tooltip_time, label_with_unit,
        labelled_value,
    },
};
use crate::model::{StatisticsPoints, TakerVolumePoint, quantity_as_f64};

/// The names this chart shows, resolved from the active locale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Labels {
    pub buy: SmolStr,
    pub sell: SmolStr,
    pub volume: SmolStr,
}

impl Labels {
    /// Resolves the series names from the active locale.
    pub(super) fn resolve(cx: &App) -> Self {
        Self {
            buy: SmolStr::new(t!(cx, "widget.perpetual_statistics.buy_volume").as_ref()),
            sell: SmolStr::new(t!(cx, "widget.perpetual_statistics.sell_volume").as_ref()),
            volume: SmolStr::new(t!(cx, "widget.perpetual_statistics.volume").as_ref()),
        }
    }

    /// The names as a legend, in the order the series are drawn.
    fn legend(&self) -> Box<[SeriesLabel]> {
        vec![
            SeriesLabel {
                series: Series::Long,
                label: self.buy.clone(),
            },
            SeriesLabel {
                series: Series::Short,
                label: self.sell.clone(),
            },
        ]
        .into_boxed_slice()
    }
}

/// The taker-volume chart's state.
pub(crate) struct TakerVolumeChart {
    points: Arc<[TakerVolumePoint]>,
    metadata: Option<ListingPrecision>,
    /// The asset the volume is measured in, which a tooltip names.
    base: SmolStr,
    labels: Labels,
    revision: u64,
    volume: AxisScale,
    time_labels: Box<[SmolStr]>,
    tooltips: Arc<[Tooltip]>,
}

impl TakerVolumeChart {
    /// Builds an empty chart of `instrument`'s taker volume.
    pub(super) fn new(cx: &App, base: &str) -> Self {
        let labels = Labels::resolve(cx);
        Self::build(Arc::from(Vec::new()), None, 0, SmolStr::new(base), labels)
    }

    fn build(
        points: Arc<[TakerVolumePoint]>,
        metadata: Option<ListingPrecision>,
        revision: u64,
        base: SmolStr,
        labels: Labels,
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
        let time_labels = time_labels(points.iter().map(|point| point.timestamp), format_axis_time);
        let buy_label = label_with_unit(labels.buy.as_str(), base.as_str());
        let sell_label = label_with_unit(labels.sell.as_str(), base.as_str());
        let tooltips = points
            .iter()
            .map(|point| Tooltip {
                title: format_tooltip_time(point.timestamp),
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
            base,
            labels,
            revision,
            volume,
            time_labels,
            tooltips,
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

    fn frame_key(&self, bounds: Bounds<Pixels>) -> FrameKey {
        FrameKey::new(bounds, self.revision, self.metadata.is_some())
    }

    fn scales(&self) -> (Option<&AxisScale>, &AxisScale, Option<&AxisScale>) {
        (None, &self.volume, None)
    }

    fn legend(&self, _cx: &App) -> Box<[SeriesLabel]> {
        self.labels.legend()
    }

    fn axis_titles(&self, _cx: &App) -> (Option<SharedString>, SharedString) {
        (
            None,
            SharedString::new(label_with_unit(
                self.labels.volume.as_str(),
                self.base.as_str(),
            )),
        )
    }

    fn hover_point(
        &self,
        index: usize,
        cursor: Point<Pixels>,
        _layout: ChartLayout,
    ) -> Option<Hover> {
        let index = (index < self.points.len()).then_some(index)?;
        Some(Hover { index, cursor })
    }

    fn prepare(&self, layout: ChartLayout) -> Prepared {
        prepare_taker_volume(&self.points, &self.volume, &self.time_labels, layout)
    }

    fn tooltip(&self, index: usize) -> Option<Tooltip> {
        self.tooltips.get(index).cloned()
    }

    fn set_snapshot(
        &mut self,
        revision: u64,
        metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool {
        // A revision that moved is a new window even when its points are the same
        // values: the pane asked for this one, and it is the one it is now showing.
        if revision < self.revision
            || (revision == self.revision
                && Arc::ptr_eq(&self.points, &points)
                && metadata == self.metadata)
        {
            return false;
        }
        let base = self.base.clone();
        let labels = self.labels.clone();
        *self = Self::build(points, metadata, revision, base, labels);
        true
    }
}
