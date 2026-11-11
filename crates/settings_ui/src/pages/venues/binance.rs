//! Binance's page: the two markets the venue publishes, each with its origins and credentials.

use app_config::market_participants::origins::{
    BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin,
};
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    pages::venues::{
        credentials::{self, CredentialsKind},
        shared::{self, OriginKind},
    },
    search::FieldSearch,
};

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let venue = VenueId::Binance;
    let page_id = shared::page_id(&venue);
    let mut groups = Vec::new();

    for (index, market) in BinanceMarket::ALL.into_iter().enumerate() {
        let scope = market.scope();
        let mut rows = Vec::new();
        let origins = shared::binance_origins(args.config, &venue, market);

        let http_id = format!("{page_id}.{scope}.http");
        if args.filter.admits_field(page_id.as_ref(), &http_id) {
            let label = t!(args.cx, OriginKind::Http.label_key());
            let description = t!(args.cx, OriginKind::Http.description_key());
            let selected = origins
                .as_ref()
                .map_or_else(|| market.http(), |origins| origins.http);

            let select = shared::address_select(
                args,
                &http_id,
                selected.as_str(),
                BinanceHttpOrigin::for_market(market)
                    .iter()
                    .map(BinanceHttpOrigin::as_str),
                label.clone(),
                move |config, value| {
                    shared::write_binance_origin(config, &venue, market, OriginKind::Http, value)
                },
            );

            rows.push(layout::row(
                &http_id,
                label,
                description,
                select.into_any_element(),
                layout::CONTROL_WIDTH,
                args.cx,
            ));
        }

        let websocket_id = format!("{page_id}.{scope}.websocket");
        if args.filter.admits_field(page_id.as_ref(), &websocket_id) {
            let label = t!(args.cx, OriginKind::Websocket.label_key());
            let description = t!(args.cx, OriginKind::Websocket.description_key());
            let selected = origins
                .as_ref()
                .map_or_else(|| market.websocket(), |origins| origins.websocket);

            let select = shared::address_select(
                args,
                &websocket_id,
                selected.as_str(),
                BinanceWebsocketOrigin::for_market(market)
                    .iter()
                    .map(BinanceWebsocketOrigin::as_str),
                label.clone(),
                move |config, value| {
                    shared::write_binance_origin(
                        config,
                        &venue,
                        market,
                        OriginKind::Websocket,
                        value,
                    )
                },
            );

            rows.push(layout::row(
                &websocket_id,
                label,
                description,
                select.into_any_element(),
                layout::CONTROL_WIDTH,
                args.cx,
            ));
        }

        let credentials = CredentialsKind::for_market(market);
        if args
            .filter
            .admits_field(page_id.as_ref(), credentials.row_id())
        {
            rows.push(credentials::credentials_row(args, credentials));
        }

        if rows.is_empty() {
            continue;
        }

        groups.push(layout::section(
            Some(market_title(market, args.cx)),
            &format!("market_participants.binance_{index}"),
            rows,
            args.cx,
        ));
    }

    layout::page(groups)
}

/// The name one Binance market is drawn under.
fn market_title(market: BinanceMarket, cx: &App) -> SharedString {
    match market {
        BinanceMarket::Spot => t!(cx, "settings_ui.market_participants.binance_spot"),
        BinanceMarket::UsdMPerp => t!(cx, "settings_ui.market_participants.binance_usd_m_perp"),
    }
}

pub(crate) fn search_fields(cx: &App) -> Vec<FieldSearch> {
    let venue = VenueId::Binance;
    let page_id = shared::page_id(&venue);
    let mut fields = Vec::with_capacity(BinanceMarket::ALL.len() * 3);

    for market in BinanceMarket::ALL {
        let scope = market.scope();
        let http_label = t!(cx, OriginKind::Http.label_key());
        let http_description = t!(cx, OriginKind::Http.description_key());
        let websocket_label = t!(cx, OriginKind::Websocket.label_key());
        let websocket_description = t!(cx, OriginKind::Websocket.description_key());

        fields.push(shared::origin_search_field(
            format!("{page_id}.{scope}.http"),
            http_label.as_ref(),
            http_description.as_ref(),
            &[venue.as_str(), scope, "http", "origin", "地址"],
        ));
        fields.push(shared::origin_search_field(
            format!("{page_id}.{scope}.websocket"),
            websocket_label.as_ref(),
            websocket_description.as_ref(),
            &[venue.as_str(), scope, "websocket", "origin", "地址"],
        ));
        fields.push(CredentialsKind::for_market(market).search_field(cx));
    }

    fields
}
