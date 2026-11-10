//! The data formatting page: the time zone every widget displays data timestamps in.

use app_config::time_zone::DataTimeZone;
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;

use crate::{
    layout::{self, PageArgs},
    search::{FieldSearch, PageSearch, words},
};

/// The id of this page's setting.
const PAGE: &str = "data-format";

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    if !args.filter.admits_field(PAGE, "data-format.offset") {
        return layout::page(Vec::new());
    }

    let label = t!(args.cx, "settings_ui.data_format.time_zone");
    let description = t!(args.cx, "settings_ui.data_format.description");
    let selected = SharedString::from(args.config.data_time_zone.hours().to_string());
    let choices = DataTimeZone::ALL.into_iter().map(|zone| {
        (
            SharedString::from(zone.hours().to_string()),
            SharedString::from(zone.label().as_str()),
        )
    });
    let select = layout::choice_select(
        layout::element_id("settings-select", "data-format.offset"),
        Some(selected),
        choices,
        label.clone(),
        None,
        false,
        layout::on_select(args.handle, |config, value| {
            let Ok(hours) = value.parse::<i8>() else {
                return;
            };
            let Some(zone) = DataTimeZone::from_hours(hours) else {
                return;
            };
            config.data_time_zone = zone;
        }),
    );

    let row = layout::row(
        "data-format.offset",
        label,
        description,
        select.into_any_element(),
        layout::CONTROL_WIDTH,
        args.cx,
    );

    layout::page(vec![layout::section(
        None,
        "data-format_0",
        vec![row],
        args.cx,
    )])
}

pub(crate) fn search(cx: &App) -> PageSearch {
    let title = t!(cx, "settings_ui.data_format.title");
    let label = t!(cx, "settings_ui.data_format.time_zone");
    let description = t!(cx, "settings_ui.data_format.description");

    PageSearch {
        label: words(&[
            title.as_ref(),
            "data format",
            "data formatting",
            "data",
            "format",
            "时区",
            "时间",
            "数据格式化",
        ]),
        fields: vec![FieldSearch {
            id: "data-format.offset".into(),
            text: words(&[
                label.as_ref(),
                description.as_ref(),
                "time zone",
                "timezone",
                "utc",
                "offset",
                "时区",
                "时间",
            ]),
        }],
    }
}
