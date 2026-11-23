use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

pub struct TopTraderAccountRatioWidgetSeries;

impl StatisticsMetric for TopTraderAccountRatioWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.top_trader_account_ratio");
    const DISPLAY_NAME: &'static str = "widget.names.top_trader_account_ratio";
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.top_trader_account_ratio");
    const ICON: &'static str = "widgets/top_trader_account_ratio.png";
    const METRIC: MetricKind = MetricKind::TopTraderAccountRatio;
}

pub type TopTraderAccountRatioWidget = StatisticsWidget<TopTraderAccountRatioWidgetSeries>;
