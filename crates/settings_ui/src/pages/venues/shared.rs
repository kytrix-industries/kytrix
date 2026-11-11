//! The parts every venue page shares: the published origins and their search rows.
//!
//! A venue page is native UI written for the settings that venue's API has, so this module holds
//! only the conversions that are the same for every venue: how one Binance market's origin pair is
//! updated, how a published address is offered as a choice, and how a row is described to the
//! search. The layout of a page stays in the page that draws it.

use app_config::{
    market_participants::{
        origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
        settings::{BinanceOrigins, VenueSettings},
    },
    settings::Config,
};
use base_ui::select::Select;
use gpui_kit::SharedString;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    search::{FieldSearch, words},
};

/// Which of one scope's two addresses a row writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OriginKind {
    Http,
    Websocket,
}

impl OriginKind {
    /// The name the row is drawn under.
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Http => "settings_ui.market_participants.http_api_endpoint",
            Self::Websocket => "settings_ui.market_participants.websocket_api_endpoint",
        }
    }

    /// The line under the row that says what the endpoint is for.
    pub(crate) const fn description_key(self) -> &'static str {
        match self {
            Self::Http => "settings_ui.market_participants.http_api_endpoint_description",
            Self::Websocket => "settings_ui.market_participants.websocket_api_endpoint_description",
        }
    }
}

/// The address choices one Binance market holds, or nothing while the venue is not configured as
/// Binance.
pub(crate) fn binance_origins(
    config: &Config,
    venue: &VenueId,
    market: BinanceMarket,
) -> Option<BinanceOrigins> {
    match config.venue(venue) {
        Some(VenueSettings::Binance(settings)) => settings.market_origins(market),
        _ => None,
    }
}

/// Writes one Binance market's choice of one of its two addresses.
///
/// The pair is written as a whole — the other role is carried over unchanged — because a market is
/// reached at one pair of addresses and not at two values of different shapes. A value no table
/// publishes writes nothing: the choices are the venue's own, and a control that offered one this
/// build does not know is not a control it drew.
pub(crate) fn write_binance_origin(
    config: &mut Config,
    venue: &VenueId,
    market: BinanceMarket,
    kind: OriginKind,
    value: &str,
) {
    let Some(mut origins) = binance_origins(config, venue, market) else {
        return;
    };

    match kind {
        OriginKind::Http => {
            let Some(choice) = BinanceHttpOrigin::for_market(market)
                .iter()
                .find(|choice| choice.as_str() == value)
            else {
                return;
            };
            origins.http = *choice;
        }
        OriginKind::Websocket => {
            let Some(choice) = BinanceWebsocketOrigin::for_market(market)
                .iter()
                .find(|choice| choice.as_str() == value)
            else {
                return;
            };
            origins.websocket = *choice;
        }
    }

    config.set_binance_market_origins(venue, market, &origins);
}

/// A select over the published addresses of one origin row.
pub(crate) fn address_select(
    args: &mut PageArgs<'_, '_>,
    field_id: &str,
    selected: &str,
    choices: impl IntoIterator<Item = &'static str>,
    label: SharedString,
    apply: impl Fn(&mut Config, &str) + 'static,
) -> Select {
    let choices = choices.into_iter().map(|address| {
        (
            SharedString::new_static(address),
            SharedString::new_static(address),
        )
    });

    layout::choice_select(
        layout::element_id("settings-origin", field_id),
        Some(SharedString::from(selected)),
        choices,
        label,
        None,
        false,
        layout::on_select(args.handle, apply),
    )
}

/// The search row of one origin.
pub(crate) fn origin_search_field(
    id: impl Into<SharedString>,
    label: &str,
    description: &str,
    keywords: &[&str],
) -> FieldSearch {
    let id = id.into();
    let mut parts = Vec::with_capacity(keywords.len() + 3);
    parts.push(id.as_ref());
    parts.push(label);
    parts.push(description);
    parts.extend_from_slice(keywords);

    FieldSearch {
        text: words(&parts),
        id,
    }
}

/// The page id one venue's settings are drawn under.
pub(crate) fn page_id(venue: &VenueId) -> SharedString {
    SharedString::from(format!("market_participants.{}", venue.as_str()))
}
