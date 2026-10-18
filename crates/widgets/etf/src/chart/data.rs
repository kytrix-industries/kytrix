//! What one ETF chart is showing, prepared once per snapshot.
//!
//! A chart is the same drawing whatever the user picked; what changes is *what it
//! says*. So the picking — which asset, how much history, which fund — is resolved
//! here, at the boundary where a snapshot arrives, and everything a frame draws is
//! derived from that resolution: the points, their labels, the axis scales and the
//! tooltips.
//!
//! The drawing code therefore never formats a number and never asks the translator
//! for a string. That is what makes a frame cheap, and it is also what makes a
//! language change visible: the locale a chart was prepared in is kept, so the pane
//! can tell that the labels on screen are the ones of the language it just left.
//!
//! Net assets are read from [`EtfDataset::days_for_window`]'s rows, but they are
//! *accumulated* over the whole history by the model: the total is a level the
//! funds hold, not a sum over the window a chart happens to show.

use std::sync::Arc;

use gpui_kit::{App, SharedString};
use i18n::{language::codes::LanguageCode, t};
use smol_str::SmolStr;

use super::{
    axes::{AxisScale, flow_domain, net_assets_domain},
    text::{format_axis_value, format_date, format_header_date, format_money, format_signed_money},
};
use crate::model::{EtfDataset, EtfFundSelection, EtfWindow};

/// One trading day, as the frame draws it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedPoint {
    /// The day, written in full, which is the tooltip's title and the pane's own
    /// date field.
    pub header_date: SharedString,
    /// The day, written for the time axis.
    pub date_label: SmolStr,
    /// What the funds moved that day, in millions of dollars.
    pub flow: f64,
    /// The flow, as a person reads an amount of money.
    pub flow_label: SharedString,
    /// The flow, named as an inflow or an outflow.
    pub flow_caption: SharedString,
    /// The flow, in a tooltip: what the day's reading is called, written with the
    /// separator its amount follows.
    ///
    /// The amount is [`PreparedPoint::flow_label`] rather than part of this string:
    /// a tooltip draws the day's direction in its own text colour and the amount in
    /// the colour the day went, so the reading is kept as the two runs it is drawn
    /// from.
    pub flow_tooltip_caption: SharedString,
    /// What the funds hold after that day.
    pub net_assets: f64,
    pub net_assets_label: SharedString,
    pub net_assets_tooltip: SharedString,
}

/// Everything one chart draws, and everything it says about it.
pub(crate) struct PreparedData {
    /// The history this was read from, kept so a caller can tell one dataset from
    /// another by pointer.
    pub source: Arc<EtfDataset>,
    pub selection: EtfFundSelection,
    pub window: EtfWindow,
    /// The language the labels were written in.
    pub locale: LanguageCode,
    /// What each axis counts, and the unit its numbers are read in. The numbers on
    /// an axis are bare, so the title is the only thing that says what they are.
    pub left_axis_title: SharedString,
    pub right_axis_title: SharedString,
    pub points: Box<[PreparedPoint]>,
    pub flow: AxisScale,
    /// The net-asset axis, which only a chart of every fund has: a single fund has
    /// a flow but no assets of its own to report.
    pub net_assets: Option<AxisScale>,
    pub show_net_assets: bool,
}

/// The two readings and the day they belong to, as the pane's own row shows them.
///
/// The flow arrives both as the text to draw and as the number it was written from,
/// because the row colours the reading by the direction it went while the text is
/// already the reader's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Metrics {
    pub date: SharedString,
    pub caption: SharedString,
    pub flow: SharedString,
    pub flow_value: f64,
    pub net_assets: Option<SharedString>,
}

