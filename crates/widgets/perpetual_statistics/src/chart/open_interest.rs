//! The open-interest chart: two lines over one time axis.
//!
//! The two series are the same open interest measured two ways — the size of the
//! position in the base asset, and what it was worth in the quote asset — so they are
//! drawn on their own axes rather than on a shared one. A shared axis would have to
//! compare a number of coins with a number of dollars, and the smaller series would be
//! flattened against the floor by the larger.

use std::sync::Arc;

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_model::listing::rules::ListingPrecision;
use smol_str::SmolStr;
use time::UtcOffset;

use super::{
    axes::{AxisScale, ChartLayout, line_domain},
    canvas::ChartData,
    paint::{Prepared, Tooltip, TooltipEntry, prepare_open_interest, time_labels},
    palette::{Series, SeriesLabel},
    text::{
        format_axis_time, format_compact, format_price, format_quantity, format_tooltip_time,
        label_with_unit, labelled_value,
    },
};
use crate::model::{OpenInterestPoint, StatisticsPoints, quantity_as_f64};

/// The asset each series is measured in, which a tooltip names.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Units {
    base: SmolStr,
    quote: SmolStr,
}

/// The names this chart shows, resolved from the active language.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Labels {
    amount: SmolStr,
    value: SmolStr,
}

impl Labels {
    /// Resolves the series names from the active language.
    fn resolve(translate: &Translate) -> Self {
        Self {
            amount: t_smol!(translate, "widget.perpetual_statistics.amount"),
            value: t_smol!(translate, "widget.perpetual_statistics.value"),
        }
    }
}

/// The open-interest chart's state.
pub(crate) struct OpenInterestChart {
    points: Arc<[OpenInterestPoint]>,
    metadata: Option<ListingPrecision>,
    units: Units,
    labels: Labels,
    locale: LanguageCode,
    revision: u64,
    amount: AxisScale,
    value: AxisScale,
    time_labels: Box<[SmolStr]>,
    tooltips: Arc<[Tooltip]>,
    legend: Box<[SeriesLabel]>,
    amount_title: SmolStr,
    value_title: SmolStr,
    /// The zone the axis labels and tooltips were written in.
    zone: UtcOffset,
}

impl OpenInterestChart {
    /// Builds an empty chart of the two series of `instrument`.
    pub(crate) fn new(translate: &Translate, base: &str, quote: &str, zone: UtcOffset) -> Self {
        Self::build(
            Arc::from(Vec::new()),
            None,
            0,
            Units {
                base: SmolStr::new(base),
                quote: SmolStr::new(quote),
            },
            Labels::resolve(translate),
            translate.language(),
            zone,
        )
    }

