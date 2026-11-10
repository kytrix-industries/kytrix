//! The parts every venue page shares: credentials, published origins, and their search rows.
//!
//! A venue page is native UI written for the settings that venue's API has, so this module holds
//! only the conversions that are the same for every venue: how a credential value is read and
//! written, how one Binance market's origin pair is updated, and how a row is described to the
//! search. The layout of a page stays in the page that draws it.

use std::rc::Rc;

use app_config::{
    market_participants::{
        origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
        settings::{BinanceOrigins, VenueSettings},
    },
    settings::Config,
};
use base_ui::{input::Input, select::Select, size::UnifiedSizable};
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{
    layout::{self, PageArgs},
    search::{FieldSearch, words},
};

/// One value of one venue's credential set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialField {
    BinanceApiKey,
    BinancePrivateKey,
    BinancePassphrase,
    OkxApiKey,
    OkxSecretKey,
    OkxPassphrase,
    MainAccountAddress,
    ApiAgentPrivateKey,
    LongbridgeAppKey,
    LongbridgeAppSecret,
    LongbridgeAccessToken,
}

impl CredentialField {
    /// The name the settings store this value under in one Binance market.
    ///
    /// Binance's two markets hold a set each, so a value's name carries the market it belongs to:
    /// `spot_api_key` and `usd_m_perp_api_key` are two values of one venue, and a name that did
    /// not say which would be a write the runtime could not place.
    pub(crate) const fn wire_name(self, market: BinanceMarket) -> &'static str {
        match (market, self) {
            (BinanceMarket::UsdMPerp, Self::BinanceApiKey) => "usd_m_perp_api_key",
            (BinanceMarket::UsdMPerp, Self::BinancePrivateKey) => "usd_m_perp_private_key",
            (BinanceMarket::UsdMPerp, Self::BinancePassphrase) => "usd_m_perp_passphrase",
            (_, Self::BinanceApiKey) => "spot_api_key",
            (_, Self::BinancePrivateKey) => "spot_private_key",
            (_, Self::BinancePassphrase) => "spot_passphrase",
            (_, Self::OkxApiKey) => "api_key",
            (_, Self::OkxSecretKey) => "secret_key",
            (_, Self::OkxPassphrase) => "passphrase",
            (_, Self::MainAccountAddress) => "main_account_address",
            (_, Self::ApiAgentPrivateKey) => "api_agent_private_key",
            (_, Self::LongbridgeAppKey) => "app_key",
            (_, Self::LongbridgeAppSecret) => "app_secret",
            (_, Self::LongbridgeAccessToken) => "access_token",
        }
    }

    /// The name this value is drawn under.
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::BinanceApiKey | Self::OkxApiKey => "settings_ui.market_participants.api_key",
            Self::BinancePrivateKey => "settings_ui.market_participants.ed25519_private_key",
            Self::BinancePassphrase => "settings_ui.market_participants.ed25519_passphrase",
            Self::OkxSecretKey => "settings_ui.market_participants.secret_key",
            Self::OkxPassphrase => "settings_ui.market_participants.passphrase",
            Self::MainAccountAddress => "settings_ui.market_participants.main_account_address",
            Self::ApiAgentPrivateKey => "settings_ui.market_participants.api_agent_private_key",
            Self::LongbridgeAppKey => "settings_ui.market_participants.app_key",
            Self::LongbridgeAppSecret => "settings_ui.market_participants.app_secret",
            Self::LongbridgeAccessToken => "settings_ui.market_participants.access_token",
        }
    }

    /// The line under the row that says what the value is for.
    pub(crate) const fn description_key(self) -> &'static str {
        match self {
            Self::BinanceApiKey | Self::OkxApiKey => {
                "settings_ui.market_participants.api_key_description"
            }
            Self::BinancePrivateKey | Self::ApiAgentPrivateKey => {
                "settings_ui.market_participants.private_key_description"
            }
            Self::BinancePassphrase | Self::OkxPassphrase => {
                "settings_ui.market_participants.passphrase_description"
            }
            Self::OkxSecretKey => "settings_ui.market_participants.secret_key_description",
            Self::MainAccountAddress => "settings_ui.market_participants.main_account_description",
            Self::LongbridgeAppKey => "settings_ui.market_participants.app_key_description",
            Self::LongbridgeAppSecret => "settings_ui.market_participants.app_secret_description",
            Self::LongbridgeAccessToken => {
                "settings_ui.market_participants.access_token_description"
            }
        }
    }

    /// The hint a row shows while it is empty, if it has one.
    pub(crate) const fn placeholder_key(self) -> Option<&'static str> {
        Some(match self {
            Self::BinanceApiKey | Self::OkxApiKey => {
                "settings_ui.market_participants.enter_api_key"
            }
            Self::BinancePrivateKey | Self::ApiAgentPrivateKey => {
                "settings_ui.market_participants.enter_private_key"
            }
            Self::BinancePassphrase | Self::OkxPassphrase => {
                "settings_ui.market_participants.enter_passphrase"
            }
            Self::OkxSecretKey => "settings_ui.market_participants.enter_secret_key",
            Self::MainAccountAddress => return None,
            Self::LongbridgeAppKey => "settings_ui.market_participants.enter_app_key",
            Self::LongbridgeAppSecret => "settings_ui.market_participants.enter_app_secret",
            Self::LongbridgeAccessToken => "settings_ui.market_participants.enter_access_token",
        })
    }

    /// Whether the value is drawn masked.
    ///
    /// A private key and a passphrase are secrets a shoulder must not read; an account address and
    /// an API key are identities the operator checks against the venue's own page, so they are
    /// drawn as they are. Longbridge's app key is that kind of identity, and the two values that
    /// sign with it are not.
    pub(crate) const fn password(self) -> bool {
        matches!(
            self,
            Self::BinancePrivateKey
                | Self::BinancePassphrase
                | Self::OkxSecretKey
                | Self::OkxPassphrase
                | Self::ApiAgentPrivateKey
                | Self::LongbridgeAppSecret
                | Self::LongbridgeAccessToken
        )
    }

    /// The id one venue's row for this value is drawn and searched under.
    pub(crate) fn field_id(self, venue: &VenueId, market: Option<BinanceMarket>) -> SharedString {
        match market {
            Some(market) => SharedString::from(format!(
                "market_participants.{}.{}.{}",
                venue.as_str(),
                market.scope(),
                self.wire_name(market)
            )),
            None => SharedString::from(format!(
                "market_participants.{}.{}",
                venue.as_str(),
                self.wire_name(BinanceMarket::Spot)
            )),
        }
    }
}

