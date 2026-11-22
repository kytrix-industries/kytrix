//! HyperCore's page: one address of each kind, and the API Agent's credentials.

use app_config::{
    market_participants::{
        origins::{HyperliquidHttpOrigin, HyperliquidWebsocketOrigin},
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
        venues::{
            credentials::{CredentialsArea, CredentialsKind},
            shared::{self, OriginKind, VenueSelect},
        },
    },
    search::FieldSearch,
};

/// The id of this page's settings.
const PAGE: &str = "market_participants.hyperliquid";

/// The HTTP address row, as the search filter and the tests name it.
const HTTP_FIELD: &str = "market_participants.hyperliquid.http";

/// The WebSocket address row, as the search filter and the tests name it.
const WEBSOCKET_FIELD: &str = "market_participants.hyperliquid.websocket";

/// The id of the card this page's rows are drawn in.
const SECTION_ID: &str = "market_participants.hyperliquid_0";

/// The HyperCore page.
pub(crate) struct VenuePage {
    host: SettingsHost,
    selects: Vec<VenueSelect>,
    credentials: CredentialsArea,
}

impl VenuePage {
    /// The page over the injected collaborators, keyed by the settings page instance it belongs
    /// to.
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
        let venue = VenueId::Hyperliquid;
        let mut rows: Vec<RowFn> = Vec::with_capacity(3);

        if cx.filter.admits_field(PAGE, HTTP_FIELD) {
            let label = i18n::t_smol!(translate, OriginKind::Http.label_key());
            let description = i18n::t_smol!(translate, OriginKind::Http.description_key());
            let current = move |config: &Config| match config.venue(&venue) {
                Some(VenueSettings::Hyperliquid(settings)) => {
                    SmolStr::new_static(settings.http.as_str())
                }
                _ => SmolStr::new_static(HyperliquidHttpOrigin::default().as_str()),
            };
            let select = shared::address_select(
                app,
                &self.host,
                window,
                shared::control_key(cx, HTTP_FIELD),
                HyperliquidHttpOrigin::ALL
                    .iter()
                    .map(HyperliquidHttpOrigin::as_str),
                label.clone(),
                current,
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
            rows.push(select.row(&theme, HTTP_FIELD, label, description));
            self.selects.push(select);
        }

        if cx.filter.admits_field(PAGE, WEBSOCKET_FIELD) {
            let label = i18n::t_smol!(translate, OriginKind::Websocket.label_key());
            let description = i18n::t_smol!(translate, OriginKind::Websocket.description_key());
            let current = move |config: &Config| match config.venue(&venue) {
                Some(VenueSettings::Hyperliquid(settings)) => {
                    SmolStr::new_static(settings.websocket.as_str())
                }
                _ => SmolStr::new_static(HyperliquidWebsocketOrigin::default().as_str()),
            };
            let select = shared::address_select(
                app,
                &self.host,
                window,
                shared::control_key(cx, WEBSOCKET_FIELD),
                HyperliquidWebsocketOrigin::ALL
                    .iter()
                    .map(HyperliquidWebsocketOrigin::as_str),
                label.clone(),
                current,
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
            rows.push(select.row(&theme, WEBSOCKET_FIELD, label, description));
            self.selects.push(select);
        }

        let kind = CredentialsKind::Hyperliquid;
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
        consumed |= self.credentials.dispatch(app, event);

        consumed
    }

    fn release(&mut self, app: &mut App) {
        for select in self.selects.drain(..) {
            select.release(app);
        }
        self.credentials.release(app);
    }
}

/// The search rows of the settings this page draws.
pub(crate) fn search_fields(translate: &Translate) -> Vec<FieldSearch> {
    let venue = VenueId::Hyperliquid;
    let page_id = shared::page_id(&venue);
    let http_label = i18n::t_smol!(translate, OriginKind::Http.label_key());
    let http_description = i18n::t_smol!(translate, OriginKind::Http.description_key());
    let websocket_label = i18n::t_smol!(translate, OriginKind::Websocket.label_key());
    let websocket_description = i18n::t_smol!(translate, OriginKind::Websocket.description_key());

    vec![
        shared::origin_search_field(
            format_smolstr!("{page_id}.http"),
            http_label,
            http_description,
            &[
                "market_participants",
                "hyperliquid",
                "http",
                "origin",
                "地址",
            ],
        ),
        shared::origin_search_field(
            format_smolstr!("{page_id}.websocket"),
            websocket_label,
            websocket_description,
            &[
                "market_participants",
                "hyperliquid",
                "websocket",
                "origin",
                "地址",
            ],
        ),
        CredentialsKind::Hyperliquid.search_field(translate),
    ]
}
