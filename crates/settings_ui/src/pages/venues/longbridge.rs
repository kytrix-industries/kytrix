//! Longbridge's page: the service region the account belongs to, and its credentials.

use app_config::market_participants::{origins::LongbridgeRegion, settings::VenueSettings};
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    pages::venues::shared::{self, CredentialField},
    search::FieldSearch,
};

const REGION_OPTIONS: [(LongbridgeRegion, &str); 2] = [
    (
        LongbridgeRegion::Global,
        "settings_ui.market_participants.service_region_global",
    ),
    (
        LongbridgeRegion::China,
        "settings_ui.market_participants.service_region_china",
    ),
];

const CREDENTIAL_FIELDS: [CredentialField; 3] = [
    CredentialField::LongbridgeAppKey,
    CredentialField::LongbridgeAppSecret,
    CredentialField::LongbridgeAccessToken,
];

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let venue = VenueId::Longbridge;
    let page_id = shared::page_id(&venue);
    let mut groups = Vec::new();
    let mut region_rows = Vec::new();
    let mut credential_rows = Vec::new();

    let region_id = format!("{page_id}.region");
    if args.filter.admits_field(page_id.as_ref(), &region_id) {
        let label = t!(args.cx, "settings_ui.market_participants.service_region");
        let description = t!(
            args.cx,
            "settings_ui.market_participants.service_region_description"
        );
        let current = match args.config.venue(&venue) {
            Some(VenueSettings::Longbridge(settings)) => settings.region,
            _ => LongbridgeRegion::default(),
        };
        let choices = REGION_OPTIONS.into_iter().map(|(region, label_key)| {
            (
                SharedString::new_static(region.as_str()),
                t!(args.cx, label_key),
            )
        });
        let select = layout::choice_select(
            layout::element_id("settings-select", &region_id),
            Some(SharedString::new_static(current.as_str())),
            choices,
            label.clone(),
            None,
            false,
            layout::on_select(args.handle, move |config, value| {
                let Some(region) = LongbridgeRegion::ALL
                    .iter()
                    .find(|region| region.as_str() == value)
                else {
                    return;
                };
                config.set_longbridge_region(&venue, *region);
            }),
        );

        region_rows.push(layout::row(
            &region_id,
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

    if !region_rows.is_empty() {
        groups.push(layout::section(
            None,
            "market_participants.longbridge_0",
            region_rows,
            args.cx,
        ));
    }
    if !credential_rows.is_empty() {
        groups.push(layout::section(
            Some(t!(args.cx, "settings_ui.market_participants.credentials")),
            "market_participants.longbridge_1",
            credential_rows,
            args.cx,
        ));
    }

    layout::page(groups)
}

pub(crate) fn search_fields(cx: &App) -> Vec<FieldSearch> {
    let venue = VenueId::Longbridge;
    let page_id = shared::page_id(&venue);
    let label = t!(cx, "settings_ui.market_participants.service_region");
    let description = t!(
        cx,
        "settings_ui.market_participants.service_region_description"
    );
    let mut fields = Vec::with_capacity(4);

    fields.push(shared::origin_search_field(
        format!("{page_id}.region"),
        label.as_ref(),
        description.as_ref(),
        &[
            "market_participants",
            "longbridge",
            "region",
            "origin",
            "服务地区",
            "市场参与者",
        ],
    ));
    for field in CREDENTIAL_FIELDS {
        fields.push(shared::credential_search_field(venue, None, field, cx));
    }

    fields
}