/// One credential row of one venue.
pub(crate) fn credential_row(
    args: &mut PageArgs<'_, '_>,
    venue: VenueId,
    market: Option<BinanceMarket>,
    field: CredentialField,
) -> AnyElement {
    let field_id = field.field_id(&venue, market);
    let label = t!(args.cx, field.label_key());
    let description = t!(args.cx, field.description_key());
    let placeholder = field.placeholder_key().map(|key| t!(args.cx, key));
    let stored = credential_value(
        args.config,
        &venue,
        field.wire_name(market.unwrap_or(BinanceMarket::Spot)),
    );
    let wire_name = field.wire_name(market.unwrap_or(BinanceMarket::Spot));
    let write = Rc::new(move |config: &mut Config, value: &str| {
        config.set_venue_credential(&venue, wire_name, value);
    });
    let input = args.settings.text_input(
        field_id.clone(),
        stored,
        placeholder,
        field.password(),
        write,
        args.window,
        args.cx,
    );

    layout::row(
        field_id.as_ref(),
        label,
        description,
        Input::new(&input).medium().into_any_element(),
        layout::CONTROL_WIDTH,
        args.cx,
    )
}

/// The search row of one credential value.
pub(crate) fn credential_search_field(
    venue: VenueId,
    market: Option<BinanceMarket>,
    field: CredentialField,
    cx: &App,
) -> FieldSearch {
    let id = field.field_id(&venue, market);
    let label = t!(cx, field.label_key());
    let description = t!(cx, field.description_key());

    FieldSearch {
        text: words(&[
            id.as_ref(),
            label.as_ref(),
            description.as_ref(),
            "market_participants",
            "credential",
            "市场参与者",
            "凭据",
        ]),
        id,
    }
}

