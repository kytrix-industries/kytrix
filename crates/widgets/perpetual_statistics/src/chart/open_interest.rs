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

#[derive(Clone, Debug, PartialEq, Eq)]
struct Units {
    base: SmolStr,
    quote: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Labels {
    amount: SmolStr,
    value: SmolStr,
}

impl Labels {
    fn resolve(translate: &Translate) -> Self {
        Self {
            amount: t_smol!(translate, "widget.perpetual_statistics.amount"),
            value: t_smol!(translate, "widget.perpetual_statistics.value"),
        }
    }
}

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
    zone: UtcOffset,
}

impl OpenInterestChart {
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
