//! Polymarket's page: the event directory, the CLOB and the CLOB stream.

use app_config::market_participants::{
    origins::{PolymarketClobOrigin, PolymarketGammaOrigin, PolymarketWebsocketOrigin},
    settings::VenueSettings,
};
use gpui_kit::{AnyElement, App, IntoElement};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    pages::venues::shared,
    search::FieldSearch,
};

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let venue = VenueId::Polymarket;
    let page_id = shared::page_id(&venue);
    let mut rows = Vec::new();
    let settings = match args.config.venue(&venue) {
        Some(VenueSettings::Polymarket(settings)) => Some(settings),
        _ => None,
    };

    let gamma_id = format!("{page_id}.gamma");
    if args.filter.admits_field(page_id.as_ref(), &gamma_id) {
        let label = t!(
            args.cx,
            "settings_ui.market_participants.gamma_api_endpoint"
        );
        let description = t!(
            args.cx,
            "settings_ui.market_participants.gamma_api_endpoint_description"
        );
        let selected =
            settings.map_or_else(PolymarketGammaOrigin::default, |settings| settings.gamma);
        let select = shared::address_select(
            args,
            &gamma_id,
            selected.as_str(),
            PolymarketGammaOrigin::ALL
                .iter()
                .map(PolymarketGammaOrigin::as_str),
            label.clone(),
            move |config, value| {
                let Some(choice) = PolymarketGammaOrigin::from_url(value) else {
                    return;
                };
                config.set_polymarket_gamma(&venue, choice);
            },
        );

        rows.push(layout::row(
            &gamma_id,
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    let clob_id = format!("{page_id}.clob");
    if args.filter.admits_field(page_id.as_ref(), &clob_id) {
        let label = t!(args.cx, "settings_ui.market_participants.clob_api_endpoint");
        let description = t!(
            args.cx,
            "settings_ui.market_participants.clob_api_endpoint_description"
        );
        let selected =
            settings.map_or_else(PolymarketClobOrigin::default, |settings| settings.clob);
        let select = shared::address_select(
            args,
            &clob_id,
            selected.as_str(),
            PolymarketClobOrigin::ALL
                .iter()
                .map(PolymarketClobOrigin::as_str),
            label.clone(),
            move |config, value| {
                let Some(choice) = PolymarketClobOrigin::from_url(value) else {
                    return;
                };
                config.set_polymarket_clob(&venue, choice);
            },
        );

        rows.push(layout::row(
            &clob_id,
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    let websocket_id = format!("{page_id}.websocket");
    if args.filter.admits_field(page_id.as_ref(), &websocket_id) {
        let label = t!(
            args.cx,
            "settings_ui.market_participants.websocket_api_endpoint"
        );
        let description = t!(
            args.cx,
            "settings_ui.market_participants.websocket_api_endpoint_description"
        );
        let selected = settings.map_or_else(PolymarketWebsocketOrigin::default, |settings| {
            settings.websocket
        });
        let select = shared::address_select(
            args,
            &websocket_id,
            selected.as_str(),
            PolymarketWebsocketOrigin::ALL
                .iter()
                .map(PolymarketWebsocketOrigin::as_str),
            label.clone(),
            move |config, value| {
                let Some(choice) = PolymarketWebsocketOrigin::from_url(value) else {
                    return;
                };
                config.set_polymarket_websocket(&venue, choice);
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

    if rows.is_empty() {
        return layout::page(Vec::new());
    }

    layout::page(vec![layout::section(
        None,
        "market_participants.polymarket_0",
        rows,
        args.cx,
    )])
}

pub(crate) fn search_fields(cx: &App) -> Vec<FieldSearch> {
    let venue = VenueId::Polymarket;
    let page_id = shared::page_id(&venue);
    let gamma = t!(cx, "settings_ui.market_participants.gamma_api_endpoint");
    let gamma_description = t!(
        cx,
        "settings_ui.market_participants.gamma_api_endpoint_description"
    );
    let clob = t!(cx, "settings_ui.market_participants.clob_api_endpoint");
    let clob_description = t!(
        cx,
        "settings_ui.market_participants.clob_api_endpoint_description"
    );
    let websocket = t!(cx, "settings_ui.market_participants.websocket_api_endpoint");
    let websocket_description = t!(
        cx,
        "settings_ui.market_participants.websocket_api_endpoint_description"
    );

    vec![
        shared::origin_search_field(
            format!("{page_id}.gamma"),
            gamma.as_ref(),
            gamma_description.as_ref(),
            &[
                "market_participants",
                "polymarket",
                "gamma",
                "events",
                "origin",
                "地址",
            ],
        ),
        shared::origin_search_field(
            format!("{page_id}.clob"),
            clob.as_ref(),
            clob_description.as_ref(),
            &[
                "market_participants",
                "polymarket",
                "clob",
                "prices",
                "origin",
                "地址",
            ],
        ),
        shared::origin_search_field(
            format!("{page_id}.websocket"),
            websocket.as_ref(),
            websocket_description.as_ref(),
            &[
                "market_participants",
                "polymarket",
                "websocket",
                "origin",
                "地址",
            ],
        ),
    ]
}
