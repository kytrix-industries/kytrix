//! The parts every venue page shares: the published origins and their search rows.
//!
//! A venue page is native UI written for the settings that venue's API has, so this module holds
//! only the conversions that are the same for every venue: how one Binance market's origin pair is
//! updated, how a published address is offered as a choice, and how a row is described to the
//! search. The layout of a page stays in the page that draws it.

use std::rc::Rc;

use app_config::{
    market_participants::{
        origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
        settings::{BinanceOrigins, VenueSettings},
    },
    settings::Config,
};
use base_ui::{kit::select::Select, theme::handle::ThemeHandle};
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, event::Event, window::WindowId};

use crate::{
    host::SettingsHost,
    layout::{self, RowFn},
    pages::page::PageBuild,
    search::{FieldSearch, words},
    session::SettingsChange,
};

/// Which of one scope's two addresses a row writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OriginKind {
    Http,
    Websocket,
}

impl OriginKind {
    /// The name the row is drawn under.
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Http => "settings_ui.market_participants.http_api_endpoint",
            Self::Websocket => "settings_ui.market_participants.websocket_api_endpoint",
        }
    }

    /// The line under the row that says what the endpoint is for.
    pub(crate) const fn description_key(self) -> &'static str {
        match self {
            Self::Http => "settings_ui.market_participants.http_api_endpoint_description",
            Self::Websocket => "settings_ui.market_participants.websocket_api_endpoint_description",
        }
    }
}

/// The address choices one Binance market holds, or nothing while the venue is not configured as
/// Binance.
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

/// Writes one Binance market's choice of one of its two addresses.
///
/// The pair is written as a whole — the other role is carried over unchanged — because a market is
/// reached at one pair of addresses and not at two values of different shapes. A value no table
/// publishes writes nothing: the choices are the venue's own, and a control that offered one this
/// build does not know is not a control it drew.
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

/// The search row of one origin.
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

/// The page id one venue's settings are drawn under.
pub(crate) fn page_id(venue: &VenueId) -> SmolStr {
    format_smolstr!("market_participants.{}", venue.as_str())
}

/// The application-level key one control of a venue page is named by.
///
/// It is the settings page instance's key followed by the row's field id, which already names the
/// page: two panes showing the same page are two settings page instances, so their controls are
/// two identities and one pane's control never dispatches the other pane's handler.
pub(crate) fn control_key(cx: &PageBuild<'_>, field_id: &str) -> SmolStr {
    format_smolstr!("{}.{field_id}", cx.key)
}

/// A select over the published addresses of one origin row.
///
/// The choices are the addresses themselves: a published address is its own id and its own label,
/// and a value no table publishes writes nothing (see [`write_binance_origin`]).
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

/// One setting of a venue page as the page draws it: the value the settings hold and the selector
/// that writes it.
///
/// The selector is a value the page keeps: it is what `sync` writes the settings of the frame into
/// and what `release` gives back. A value the settings hold that no choice names is still handed to
/// the selector — the row shows what the settings hold, not what the table offers — and the address
/// writers ignore a value they do not publish, so such a row writes nothing until it is changed.
pub(crate) struct VenueSelect {
    host: SettingsHost,
    select: Select,
    /// The value the settings hold for this row, read for the frame's `sync`.
    current: Box<dyn Fn(&Config) -> SmolStr>,
}

impl VenueSelect {
    /// The selector over `choices`, writing what the user chose through `apply`.
    ///
    /// `current` reads the value the row draws from the settings; it is read once here for the
    /// first frame and on every `sync` afterwards.
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

    /// The row this selector is drawn in.
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

    /// Makes the selector show the value the settings hold for this frame.
    ///
    /// # Errors
    /// The selector refused a write (`StaleNode`, or a selector whose nodes were removed).
    pub(crate) fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        let value = self.host.config(app, |config| (self.current)(config));
        self.select.set_chosen(app, [value.as_str()], false);
        self.select.sync(app)?;
        Ok(())
    }

    /// Offers one platform event to the selector.
    pub(crate) fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.select.dispatch(app, event)
    }

    /// Releases the selector.
    pub(crate) fn release(&self, app: &mut App) {
        self.select.release(app);
    }
}
