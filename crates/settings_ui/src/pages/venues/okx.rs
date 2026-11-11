//! OKX's page: the service region the account belongs to, and the credentials it authenticates with.

use app_config::market_participants::{origins::OkxRegion, settings::VenueSettings};
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    pages::venues::{
        credentials::{self, CredentialsKind},
        shared,
    },
    search::FieldSearch,
};

const REGION_OPTIONS: [(OkxRegion, &str); 3] = [
    (
        OkxRegion::Global,
        "settings_ui.market_participants.service_region_global",
    ),
    (
        OkxRegion::UnitedStatesAustralia,
        "settings_ui.market_participants.service_region_us_au",
    ),
    (
        OkxRegion::Europe,
        "settings_ui.market_participants.service_region_eu",
    ),
];

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let venue = VenueId::Okx;
    let page_id = shared::page_id(&venue);
    let mut rows = Vec::new();

    let region_id = format!("{page_id}.region");
    if args.filter.admits_field(page_id.as_ref(), &region_id) {
        let label = t!(args.cx, "settings_ui.market_participants.service_region");
        let description = t!(
            args.cx,
            "settings_ui.market_participants.service_region_description"
        );
        let current = match args.config.venue(&venue) {
            Some(VenueSettings::Okx(settings)) => settings.region,
            _ => OkxRegion::default(),
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
                let Some(region) = OkxRegion::ALL
                    .iter()
                    .find(|region| region.as_str() == value)
                else {
                    return;
                };
                config.set_okx_region(&venue, *region);
            }),
        );

        rows.push(layout::row(
            &region_id,
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    let credentials = CredentialsKind::Okx;
    if args
        .filter
        .admits_field(page_id.as_ref(), credentials.row_id())
    {
        rows.push(credentials::credentials_row(args, credentials));
    }

    if rows.is_empty() {
        return layout::page(Vec::new());
    }

    layout::page(vec![layout::section(
        None,
        "market_participants.okx_0",
        rows,
        args.cx,
    )])
}

pub(crate) fn search_fields(cx: &App) -> Vec<FieldSearch> {
    let venue = VenueId::Okx;
    let page_id = shared::page_id(&venue);
    let label = t!(cx, "settings_ui.market_participants.service_region");
    let description = t!(
        cx,
        "settings_ui.market_participants.service_region_description"
    );

    vec![
        shared::origin_search_field(
            format!("{page_id}.region"),
            label.as_ref(),
            description.as_ref(),
            &[
                "market_participants",
                "okx",
                "region",
                "origin",
                "服务地区",
                "市场参与者",
            ],
        ),
        CredentialsKind::Okx.search_field(cx),
    ]
}
