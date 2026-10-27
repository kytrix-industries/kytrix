//! the top-trader position-ratio pane.

use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

/// The perpetual series this pane plots.
pub struct TopTraderPositionRatioWidgetSeries;

impl StatisticsMetric for TopTraderPositionRatioWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.top_trader_position_ratio");
    const DISPLAY_NAME: &'static str = "widget.names.top_trader_position_ratio";
    const DESCRIPTION: &'static str = "Top traders' long and short position ratio";
    const ICON: &'static str = "widgets/top_trader_position_ratio.png";
    const METRIC: MetricKind = MetricKind::TopTraderPositionRatio;
}

/// Top traders' long and short position ratio.
pub type TopTraderPositionRatioWidget = StatisticsWidget<TopTraderPositionRatioWidgetSeries>;
