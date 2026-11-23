use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

pub struct LongShortAccountRatioWidgetSeries;

impl StatisticsMetric for LongShortAccountRatioWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.long_short_account_ratio");
    const DISPLAY_NAME: &'static str = "widget.names.long_short_account_ratio";
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.long_short_account_ratio");
    const ICON: &'static str = "widgets/long_short_account_ratio.png";
    const METRIC: MetricKind = MetricKind::LongShortAccountRatio;
}

pub type LongShortAccountRatioWidget = StatisticsWidget<LongShortAccountRatioWidgetSeries>;
