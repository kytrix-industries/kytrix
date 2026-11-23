use app_config::{
    market_participants::{
        origins::{PolymarketClobOrigin, PolymarketGammaOrigin, PolymarketWebsocketOrigin},
        settings::VenueSettings,
    },
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
        venues::shared::{self, VenueSelect},
    },
    search::FieldSearch,
};

const PAGE: &str = "market_participants.polymarket";

const GAMMA_FIELD: &str = "market_participants.polymarket.gamma";

const CLOB_FIELD: &str = "market_participants.polymarket.clob";

const WEBSOCKET_FIELD: &str = "market_participants.polymarket.websocket";

const SECTION_ID: &str = "market_participants.polymarket_0";

pub(crate) struct VenuePage {
    host: SettingsHost,
    selects: Vec<VenueSelect>,
}

impl VenuePage {
    pub(crate) fn new(host: SettingsHost, _key: &str) -> Self {
        Self {
            host,
            selects: Vec::new(),
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
        let venue = VenueId::Polymarket;
        let mut rows: Vec<RowFn> = Vec::with_capacity(3);

        if cx.filter.admits_field(PAGE, GAMMA_FIELD) {
            let label = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.gamma_api_endpoint"
            );
            let description = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.gamma_api_endpoint_description"
            );
            let current = move |config: &Config| match config.venue(&venue) {
                Some(VenueSettings::Polymarket(settings)) => {
                    SmolStr::new_static(settings.gamma.as_str())
                }
                _ => SmolStr::new_static(PolymarketGammaOrigin::default().as_str()),
            };
            let select = shared::address_select(
                app,
                &self.host,
                window,
                shared::control_key(cx, GAMMA_FIELD),
                PolymarketGammaOrigin::ALL
                    .iter()
                    .map(PolymarketGammaOrigin::as_str),
                label.clone(),
                current,
                move |config, value| {
                    let Some(choice) = PolymarketGammaOrigin::from_url(value) else {
                        return;
                    };
                    config.set_polymarket_gamma(&venue, choice);
                },
            );
            rows.push(select.row(&theme, GAMMA_FIELD, label, description));
            self.selects.push(select);
        }

        if cx.filter.admits_field(PAGE, CLOB_FIELD) {
            let label = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.clob_api_endpoint"
            );
            let description = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.clob_api_endpoint_description"
            );
            let current = move |config: &Config| match config.venue(&venue) {
                Some(VenueSettings::Polymarket(settings)) => {
                    SmolStr::new_static(settings.clob.as_str())
                }
                _ => SmolStr::new_static(PolymarketClobOrigin::default().as_str()),
            };
            let select = shared::address_select(
                app,
                &self.host,
                window,
                shared::control_key(cx, CLOB_FIELD),
                PolymarketClobOrigin::ALL
                    .iter()
                    .map(PolymarketClobOrigin::as_str),
                label.clone(),
                current,
                move |config, value| {
                    let Some(choice) = PolymarketClobOrigin::from_url(value) else {
                        return;
                    };
                    config.set_polymarket_clob(&venue, choice);
                },
            );
            rows.push(select.row(&theme, CLOB_FIELD, label, description));
            self.selects.push(select);
        }

        if cx.filter.admits_field(PAGE, WEBSOCKET_FIELD) {
            let label = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.websocket_api_endpoint"
            );
            let description = i18n::t_smol!(
                translate,
                "settings_ui.market_participants.websocket_api_endpoint_description"
            );
            let current = move |config: &Config| match config.venue(&venue) {
                Some(VenueSettings::Polymarket(settings)) => {
                    SmolStr::new_static(settings.websocket.as_str())
                }
                _ => SmolStr::new_static(PolymarketWebsocketOrigin::default().as_str()),
            };
            let select = shared::address_select(
                app,
                &self.host,
                window,
                shared::control_key(cx, WEBSOCKET_FIELD),
                PolymarketWebsocketOrigin::ALL
                    .iter()
                    .map(PolymarketWebsocketOrigin::as_str),
                label.clone(),
                current,
                move |config, value| {
                    let Some(choice) = PolymarketWebsocketOrigin::from_url(value) else {
                        return;
                    };
                    config.set_polymarket_websocket(&venue, choice);
                },
            );
            rows.push(select.row(&theme, WEBSOCKET_FIELD, label, description));
            self.selects.push(select);
        }

        layout::section(app, window, parent, &theme, SECTION_ID, None, rows)
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        for select in &self.selects {
            select.sync(app)?;
        }

        Ok(())
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
    }
}

pub(crate) fn search_fields(translate: &Translate) -> Vec<FieldSearch> {
    let venue = VenueId::Polymarket;
    let page_id = shared::page_id(&venue);
    let gamma = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.gamma_api_endpoint"
    );
    let gamma_description = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.gamma_api_endpoint_description"
    );
    let clob = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.clob_api_endpoint"
    );
    let clob_description = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.clob_api_endpoint_description"
    );
    let websocket = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.websocket_api_endpoint"
    );
    let websocket_description = i18n::t_smol!(
        translate,
        "settings_ui.market_participants.websocket_api_endpoint_description"
    );

    vec![
        shared::origin_search_field(
            format_smolstr!("{page_id}.gamma"),
            gamma,
            gamma_description,
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
            format_smolstr!("{page_id}.clob"),
            clob,
            clob_description,
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
            format_smolstr!("{page_id}.websocket"),
            websocket,
            websocket_description,
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
