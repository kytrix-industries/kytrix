use app_config::{
    market_participants::{origins::OkxRegion, settings::VenueSettings},
    settings::Config,
};
use i18n::translate::Translate;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};

use crate::{
    host::SettingsHost,
    layout::{self, RowFn},
    pages::{
        page::{PageBuild, PageView},
        venues::{
            credentials::{CredentialsArea, CredentialsKind},
            shared::{self, VenueSelect},
        },
    },
    search::FieldSearch,
};

const PAGE: &str = "market_participants.okx";

const REGION_FIELD: &str = "market_participants.okx.region";

const SECTION_ID: &str = "market_participants.okx_0";

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

pub(crate) struct VenuePage {
    host: SettingsHost,
    selects: Vec<VenueSelect>,
    credentials: CredentialsArea,
}

impl VenuePage {
    pub(crate) fn new(host: SettingsHost, _key: &str) -> Self {
        Self {
            host,
            selects: Vec::new(),
            credentials: CredentialsArea::new(),
        }
    }
}

impl PageView for VenuePage {
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
        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let venue = VenueId::Okx;
        let mut rows: Vec<RowFn> = Vec::with_capacity(2);

        if cx.filter.admits_field(PAGE, REGION_FIELD) {
            let label = i18n::t_smol!(translate, "settings_ui.market_participants.service_region");
            let description = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.service_region_description"
            );
            let choices = REGION_OPTIONS
                .into_iter()
                .map(|(region, label_key)| {
                    (
                        SmolStr::new_static(region.as_str()),
                        i18n::t_smol!(translate, label_key),
                    )
                })
                .collect::<Vec<_>>();
            let current = move |config: &Config| {
                let region = match config.venue(&venue) {
                    Some(VenueSettings::Okx(settings)) => settings.region,
                    _ => OkxRegion::default(),
                };
                SmolStr::new_static(region.as_str())
            };
            let select = VenueSelect::new(
                app,
                &self.host,
                window,
                shared::control_key(cx, REGION_FIELD),
                current,
                choices,
                label.clone(),
                move |config, value| {
                    let Some(region) = OkxRegion::ALL
                        .iter()
                        .find(|region| region.as_str() == value)
                    else {
                        return;
                    };
                    config.set_okx_region(&venue, *region);
                },
            );
            rows.push(select.row(&theme, REGION_FIELD, label, description));
            self.selects.push(select);
        }

        let kind = CredentialsKind::Okx;
        if cx.filter.admits_field(PAGE, kind.row_id()) {
            let base_key = shared::control_key(cx, kind.row_id());
            rows.push(
                self.credentials
                    .row(app, window, &self.host, base_key, kind, &theme)?,
            );
        }

        layout::section(app, window, parent, &theme, SECTION_ID, None, rows)
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        for select in &self.selects {
            select.sync(app)?;
        }
        self.credentials.sync(app)
    }

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        let mut consumed = false;
        for select in &self.selects {
            consumed |= select.dispatch(app, event);
        }

        consumed
    }

    fn release(&mut self, app: &mut App) {
        for select in self.selects.drain(..) {
            select.release(app);
        }
        self.credentials.release(app);
    }
}

pub(crate) fn search_fields(translate: &Translate) -> Vec<FieldSearch> {
    let venue = VenueId::Okx;
    let page_id = shared::page_id(&venue);
    let label = i18n::t_smol!(translate, "settings_ui.market_participants.service_region");
    let description = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.service_region_description"
    );

    vec![
        shared::origin_search_field(
            format_smolstr!("{page_id}.region"),
            label,
            description,
            &[
                "market_participants",
                "okx",
                "region",
                "origin",
                "服务地区",
                "市场参与者",
            ],
        ),
        CredentialsKind::Okx.search_field(translate),
    ]
}
