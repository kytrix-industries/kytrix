//! HyperCore's page: one address of each kind, and the API Agent's credentials.

use app_config::market_participants::{
    origins::{HyperliquidHttpOrigin, HyperliquidWebsocketOrigin},
    settings::VenueSettings,
};
use gpui_kit::{AnyElement, App, IntoElement};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    pages::venues::shared::{self, CredentialField, OriginKind},
    search::FieldSearch,
};

const CREDENTIAL_FIELDS: [CredentialField; 2] = [
    CredentialField::MainAccountAddress,
    CredentialField::ApiAgentPrivateKey,
];

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let venue = VenueId::Hyperliquid;
    let page_id = shared::page_id(&venue);
    let mut groups = Vec::new();
    let mut origin_rows = Vec::new();
    let mut credential_rows = Vec::new();

    let (http, websocket) = match args.config.venue(&venue) {
        Some(VenueSettings::Hyperliquid(settings)) => (settings.http, settings.websocket),
        _ => (
            HyperliquidHttpOrigin::default(),
            HyperliquidWebsocketOrigin::default(),
        ),
    };

    let http_id = format!("{page_id}.http");
    if args.filter.admits_field(page_id.as_ref(), &http_id) {
        let label = t!(args.cx, OriginKind::Http.label_key());
        let description = t!(args.cx, OriginKind::Http.description_key());
        let select = shared::address_select(
            args,
            &http_id,
            http.as_str(),
            HyperliquidHttpOrigin::ALL
                .iter()
                .map(HyperliquidHttpOrigin::as_str),
            label.clone(),
            move |config, value| {
                let Some(choice) = HyperliquidHttpOrigin::from_url(value) else {
                    return;
                };
                let Some(VenueSettings::Hyperliquid(settings)) = config.venue(&venue) else {
                    return;
                };
                let websocket = settings.websocket;
                config.set_hyperliquid_origins(&venue, choice, websocket);
            },
        );

        origin_rows.push(layout::row(
            &http_id,
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    let websocket_id = format!("{page_id}.websocket");
    if args.filter.admits_field(page_id.as_ref(), &websocket_id) {
        let label = t!(args.cx, OriginKind::Websocket.label_key());
        let description = t!(args.cx, OriginKind::Websocket.description_key());
        let select = shared::address_select(
            args,
            &websocket_id,
            websocket.as_str(),
            HyperliquidWebsocketOrigin::ALL
                .iter()
                .map(HyperliquidWebsocketOrigin::as_str),
            label.clone(),
            move |config, value| {
                let Some(choice) = HyperliquidWebsocketOrigin::from_url(value) else {
                    return;
                };
                let Some(VenueSettings::Hyperliquid(settings)) = config.venue(&venue) else {
                    return;
                };
                let http = settings.http;
                config.set_hyperliquid_origins(&venue, http, choice);
            },
        );

        origin_rows.push(layout::row(
            &websocket_id,
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    for field in CREDENTIAL_FIELDS {
        if args
            .filter
            .admits_field(page_id.as_ref(), field.field_id(&venue, None).as_ref())
        {
            credential_rows.push(shared::credential_row(args, venue, None, field));
        }
    }

    if !origin_rows.is_empty() {
        groups.push(layout::section(
            None,
            "market_participants.hyperliquid_0",
            origin_rows,
            args.cx,
        ));
    }
    if !credential_rows.is_empty() {
        groups.push(layout::section(
            Some(t!(args.cx, "settings_ui.market_participants.credentials")),
            "market_participants.hyperliquid_1",
            credential_rows,
            args.cx,
        ));
    }

    layout::page(groups)
}

pub(crate) fn search_fields(cx: &App) -> Vec<FieldSearch> {
    let venue = VenueId::Hyperliquid;
    let page_id = shared::page_id(&venue);
    let http_label = t!(cx, OriginKind::Http.label_key());
    let http_description = t!(cx, OriginKind::Http.description_key());
    let websocket_label = t!(cx, OriginKind::Websocket.label_key());
    let websocket_description = t!(cx, OriginKind::Websocket.description_key());
    let mut fields = Vec::with_capacity(4);

    fields.push(shared::origin_search_field(
        format!("{page_id}.http"),
        http_label.as_ref(),
        http_description.as_ref(),
        &[
            "market_participants",
            "hyperliquid",
            "http",
            "origin",
            "地址",
        ],
    ));
    fields.push(shared::origin_search_field(
        format!("{page_id}.websocket"),
        websocket_label.as_ref(),
        websocket_description.as_ref(),
        &[
            "market_participants",
            "hyperliquid",
            "websocket",
            "origin",
            "地址",
        ],
    ));
    for field in CREDENTIAL_FIELDS {
        fields.push(shared::credential_search_field(venue, None, field, cx));
    }

    fields
}
