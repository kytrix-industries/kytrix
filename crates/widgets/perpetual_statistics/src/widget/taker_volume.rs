use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

pub struct TakerVolumeWidgetSeries;

impl StatisticsMetric for TakerVolumeWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.taker_volume");
    const DISPLAY_NAME: &'static str = "widget.names.taker_volume";
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.taker_volume");
    const ICON: &'static str = "widgets/taker_volume.png";
    const METRIC: MetricKind = MetricKind::TakerVolume;
}

pub type TakerVolumeWidget = StatisticsWidget<TakerVolumeWidgetSeries>;
