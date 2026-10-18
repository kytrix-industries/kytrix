//! the taker-volume pane.

use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

/// The perpetual series this pane plots.
pub struct TakerVolumeWidgetSeries;

impl StatisticsMetric for TakerVolumeWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.market.taker_volume");
    const DISPLAY_NAME: &'static str = "widget.names.taker_volume";
    const DESCRIPTION: &'static str = "Perpetual taker buy and sell volume";
    const ICON: &'static str = "widgets/taker_volume.png";
    const METRIC: MetricKind = MetricKind::TakerVolume;
}

/// Perpetual taker buy and sell volume.
pub type TakerVolumeWidget = StatisticsWidget<TakerVolumeWidgetSeries>;
