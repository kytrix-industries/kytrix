use std::rc::Rc;

use app_config::{
    market_participants::{
        origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
        settings::{BinanceOrigins, VenueSettings},
    },
    settings::Config,
};
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, event::Event, window::WindowId};
use ui_kit::{kit::select::Select, theme::handle::ThemeHandle};

use crate::{
    host::SettingsHost,
    layout::{self, RowFn},
    pages::page::PageBuild,
    search::{FieldSearch, words},
    session::SettingsChange,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OriginKind {
    Http,
    Websocket,
}

impl OriginKind {
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Http => "settings_ui.market_participants.http_api_endpoint",
            Self::Websocket => "settings_ui.market_participants.websocket_api_endpoint",
        }
    }

    pub(crate) const fn description_key(self) -> &'static str {
        match self {
            Self::Http => "settings_ui.market_participants.http_api_endpoint_description",
            Self::Websocket => "settings_ui.market_participants.websocket_api_endpoint_description",
        }
    }
}

pub(crate) fn binance_origins(
    config: &Config,
    venue: &VenueId,
    market: BinanceMarket,
) -> Option<BinanceOrigins> {
    match config.venue(venue) {
        Some(VenueSettings::Binance(settings)) => settings.market_origins(market),
        _ => None,
    }
}

pub(crate) fn write_binance_origin(
    config: &mut Config,
    venue: &VenueId,
    market: BinanceMarket,
    kind: OriginKind,
    value: &str,
) {
    let Some(mut origins) = binance_origins(config, venue, market) else {
        return;
    };

    match kind {
        OriginKind::Http => {
            let Some(choice) = BinanceHttpOrigin::for_market(market)
                .iter()
                .find(|choice| choice.as_str() == value)
            else {
                return;
            };
            origins.http = *choice;
        }
        OriginKind::Websocket => {
            let Some(choice) = BinanceWebsocketOrigin::for_market(market)
                .iter()
                .find(|choice| choice.as_str() == value)
            else {
                return;
            };
            origins.websocket = *choice;
        }
    }

    config.set_binance_market_origins(venue, market, &origins);
}

pub(crate) fn origin_search_field(
    id: SmolStr,
    label: SmolStr,
    description: SmolStr,
    keywords: &[&str],
) -> FieldSearch {
    let mut parts = Vec::with_capacity(keywords.len() + 3);
    parts.push(id.as_ref());
    parts.push(label.as_ref());
    parts.push(description.as_ref());
    parts.extend_from_slice(keywords);
    let text = words(&parts);

    FieldSearch { id, text }
}

pub(crate) fn page_id(venue: &VenueId) -> SmolStr {
    format_smolstr!("market_participants.{}", venue.as_str())
}

pub(crate) fn control_key(cx: &PageBuild<'_>, field_id: &str) -> SmolStr {
    format_smolstr!("{}.{field_id}", cx.key)
}

pub(crate) fn address_select(
    app: &mut App,
    host: &SettingsHost,
    window: WindowId,
    key: SmolStr,
    choices: impl IntoIterator<Item = &'static str>,
    label: SmolStr,
    current: impl Fn(&Config) -> SmolStr + 'static,
    apply: impl Fn(&mut Config, &str) + 'static,
) -> VenueSelect {
    let choices = choices.into_iter().map(|address| {
        let address = SmolStr::new_static(address);
        (address.clone(), address)
    });

    VenueSelect::new(app, host, window, key, current, choices, label, apply)
}

pub(crate) struct VenueSelect {
    host: SettingsHost,
    select: Select,
    current: Box<dyn Fn(&Config) -> SmolStr>,
}

impl VenueSelect {
    pub(crate) fn new(
        app: &mut App,
        host: &SettingsHost,
        window: WindowId,
        key: SmolStr,
        current: impl Fn(&Config) -> SmolStr + 'static,
        choices: impl IntoIterator<Item = (SmolStr, SmolStr)>,
        label: SmolStr,
        apply: impl Fn(&mut Config, &str) + 'static,
    ) -> Self {
        let selected = host.config(app, |config| current(config));
        let write_host = host.clone();
        let apply = Rc::new(apply);
        let select = layout::choice_select(
            app,
            host,
            key,
            selected.as_str(),
            choices,
            label,
            move |app, value| {
                let value = SmolStr::new(value);
                let apply = Rc::clone(&apply);
                let _ = write_host.write(
                    app,
                    window,
                    SettingsChange::edit(move |config| apply(config, value.as_str())),
                );
            },
        );

        Self {
            host: host.clone(),
            select,
            current: Box::new(current),
        }
    }

    pub(crate) fn row(
        &self,
        theme: &ThemeHandle,
        id: &'static str,
        label: SmolStr,
        description: SmolStr,
    ) -> RowFn {
        let select = self.select.clone();
        layout::row(theme, id, label, description, move |app, window, parent| {
            select.mount(app, window, parent)?;
            Ok(())
        })
    }

    pub(crate) fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        let value = self.host.config(app, |config| (self.current)(config));
        self.select.set_chosen(app, [value.as_str()], false);
        self.select.sync(app)?;
        Ok(())
    }

    pub(crate) fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.select.dispatch(app, event)
    }

    pub(crate) fn release(&self, app: &mut App) {
        self.select.release(app);
    }
}
