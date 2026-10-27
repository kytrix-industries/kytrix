//! the open-interest pane.

use widget::identity::WidgetId;

use super::display::{StatisticsMetric, StatisticsWidget};
use crate::model::MetricKind;

/// The perpetual series this pane plots.
pub struct OpenInterestWidgetSeries;

impl StatisticsMetric for OpenInterestWidgetSeries {
    const ID: WidgetId = WidgetId::new_static("kytrix.open_interest");
    const DISPLAY_NAME: &'static str = "widget.names.open_interest";
    const DESCRIPTION: &'static str = "Perpetual open interest, in the base and quote asset";
    const ICON: &'static str = "widgets/open_interest.png";
    const METRIC: MetricKind = MetricKind::OpenInterest;
}

/// Perpetual open interest, in the base and quote asset.
pub type OpenInterestWidget = StatisticsWidget<OpenInterestWidgetSeries>;
