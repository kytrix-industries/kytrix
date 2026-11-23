use std::sync::Arc;

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::SmolStr;

use super::{
    axes::{AxisScale, flow_domain, net_assets_domain},
    text::{format_axis_value, format_date, format_header_date, format_money, format_signed_money},
};
use crate::model::{
    CryptocurrencyEtfDataset, CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedPoint {
    pub header_date: SmolStr,
    pub date_label: SmolStr,
    pub flow: f64,
    pub flow_label: SmolStr,
    pub flow_caption: SmolStr,
    pub flow_tooltip_caption: SmolStr,
    pub net_assets: f64,
    pub net_assets_label: SmolStr,
    pub net_assets_tooltip: SmolStr,
}

pub(crate) struct PreparedData {
    pub source: Arc<CryptocurrencyEtfDataset>,
    pub selection: CryptocurrencyEtfFundSelection,
    pub window: CryptocurrencyEtfWindow,
    pub locale: LanguageCode,
    pub left_axis_title: SmolStr,
    pub right_axis_title: SmolStr,
    pub points: Box<[PreparedPoint]>,
    pub flow: AxisScale,
    pub net_assets: Option<AxisScale>,
    pub show_net_assets: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Metrics {
    pub date: SmolStr,
    pub caption: SmolStr,
    pub flow: SmolStr,
    pub flow_value: f64,
    pub net_assets: Option<SmolStr>,
}

impl PreparedData {
    pub(crate) fn new(
        translate: &Translate,
        source: Arc<CryptocurrencyEtfDataset>,
        selection: CryptocurrencyEtfFundSelection,
        window: CryptocurrencyEtfWindow,
    ) -> Self {
        let locale = translate.language();
        let left_axis_title = t_smol!(translate, "widget.cryptocurrency_etf.flow_axis");
        let right_axis_title = t_smol!(translate, "widget.cryptocurrency_etf.net_assets_axis");
        let selection_valid = match &selection {
            CryptocurrencyEtfFundSelection::All => true,
            CryptocurrencyEtfFundSelection::Named(name) => {
                source.fund_index(name.as_str()).is_some()
            }
        };
        let visible = if selection_valid {
            source.days_for_window(window)
        } else {
            &[]
        };
        let flow_name = match &selection {
            CryptocurrencyEtfFundSelection::All => SmolStr::new_static("ETF"),
            CryptocurrencyEtfFundSelection::Named(name) => name.clone(),
        };
        let show_net_assets = matches!(&selection, CryptocurrencyEtfFundSelection::All);

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
                t_smol!(
                    translate,
                    "widget.cryptocurrency_etf.value",
                    label = right_axis_title.as_str(),
                    value = net_assets_label.as_str()
                )
            } else {
                SmolStr::new_static("--")
            };
            let full_date = format_header_date(day.date);
            points.push(PreparedPoint {
                header_date: full_date,
                date_label: format_date(day.date),
                flow,
                flow_caption: flow_caption(translate, flow, flow_name.as_str()),
                flow_tooltip_caption: t_smol!(
                    translate,
                    "widget.cryptocurrency_etf.value",
                    label = day_caption(translate, flow).as_str(),
                    value = ""
                ),
                flow_label,
                net_assets: day.net_assets,
                net_assets_label,
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

    pub(crate) fn current(&self, hover: Option<usize>) -> Option<&PreparedPoint> {
        hover
            .and_then(|index| self.points.get(index))
            .or_else(|| self.points.last())
    }

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

fn flow_caption(translate: &Translate, value: f64, name: &str) -> SmolStr {
    if value > 0.0 {
        t_smol!(
            translate,
            "widget.cryptocurrency_etf.net_inflow",
            name = name
        )
    } else if value < 0.0 {
        t_smol!(
            translate,
            "widget.cryptocurrency_etf.net_outflow",
            name = name
        )
    } else {
        t_smol!(translate, "widget.cryptocurrency_etf.net_flow", name = name)
    }
}

fn day_caption(translate: &Translate, value: f64) -> SmolStr {
    if value > 0.0 {
        t_smol!(translate, "widget.cryptocurrency_etf.day_net_inflow")
    } else if value < 0.0 {
        t_smol!(translate, "widget.cryptocurrency_etf.day_net_outflow")
    } else {
        t_smol!(translate, "widget.cryptocurrency_etf.day_net_flow")
    }
}