    fn build(
        points: Arc<[OpenInterestPoint]>,
        metadata: Option<ListingPrecision>,
        revision: u64,
        units: Units,
        labels: Labels,
        locale: LanguageCode,
        zone: UtcOffset,
    ) -> Self {
        let quantity_tick = metadata.map(|metadata| metadata.quantity_tick);
        let price_tick = metadata.map(|metadata| metadata.price_tick);
        let amount = AxisScale::new(
            line_domain(
                points
                    .iter()
                    .map(|point| quantity_as_f64(point.base_asset_volume)),
            ),
            |value, step| match quantity_tick {
                Some(tick) => format_quantity(value, tick),
                None => format_compact(value, step),
            },
        );
        let value = AxisScale::new(
            line_domain(
                points
                    .iter()
                    .filter_map(|point| point.quote_asset_volume)
                    .map(quantity_as_f64),
            ),
            |value, step| match price_tick {
                Some(tick) => format_price(value, tick),
                None => format_compact(value, step),
            },
        );
        let time_labels = time_labels(points.iter().map(|point| point.timestamp), |timestamp| {
            format_axis_time(timestamp, zone)
        });
        let amount_label = label_with_unit(labels.amount.as_str(), units.base.as_str());
        let value_label = label_with_unit(labels.value.as_str(), units.quote.as_str());
        // Neither line of this tooltip is read in a series colour. The two are one
        // instrument measured two ways, and each is already named by its own label with
        // the asset it is measured in, so a colour could only be a second, weaker way of
        // telling them apart — one that has to be read against the legend's swatches to
        // mean anything. Both lines read in the tooltip's own foreground, the colour the
        // time above them is written in.
        let tooltips = points
            .iter()
            .map(|point| Tooltip {
                title: format_tooltip_time(point.timestamp, zone),
                entries: vec![
                    TooltipEntry {
                        text: labelled_value(
                            amount_label.as_str(),
                            quantity_tick.map_or_else(
                                || format_compact(quantity_as_f64(point.base_asset_volume), 0.0),
                                |tick| {
                                    format_quantity(quantity_as_f64(point.base_asset_volume), tick)
                                },
                            ),
                        ),
                        color: None,
                    },
                    TooltipEntry {
                        // An interval the venue published no amount for has no line to
                        // name: saying so is what keeps a missing reading from reading as
                        // a value of zero.
                        text: labelled_value(
                            value_label.as_str(),
                            point.quote_asset_volume.map_or_else(
                                || SmolStr::new_static("--"),
                                |amount| match price_tick {
                                    Some(tick) => format_price(quantity_as_f64(amount), tick),
                                    None => format_compact(quantity_as_f64(amount), 0.0),
                                },
                            ),
                        ),
                        color: None,
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
                    series: Series::Amount,
                    label: labels.amount.clone(),
                },
                SeriesLabel {
                    series: Series::Value,
                    label: labels.value.clone(),
                },
            ]
            .into_boxed_slice(),
            amount_title: label_with_unit(labels.amount.as_str(), units.base.as_str()),
            value_title: label_with_unit(labels.value.as_str(), units.quote.as_str()),
            units,
            labels,
            locale,
            revision,
            amount,
            value,
            time_labels,
            tooltips,
            zone,
        }
    }

    /// The chart rebuilt with new names, in the language they were resolved in.
    fn with_labels(&mut self, translate: &Translate) -> bool {
        let labels = Labels::resolve(translate);
        if labels == self.labels {
            self.locale = translate.language();
            return false;
        }
        let (points, metadata, revision, units, locale, zone) = (
            Arc::clone(&self.points),
            self.metadata,
            self.revision,
            self.units.clone(),
            translate.language(),
            self.zone,
        );
        *self = Self::build(points, metadata, revision, units, labels, locale, zone);
        true
    }
}

impl ChartData for OpenInterestChart {
    type Slice = [OpenInterestPoint];

    fn points(snapshot: &StatisticsPoints) -> Option<Arc<Self::Slice>> {
        match snapshot {
            StatisticsPoints::OpenInterest(points) => Some(points.clone()),
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
        (Some(&self.amount), &self.value)
    }

    fn legend(&self) -> &[SeriesLabel] {
        &self.legend
    }

    fn axis_titles(&self) -> (&str, &str) {
        (self.amount_title.as_str(), self.value_title.as_str())
    }

    fn prepare(&self, layout: ChartLayout) -> Prepared {
        prepare_open_interest(
            &self.points,
            &self.amount,
            &self.value,
            &self.time_labels,
            layout,
        )
    }

    fn hover_point(&self, index: usize) -> Option<usize> {
        (index < self.points.len()).then_some(index)
    }

    fn tooltip(&self, index: usize) -> Option<Tooltip> {
        self.tooltips.get(index).cloned()
    }

    /// Rewrites the labels in another display zone.
    ///
    /// Returns whether anything moved: the same zone is the same labels, and the chart is
    /// left exactly as it was.
    fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        if self.zone == zone {
            return false;
        }
        let points = Arc::clone(&self.points);
        let metadata = self.metadata;
        let units = self.units.clone();
        let labels = self.labels.clone();
        let locale = self.locale;
        *self = Self::build(points, metadata, self.revision, units, labels, locale, zone);
        true
    }

    fn set_locale(&mut self, translate: &Translate) -> bool {
        if self.locale == translate.language() {
            return false;
        }
        self.with_labels(translate)
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
        *self = Self::build(
            points,
            metadata,
            revision,
            self.units.clone(),
            self.labels.clone(),
            self.locale,
            self.zone,
        );
        true
    }
}
