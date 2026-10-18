//! the long/short account-ratio pane.

use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

/// The perpetual series this pane plots.
pub struct LongShortAccountRatioWidgetSeries;

impl StatisticsMetric for LongShortAccountRatioWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.market.long_short_account_ratio");
    const DISPLAY_NAME: &'static str = "widget.names.long_short_account_ratio";
    const DESCRIPTION: &'static str = "Perpetual long and short account ratio";
    const ICON: &'static str = "widgets/long_short_account_ratio.png";
    const METRIC: MetricKind = MetricKind::LongShortAccountRatio;
}

/// Perpetual long and short account ratio.
pub type LongShortAccountRatioWidget = StatisticsWidget<LongShortAccountRatioWidgetSeries>;
