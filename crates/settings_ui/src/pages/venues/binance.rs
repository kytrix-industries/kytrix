use app_config::{
    market_participants::origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
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
            credentials::{self, CredentialsArea, CredentialsKind},
            shared::{self, OriginKind, VenueSelect},
        },
    },
    search::FieldSearch,
};

const PAGE: &str = "market_participants.binance";

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
        let venue = VenueId::Binance;

        for market in BinanceMarket::ALL {
            let mut rows: Vec<RowFn> = Vec::with_capacity(3);

            let http_field = http_id(market);
            if cx.filter.admits_field(PAGE, http_field) {
                let label = i18n::t_smol!(translate, OriginKind::Http.label_key());
                let description = i18n::t_smol!(translate, OriginKind::Http.description_key());
                let current = move |config: &Config| {
                    shared::binance_origins(config, &venue, market).map_or_else(
                        || SmolStr::new_static(market.http().as_str()),
                        |origins| SmolStr::new_static(origins.http.as_str()),
                    )
                };
                let select = shared::address_select(
                    app,
                    &self.host,
                    window,
                    shared::control_key(cx, http_field),
                    BinanceHttpOrigin::for_market(market)
                        .iter()
                        .map(BinanceHttpOrigin::as_str),
                    label.clone(),
                    current,
                    move |config, value| {
                        shared::write_binance_origin(
                            config,
                            &venue,
                            market,
                            OriginKind::Http,
                            value,
                        );
                    },
                );
                rows.push(select.row(&theme, http_field, label, description));
                self.selects.push(select);
            }

            let websocket_field = websocket_id(market);
            if cx.filter.admits_field(PAGE, websocket_field) {
                let label = i18n::t_smol!(translate, OriginKind::Websocket.label_key());
                let description = i18n::t_smol!(translate, OriginKind::Websocket.description_key());
                let current = move |config: &Config| {
                    shared::binance_origins(config, &venue, market).map_or_else(
                        || SmolStr::new_static(market.websocket().as_str()),
                        |origins| SmolStr::new_static(origins.websocket.as_str()),
                    )
                };
                let select = shared::address_select(
                    app,
                    &self.host,
                    window,
                    shared::control_key(cx, websocket_field),
                    BinanceWebsocketOrigin::for_market(market)
                        .iter()
                        .map(BinanceWebsocketOrigin::as_str),
                    label.clone(),
                    current,
                    move |config, value| {
                        shared::write_binance_origin(
                            config,
                            &venue,
                            market,
                            OriginKind::Websocket,
                            value,
                        );
                    },
                );
                rows.push(select.row(&theme, websocket_field, label, description));
                self.selects.push(select);
            }

            let kind = CredentialsKind::for_market(market);
            if cx.filter.admits_field(PAGE, kind.row_id()) {
                let base_key = shared::control_key(cx, kind.row_id());
                rows.push(
                    self.credentials
                        .row(app, window, &self.host, base_key, kind, &theme)?,
                );
            }

            let title = i18n::t_smol!(translate, credentials::market_title_key(market));
            layout::section(
                app,
                window,
                parent,
                &theme,
                section_id(market),
                Some(title),
                rows,
            )?;
        }

        Ok(())
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
    let venue = VenueId::Binance;
    let page_id = shared::page_id(&venue);
    let mut fields = Vec::with_capacity(BinanceMarket::ALL.len() * 3);

    for market in BinanceMarket::ALL {
        let scope = market.scope();
        let http_label = i18n::t_smol!(translate, OriginKind::Http.label_key());
        let http_description = i18n::t_smol!(translate, OriginKind::Http.description_key());
        let websocket_label = i18n::t_smol!(translate, OriginKind::Websocket.label_key());
        let websocket_description =
            i18n::t_smol!(translate, OriginKind::Websocket.description_key());

        fields.push(shared::origin_search_field(
            format_smolstr!("{page_id}.{scope}.http"),
            http_label,
            http_description,
            &[venue.as_str(), scope, "http", "origin", "地址"],
        ));
        fields.push(shared::origin_search_field(
            format_smolstr!("{page_id}.{scope}.websocket"),
            websocket_label,
            websocket_description,
            &[venue.as_str(), scope, "websocket", "origin", "地址"],
        ));
        fields.push(CredentialsKind::for_market(market).search_field(translate));
    }

    fields
}

const fn http_id(market: BinanceMarket) -> &'static str {
    match market {
        BinanceMarket::Spot => "market_participants.binance.spot.http",
        BinanceMarket::UsdMPerp => "market_participants.binance.usd-m-perp.http",
    }
}

const fn websocket_id(market: BinanceMarket) -> &'static str {
    match market {
        BinanceMarket::Spot => "market_participants.binance.spot.websocket",
        BinanceMarket::UsdMPerp => "market_participants.binance.usd-m-perp.websocket",
    }
}

const fn section_id(market: BinanceMarket) -> &'static str {
    match market {
        BinanceMarket::Spot => "market_participants.binance_0",
        BinanceMarket::UsdMPerp => "market_participants.binance_1",
    }
}