impl PreparedData {
    /// Resolves one view of one history.
    pub(crate) fn new(
        cx: &App,
        source: Arc<EtfDataset>,
        selection: EtfFundSelection,
        window: EtfWindow,
    ) -> Self {
        let locale = *cx.global::<i18n::language::codes::LanguageCode>();
        let left_axis_title = t!(cx, "widget.etf.flow_axis");
        let right_axis_title = t!(cx, "widget.etf.net_assets_axis");
        // A fund the asset does not have is a view of no data rather than of the
        // total: showing the total under a fund's name would report a number the
        // user did not ask for and cannot tell apart from the one they did.
        let selection_valid = match &selection {
            EtfFundSelection::All => true,
            EtfFundSelection::Named(name) => source.fund_index(name.as_str()).is_some(),
        };
        let visible = if selection_valid {
            source.days_for_window(window)
        } else {
            &[]
        };
        let flow_name = match &selection {
            EtfFundSelection::All => SharedString::new_static("ETF"),
            EtfFundSelection::Named(name) => SharedString::new(name.as_str()),
        };
        let show_net_assets = matches!(&selection, EtfFundSelection::All);

        let mut points = Vec::with_capacity(visible.len());
        for day in visible {
            let flow = source.flow_for(day, &selection).unwrap_or(f64::NAN);
            let flow_label = format_signed_money(flow);
            let net_assets_label = if show_net_assets {
                format_money(day.net_assets)
            } else {
                SmolStr::new_static("--")
            };
            let net_assets_tooltip = if show_net_assets {
                t!(
                    cx,
                    "widget.etf.value",
                    label = right_axis_title.as_ref(),
                    value = net_assets_label.as_str()
                )
            } else {
                SharedString::new_static("--")
            };
            let full_date = SharedString::new(format_header_date(day.date).as_str());
            points.push(PreparedPoint {
                header_date: full_date.clone(),
                date_label: format_date(day.date),
                flow,
                flow_caption: flow_caption(cx, flow, flow_name.as_str()),
                // The caption is the label half of the reading the pane writes
                // everywhere else: the template that says how a label and its amount
                // are read together, with the amount left off.
                flow_tooltip_caption: t!(
                    cx,
                    "widget.etf.value",
                    label = day_caption(cx, flow).as_str(),
                    value = ""
                ),
                flow_label: SharedString::new(flow_label.as_str()),
                net_assets: day.net_assets,
                net_assets_label: SharedString::new(net_assets_label.as_str()),
                net_assets_tooltip,
            });
        }
        let flow = AxisScale::new(
            flow_domain(points.iter().map(|point| point.flow)),
            format_axis_value,
        );
        let net_assets = show_net_assets.then(|| {
            AxisScale::new(
                net_assets_domain(points.iter().map(|point| point.net_assets)),
                format_axis_value,
            )
        });
        Self {
            source,
            selection,
            window,
            locale,
            left_axis_title,
            right_axis_title,
            points: points.into_boxed_slice(),
            flow,
            net_assets,
            show_net_assets,
        }
    }

    /// The point under a hover, or the last one when the pointer is elsewhere.
    pub(crate) fn current(&self, hover: Option<usize>) -> Option<&PreparedPoint> {
        hover
            .and_then(|index| self.points.get(index))
            .or_else(|| self.points.last())
    }

    /// What the pane's own row shows for the point under the pointer.
    ///
    /// It is the same reading the tooltip carries: the day, the amount, and which way
    /// the day went. Only the caption differs, because the two name different things
    /// — the row labels the amount with the series it belongs to, the tooltip labels
    /// it with the day it is about — while the amount itself is spelled the same in
    /// both, which is what keeps one value from reading as two.
    pub(crate) fn metrics(&self, hover: Option<usize>) -> Option<Metrics> {
        let point = self.current(hover)?;
        Some(Metrics {
            date: point.header_date.clone(),
            caption: point.flow_caption.clone(),
            flow: point.flow_label.clone(),
            flow_value: point.flow,
            net_assets: self.show_net_assets.then(|| point.net_assets_label.clone()),
        })
    }
}

/// What one day's flow is called: a direction when it has one, and its name when
/// it does not.
fn flow_caption(cx: &App, value: f64, name: &str) -> SharedString {
    if value > 0.0 {
        t!(cx, "widget.etf.net_inflow", name = name)
    } else if value < 0.0 {
        t!(cx, "widget.etf.net_outflow", name = name)
    } else {
        t!(cx, "widget.etf.net_flow", name = name)
    }
}

/// What one day's flow is called in a tooltip: the direction the day went, named for
/// the day rather than for the series.
///
/// The row above the chart is where a reading is labelled with the series it belongs
/// to, because the row is what a reader compares against the pane's own controls; a
/// tooltip has the date on the line above, so the only thing left for it to name is
/// which way that day went.
fn day_caption(cx: &App, value: f64) -> SharedString {
    if value > 0.0 {
        t!(cx, "widget.etf.day_net_inflow")
    } else if value < 0.0 {
        t!(cx, "widget.etf.day_net_outflow")
    } else {
        t!(cx, "widget.etf.day_net_flow")
    }
}