/// Reads one credential value of one venue.
pub(crate) fn credential_value(config: &Config, venue: &VenueId, wire_name: &str) -> SharedString {
    let Some(settings) = config.venue(venue) else {
        return SharedString::default();
    };

    match settings {
        VenueSettings::Binance(binance) => {
            let credentials = binance
                .credentials
                .market(market_of_name(wire_name).credential_market());
            match wire_name {
                "spot_api_key" | "usd_m_perp_api_key" => SharedString::from(credentials.api_key),
                "spot_private_key" | "usd_m_perp_private_key" => {
                    SharedString::from(credentials.private_key)
                }
                "spot_passphrase" | "usd_m_perp_passphrase" => {
                    SharedString::from(credentials.passphrase)
                }
                _ => SharedString::default(),
            }
        }
        VenueSettings::Okx(okx) => match wire_name {
            "api_key" => SharedString::from(okx.credentials.api_key.as_str()),
            "secret_key" => SharedString::from(okx.credentials.secret_key.as_str()),
            "passphrase" => SharedString::from(okx.credentials.passphrase.as_str()),
            _ => SharedString::default(),
        },
        VenueSettings::Hyperliquid(hyperliquid) => match wire_name {
            "main_account_address" => {
                SharedString::from(hyperliquid.credentials.main_account_address.as_str())
            }
            "api_agent_private_key" => {
                SharedString::from(hyperliquid.credentials.api_agent_private_key.as_str())
            }
            _ => SharedString::default(),
        },
        VenueSettings::Longbridge(longbridge) => match wire_name {
            "app_key" => SharedString::from(longbridge.credentials.app_key.as_str()),
            "app_secret" => SharedString::from(longbridge.credentials.app_secret.as_str()),
            "access_token" => SharedString::from(longbridge.credentials.access_token.as_str()),
            _ => SharedString::default(),
        },
        VenueSettings::Polymarket(_) => SharedString::default(),
    }
}

/// The Binance market one credential value's wire name belongs to.
fn market_of_name(wire_name: &str) -> BinanceMarket {
    match wire_name {
        "usd_m_perp_api_key" | "usd_m_perp_private_key" | "usd_m_perp_passphrase" => {
            BinanceMarket::UsdMPerp
        }
        _ => BinanceMarket::Spot,
    }
}

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

/// A select over the published addresses of one origin row.
pub(crate) fn address_select(
    args: &mut PageArgs<'_, '_>,
    field_id: &str,
    selected: &str,
    choices: impl IntoIterator<Item = &'static str>,
    label: SharedString,
    apply: impl Fn(&mut Config, &str) + 'static,
) -> Select {
    let choices = choices.into_iter().map(|address| {
        (
            SharedString::new_static(address),
            SharedString::new_static(address),
        )
    });

    layout::choice_select(
        layout::element_id("settings-origin", field_id),
        Some(SharedString::from(selected)),
        choices,
        label,
        None,
        false,
        layout::on_select(args.handle, apply),
    )
}

/// The search row of one origin.
pub(crate) fn origin_search_field(
    id: impl Into<SharedString>,
    label: &str,
    description: &str,
    keywords: &[&str],
) -> FieldSearch {
    let id = id.into();
    let mut parts = Vec::with_capacity(keywords.len() + 3);
    parts.push(id.as_ref());
    parts.push(label);
    parts.push(description);
    parts.extend_from_slice(keywords);

    FieldSearch {
        text: words(&parts),
        id,
    }
}

/// The page id one venue's settings are drawn under.
pub(crate) fn page_id(venue: &VenueId) -> SharedString {
    SharedString::from(format!("market_participants.{}", venue.as_str()))
}
