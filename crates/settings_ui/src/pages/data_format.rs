use app_config::time_zone::DataTimeZone;
use base_ui::kit::select::Select;
use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};

use crate::{
    host::SettingsHost,
    layout,
    pages::page::{PageBuild, PageView},
    search::{FieldSearch, PageSearch, words},
    session::SettingsChange,
};

const PAGE: &str = "data-format";

pub(crate) struct DataFormatPage {
    host: SettingsHost,
    offset: Option<Select>,
    applied: Option<DataTimeZone>,
}

impl DataFormatPage {
    pub(crate) fn new(host: SettingsHost, _key: &str) -> Self {
        Self {
            host,
            offset: None,
            applied: None,
        }
    }
}

impl PageView for DataFormatPage {
    fn page_id(&self) -> &'static str {
        PAGE
    }

    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError> {
        if !cx.filter.admits_field(PAGE, "data-format.offset") {
            return Ok(());
        }

        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let config = self.host.config(app, |config| config.clone());
        let label = i18n::t_smol!(translate, "settings_ui.data_format.time_zone");
        let description = i18n::t_smol!(translate, "settings_ui.data_format.description");
        let selected = format_smolstr!("{}", config.data_time_zone.hours());
        let choices = DataTimeZone::ALL
            .into_iter()
            .map(|zone| (format_smolstr!("{}", zone.hours()), zone.label()))
            .collect::<Vec<_>>();
        let host = self.host.clone();
        let select = layout::choice_select(
            app,
            &self.host,
            cx.unique_key("offset"),
            selected.as_str(),
            choices,
            label.clone(),
            move |app, value| {
                let Ok(hours) = value.parse::<i8>() else {
                    return;
                };
                let Some(zone) = DataTimeZone::from_hours(hours) else {
                    return;
                };
                let _ = host.write(
                    app,
                    window,
                    SettingsChange::edit(move |config| config.data_time_zone = zone),
                );
            },
        );
        let control = select.clone();
        let row = layout::row(
            &theme,
            "data-format.offset",
            label,
            description,
            move |app, window, parent| {
                control.mount(app, window, parent)?;
                Ok(())
            },
        );

        layout::section(
            app,
            window,
            parent,
            &theme,
            "data-format_0",
            None,
            vec![row],
        )?;
        self.offset = Some(select);
        self.applied = Some(config.data_time_zone);

        Ok(())
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        let zone = self.host.config(app, |config| config.data_time_zone);
        if self.applied == Some(zone) {
            return Ok(());
        }

        if let Some(select) = &self.offset {
            let hours = format_smolstr!("{}", zone.hours());
            select.set_chosen(app, [hours.as_str()], false);
        }
        self.applied = Some(zone);

        Ok(())
    }

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        let mut consumed = false;
        if let Some(select) = &self.offset {
            consumed |= select.dispatch(app, event);
        }
        consumed
    }

    fn release(&mut self, app: &mut App) {
        if let Some(select) = self.offset.take() {
            select.release(app);
        }
        self.applied = None;
    }
}

pub(crate) fn search(translate: &Translate) -> PageSearch {
    let title = i18n::t_smol!(translate, "settings_ui.data_format.title");
    let label = i18n::t_smol!(translate, "settings_ui.data_format.time_zone");
    let description = i18n::t_smol!(translate, "settings_ui.data_format.description");

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
            id: SmolStr::new_static("data-format.offset"),
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
