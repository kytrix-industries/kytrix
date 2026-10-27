//! the top-trader account-ratio pane.

use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

/// The perpetual series this pane plots.
pub struct TopTraderAccountRatioWidgetSeries;

impl StatisticsMetric for TopTraderAccountRatioWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.top_trader_account_ratio");
    const DISPLAY_NAME: &'static str = "widget.names.top_trader_account_ratio";
    const DESCRIPTION: &'static str = "Top traders' long and short account ratio";
    const ICON: &'static str = "widgets/top_trader_account_ratio.png";
    const METRIC: MetricKind = MetricKind::TopTraderAccountRatio;
}

/// Top traders' long and short account ratio.
pub type TopTraderAccountRatioWidget = StatisticsWidget<TopTraderAccountRatioWidgetSeries>;
