use std::{collections::BTreeMap, fmt, sync::Arc};

use app_config::{
    market_participants::{
        error::MarketParticipantsConfigError,
        origins::{BinanceMarket, VenueEndpoints},
        settings::VenueSettings,
    },
    network::NetworkRouteGeneration,
    settings::Config,
};
use market_participants_model::identifier::{VenueId, VenueScopeId};
use smol_str::SmolStr;
use url::Url;

use crate::routing::CredentialGeneration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialState<T> {
    Unconfigured,
    Configured(T),
    Invalid(MarketParticipantsConfigError),
}

impl<T> CredentialState<T> {
    const fn is_configured(&self) -> bool {
        matches!(self, Self::Configured(_))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BinanceCredentialValues {
    pub api_key: SmolStr,
    pub private_key: SmolStr,
    pub passphrase: Option<SmolStr>,
}

impl fmt::Debug for BinanceCredentialValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BinanceCredentialValues")
            .field("api_key", &"<redacted>")
            .field("private_key", &"<redacted>")
            .field("passphrase", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OkxCredentialValues {
    pub api_key: SmolStr,
    pub secret_key: SmolStr,
    pub passphrase: SmolStr,
}

impl fmt::Debug for OkxCredentialValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OkxCredentialValues")
            .field("api_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("passphrase", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HyperliquidCredentialValues {
    pub main_account_address: SmolStr,
    pub api_agent_private_key: SmolStr,
}

impl fmt::Debug for HyperliquidCredentialValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HyperliquidCredentialValues")
            .field("main_account_address", &"<redacted>")
            .field("api_agent_private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LongbridgeCredentialValues {
    pub app_key: SmolStr,
    pub app_secret: SmolStr,
    pub access_token: SmolStr,
}

impl fmt::Debug for LongbridgeCredentialValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LongbridgeCredentialValues")
            .field("app_key", &"<redacted>")
            .field("app_secret", &"<redacted>")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CredentialFingerprint {
    Binance([CredentialState<BinanceCredentialValues>; 2]),
    Okx(CredentialState<OkxCredentialValues>),
    Hyperliquid(CredentialState<HyperliquidCredentialValues>),
    Longbridge(CredentialState<LongbridgeCredentialValues>),
    Polymarket,
}

impl CredentialFingerprint {
    fn is_configured(&self) -> bool {
        match self {
            Self::Binance(markets) => markets.iter().any(CredentialState::is_configured),
            Self::Okx(state) => state.is_configured(),
            Self::Hyperliquid(state) => state.is_configured(),
            Self::Longbridge(state) => state.is_configured(),
            Self::Polymarket => false,
        }
    }
}

#[derive(Clone, Debug)]
struct CredentialRuntime {
    fingerprint: CredentialFingerprint,
    generation: Option<CredentialGeneration>,
}

#[derive(Clone, Debug)]
pub struct MarketParticipantNetworkSnapshot {
    network: Arc<app_config::network::Config>,
    venues: BTreeMap<VenueId, VenueSettings>,
    credentials: BTreeMap<VenueId, CredentialRuntime>,
    next_generation: u64,
}

impl Default for MarketParticipantNetworkSnapshot {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl MarketParticipantNetworkSnapshot {
    pub fn new(config: Config) -> Self {
        Self::from_previous(config, None)
    }

    pub fn from_previous(config: Config, previous: Option<&Self>) -> Self {
        let network = Arc::new(config.network_config());
        let mut snapshot = Self {
            network,
            venues: config.venues,
            credentials: BTreeMap::new(),
            next_generation: previous.map_or(1, |previous| previous.next_generation),
        };
        let fingerprints: Vec<(VenueId, CredentialFingerprint)> = snapshot
            .venues
            .iter()
            .map(|(venue, settings)| (*venue, snapshot.credential_fingerprint(venue, settings)))
            .collect();
        let mut next_generation = snapshot.next_generation;
        for (venue, fingerprint) in fingerprints {
            let generation = if fingerprint.is_configured() {
                previous
                    .and_then(|snapshot| snapshot.credentials.get(&venue))
                    .filter(|runtime| runtime.fingerprint == fingerprint)
                    .and_then(|runtime| runtime.generation)
                    .or_else(|| {
                        let generation = CredentialGeneration::new(next_generation);
                        next_generation = next_generation.saturating_add(1);
                        Some(generation)
                    })
            } else {
                None
            };
            snapshot.credentials.insert(
                venue,
                CredentialRuntime {
                    fingerprint,
                    generation,
                },
            );
        }
        snapshot.next_generation = next_generation;
        snapshot
    }

    pub fn network(&self) -> &Arc<app_config::network::Config> {
        &self.network
    }

    pub(crate) fn venue_settings(&self, venue: &VenueId) -> Option<&VenueSettings> {
        self.venues.get(venue)
    }

    pub(crate) fn credentials_configured(&self, venue: &VenueId) -> bool {
        let Some(settings) = self.venue_settings(venue) else {
            return false;
        };
        matches!(
            settings.validate_credentials(*venue, settings.primary_scope()),
            Ok(true)
        )
    }

    #[cfg(test)]
    pub(crate) fn origins(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> Option<app_config::market_participants::origins::OriginPair> {
        self.endpoints(venue, scope)
            .map(|endpoints| endpoints.primary)
    }

    pub(crate) fn endpoints(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> Option<VenueEndpoints> {
        let settings = self.venue_settings(venue)?;
        Some(
            settings
                .endpoints(scope.as_str())
                .unwrap_or_else(|| VenueEndpoints {
                    primary: settings.deployed_origins(),
                    metadata: None,
                }),
        )
    }

    pub(crate) fn serves(&self, venue: &VenueId, scope: &VenueScopeId, dialled: &Url) -> bool {
        self.endpoints(venue, scope).is_some_and(|endpoints| {
            let primary = if is_websocket(dialled) {
                &endpoints.primary.websocket
            } else {
                &endpoints.primary.http
            };
            let metadata = endpoints.metadata.as_ref().and_then(|service| {
                if is_websocket(dialled) {
                    service.websocket.as_ref()
                } else {
                    Some(&service.http)
                }
            });
            primary.origin() == dialled.origin()
                || metadata.is_some_and(|url| url.origin() == dialled.origin())
        })
    }

    pub(crate) fn network_route_generation(&self, venue: &VenueId) -> NetworkRouteGeneration {
        self.network.network_route_generation(venue)
    }

    pub(crate) fn credential_generation(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> Option<CredentialGeneration> {
        let settings = self.venue_settings(venue)?;
        let runtime = self.credentials.get(venue)?;
        let credential_scope = settings.credential_scope(scope.as_str());
        match settings.validate_credentials(*venue, credential_scope) {
            Ok(true) => runtime.generation,
            Ok(false) | Err(_) => None,
        }
    }

    pub(crate) fn resolve_binance_credentials(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> CredentialState<BinanceCredentialValues> {
        let Some(settings @ VenueSettings::Binance(binance)) = self.venue_settings(venue) else {
            return CredentialState::Unconfigured;
        };
        let Some(market) = binance_market(scope.as_str()) else {
            return CredentialState::Unconfigured;
        };
        match settings.validate_credentials(*venue, market.scope()) {
            Ok(false) => CredentialState::Unconfigured,
            Err(error) => CredentialState::Invalid(error),
            Ok(true) => {
                let stored = binance
                    .market_credentials(market)
                    .expect("a market whose credentials are complete stores them");
                CredentialState::Configured(BinanceCredentialValues {
                    api_key: stored.api_key.clone(),
                    private_key: stored.private_key.clone(),
                    passphrase: (!stored.passphrase.trim().is_empty())
                        .then(|| stored.passphrase.clone()),
                })
            }
        }
    }

    pub(crate) fn resolve_okx_credentials(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> CredentialState<OkxCredentialValues> {
        let Some(settings @ VenueSettings::Okx(okx)) = self.venue_settings(venue) else {
            return CredentialState::Unconfigured;
        };
        let credential_scope = settings.credential_scope(scope.as_str());
        match settings.validate_credentials(*venue, credential_scope) {
            Ok(false) => CredentialState::Unconfigured,
            Err(error) => CredentialState::Invalid(error),
            Ok(true) => {
                let stored = okx
                    .credentials
                    .as_ref()
                    .expect("a venue whose credentials are complete stores them");
                CredentialState::Configured(OkxCredentialValues {
                    api_key: stored.api_key.clone(),
                    secret_key: stored.secret_key.clone(),
                    passphrase: stored.passphrase.clone(),
                })
            }
        }
    }

    pub(crate) fn resolve_hyperliquid_credentials(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> CredentialState<HyperliquidCredentialValues> {
        let Some(settings @ VenueSettings::Hyperliquid(hypercore)) = self.venue_settings(venue)
        else {
            return CredentialState::Unconfigured;
        };
        let credential_scope = settings.credential_scope(scope.as_str());
        match settings.validate_credentials(*venue, credential_scope) {
            Ok(false) => CredentialState::Unconfigured,
            Err(error) => CredentialState::Invalid(error),
            Ok(true) => {
                let stored = hypercore
                    .credentials
                    .as_ref()
                    .expect("a venue whose credentials are complete stores them");
                CredentialState::Configured(HyperliquidCredentialValues {
                    main_account_address: stored.main_account_address.clone(),
                    api_agent_private_key: stored.api_agent_private_key.clone(),
                })
            }
        }
    }

    pub(crate) fn resolve_longbridge_credentials(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> CredentialState<LongbridgeCredentialValues> {
        let Some(settings @ VenueSettings::Longbridge(longbridge)) = self.venue_settings(venue)
        else {
            return CredentialState::Unconfigured;
        };
        let credential_scope = settings.credential_scope(scope.as_str());
        match settings.validate_credentials(*venue, credential_scope) {
            Ok(false) => CredentialState::Unconfigured,
            Err(error) => CredentialState::Invalid(error),
            Ok(true) => {
                let stored = longbridge
                    .credentials
                    .as_ref()
                    .expect("a venue whose credentials are complete stores them");
                CredentialState::Configured(LongbridgeCredentialValues {
                    app_key: stored.app_key.clone(),
                    app_secret: stored.app_secret.clone(),
                    access_token: stored.access_token.clone(),
                })
            }
        }
    }

    fn credential_fingerprint(
        &self,
        venue: &VenueId,
        settings: &VenueSettings,
    ) -> CredentialFingerprint {
        match settings {
            VenueSettings::Binance(_) => {
                CredentialFingerprint::Binance(BinanceMarket::ALL.map(|market| {
                    self.resolve_binance_credentials(venue, &published_scope(market.scope()))
                }))
            }
            VenueSettings::Okx(_) => CredentialFingerprint::Okx(
                self.resolve_okx_credentials(venue, &published_scope(settings.primary_scope())),
            ),
            VenueSettings::Hyperliquid(_) => {
                CredentialFingerprint::Hyperliquid(self.resolve_hyperliquid_credentials(
                    venue,
                    &published_scope(settings.primary_scope()),
                ))
            }
            VenueSettings::Longbridge(_) => {
                CredentialFingerprint::Longbridge(self.resolve_longbridge_credentials(
                    venue,
                    &published_scope(settings.primary_scope()),
                ))
            }
            VenueSettings::Polymarket(_) => CredentialFingerprint::Polymarket,
        }
    }
}

fn is_websocket(dialled: &Url) -> bool {
    matches!(dialled.scheme(), "ws" | "wss")
}

fn binance_market(scope: &str) -> Option<BinanceMarket> {
    BinanceMarket::ALL
        .into_iter()
        .find(|market| market.scope() == scope)
}

fn published_scope(name: &str) -> VenueScopeId {
    VenueScopeId::new(name).expect("a statically published scope name is canonical")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use app_config::{
        market_participants::{
            credentials::{
                BinanceMarketCredentials, HyperliquidCredentials, LongbridgeCredentials,
                OkxCredentials,
            },
            error::MarketParticipantsConfigError,
            origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
            settings::{BinanceOrigins, OkxSettings, VenueSettings, default_venues},
        },
        network::{NetworkRouteGeneration, NetworkRuntimeGenerations, ProxyRoute},
        settings::Config,
    };
    use market_participants_model::identifier::{VenueId, VenueScopeId};
    use smol_str::SmolStr;

    use super::{CredentialGeneration, CredentialState, MarketParticipantNetworkSnapshot};
    use crate::routing::{
        CORE_PERP_SCOPE, LONGBRIDGE_US_SCOPE, PERPETUAL_SCOPE, SPOT_SCOPE, SWAP_SCOPE,
    };

    fn scope(name: &str) -> VenueScopeId {
        VenueScopeId::new(name).expect("the scope should be canonical")
    }

    fn binance_spot(config: &mut Config, api_key: &str, private_key: &str) {
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&VenueId::Binance)
        else {
            panic!("binance is configured as Binance by the built-in table");
        };
        settings.set_market_credentials(
            BinanceMarket::Spot,
            Some(BinanceMarketCredentials {
                api_key: SmolStr::new(api_key),
                private_key: SmolStr::new(private_key),
                passphrase: SmolStr::default(),
            }),
        );
    }

    fn binance_perpetual(config: &mut Config, api_key: &str, private_key: &str) {
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&VenueId::Binance)
        else {
            panic!("binance is configured as Binance by the built-in table");
        };
        settings.set_market_credentials(
            BinanceMarket::UsdMPerp,
            Some(BinanceMarketCredentials {
                api_key: SmolStr::new(api_key),
                private_key: SmolStr::new(private_key),
                passphrase: SmolStr::default(),
            }),
        );
    }

    fn okx(config: &mut Config, api_key: &str) {
        let Some(VenueSettings::Okx(settings)) = config.venues.get_mut(&VenueId::Okx) else {
            panic!("okx is configured as OKX by the built-in table");
        };
        settings.credentials = Some(OkxCredentials {
            api_key: SmolStr::new(api_key),
            secret_key: SmolStr::new_static("okx-secret"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        });
    }

    fn hyperliquid(config: &mut Config, address: &str, private_key: &str) {
        let Some(VenueSettings::Hyperliquid(settings)) =
            config.venues.get_mut(&VenueId::Hyperliquid)
        else {
            panic!("hyperliquid is configured as HyperCore by the built-in table");
        };
        settings.credentials = Some(HyperliquidCredentials {
            main_account_address: SmolStr::new(address),
            api_agent_private_key: SmolStr::new(private_key),
        });
    }

    fn longbridge_credentials(config: &mut Config, app_key: &str, app_secret: &str, token: &str) {
        let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&VenueId::Longbridge)
        else {
            panic!("longbridge is configured as Longbridge by the built-in table");
        };
        settings.credentials = Some(LongbridgeCredentials {
            app_key: SmolStr::new(app_key),
            app_secret: SmolStr::new(app_secret),
            access_token: SmolStr::new(token),
        });
    }

    fn spot_mirror() -> BinanceOrigins {
        BinanceOrigins::new(
            BinanceMarket::Spot,
            BinanceHttpOrigin::Api3,
            BinanceWebsocketOrigin::StreamAlt,
        )
    }

    #[test]
    fn an_unchanged_configuration_preserves_every_generation() {
        let mut config = Config::default();
        binance_spot(&mut config, "api-key", "private-key");
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let repeated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));

        assert_eq!(
            repeated.origins(&VenueId::Binance, &scope(SPOT_SCOPE)),
            initial.origins(&VenueId::Binance, &scope(SPOT_SCOPE)),
        );
        assert_eq!(
            repeated.credential_generation(&VenueId::Binance, &scope(SPOT_SCOPE)),
            initial.credential_generation(&VenueId::Binance, &scope(SPOT_SCOPE))
        );
    }

    #[test]
    fn a_venue_origin_change_moves_only_that_venue() {
        let initial = MarketParticipantNetworkSnapshot::new(Config::default());
        let mut moved = Config::default();
        assert!(moved.set_binance_market_origins(
            &VenueId::Binance,
            BinanceMarket::Spot,
            &spot_mirror()
        ));
        let updated = MarketParticipantNetworkSnapshot::from_previous(moved, Some(&initial));

        assert_eq!(
            updated.origins(&VenueId::Binance, &scope(SPOT_SCOPE)),
            Some(spot_mirror().addresses()),
            "the venue resolves the origins its settings name now"
        );
        assert_eq!(
            updated.origins(&VenueId::Okx, &scope(SPOT_SCOPE)),
            initial.origins(&VenueId::Okx, &scope(SPOT_SCOPE)),
            "no other venue moved"
        );
    }

    #[test]
    fn a_scope_the_venue_does_not_name_is_reached_through_its_primary_scope() {
        let mut config = Config::default();
        assert!(config.set_binance_market_origins(
            &VenueId::Binance,
            BinanceMarket::Spot,
            &spot_mirror()
        ));
        let snapshot = MarketParticipantNetworkSnapshot::new(config);
        let deployed = scope("perp-dex/xyz");

        assert_eq!(
            snapshot.origins(&VenueId::Binance, &deployed),
            snapshot.origins(&VenueId::Binance, &scope(SPOT_SCOPE)),
            "a scope the venue does not publish is reached through its primary scope"
        );
        assert_ne!(
            snapshot.origins(&VenueId::Binance, &deployed),
            snapshot.origins(&VenueId::Binance, &scope(PERPETUAL_SCOPE)),
            "and not through another scope of the venue"
        );
        assert!(
            !snapshot
                .venue_settings(&VenueId::Binance)
                .expect("binance is configured by the built-in table")
                .publishes(deployed.as_str()),
            "the venue's settings name no scope of their own for it"
        );
    }

    #[test]
    fn a_credential_change_rotates_only_that_venue() {
        let mut config = Config::default();
        binance_spot(&mut config, "api-a", "private-a");
        okx(&mut config, "okx-key");
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let okx_before = initial.credential_generation(&VenueId::Okx, &scope(SPOT_SCOPE));

        binance_spot(&mut config, "api-b", "private-b");
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));

        assert_ne!(
            updated.credential_generation(&VenueId::Binance, &scope(SPOT_SCOPE)),
            initial.credential_generation(&VenueId::Binance, &scope(SPOT_SCOPE))
        );
        assert_eq!(
            updated.credential_generation(&VenueId::Okx, &scope(SPOT_SCOPE)),
            okx_before,
            "another venue keeps its generation"
        );
    }

    #[test]
    fn a_venues_generation_covers_every_credential_set_it_stores() {
        let binance = VenueId::Binance;
        let mut config = Config::default();
        binance_spot(&mut config, "api-a", "private-a");
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());

        assert!(
            initial
                .credential_generation(&binance, &scope(SPOT_SCOPE))
                .is_some(),
            "the spot market's complete set has a generation"
        );
        assert_eq!(
            initial.credential_generation(&binance, &scope(PERPETUAL_SCOPE)),
            None,
            "the market the operator never started answers no generation"
        );
        assert!(matches!(
            initial.resolve_binance_credentials(&binance, &scope(PERPETUAL_SCOPE)),
            super::CredentialState::Unconfigured
        ));

        binance_perpetual(&mut config, "perp-key", "perp-private");
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));

        assert_ne!(
            updated.credential_generation(&binance, &scope(SPOT_SCOPE)),
            initial.credential_generation(&binance, &scope(SPOT_SCOPE)),
            "completing either set of the venue moves its generation"
        );
        assert!(
            updated
                .credential_generation(&binance, &scope(PERPETUAL_SCOPE))
                .is_some()
        );
        assert!(matches!(
            updated.resolve_binance_credentials(&binance, &scope(PERPETUAL_SCOPE)),
            super::CredentialState::Configured(_)
        ));
    }

    #[test]
    fn a_complete_longbridge_set_resolves_and_answers_a_generation() {
        let longbridge = VenueId::Longbridge;
        let mut config = Config::default();
        longbridge_credentials(&mut config, "app-key", "app-secret", "access-token");

        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        let super::CredentialState::Configured(credentials) =
            snapshot.resolve_longbridge_credentials(&longbridge, &scope(LONGBRIDGE_US_SCOPE))
        else {
            panic!("a complete Longbridge set resolves");
        };
        assert_eq!(credentials.app_key, "app-key");
        assert_eq!(credentials.app_secret, "app-secret");
        assert_eq!(credentials.access_token, "access-token");
        assert!(
            snapshot
                .credential_generation(&longbridge, &scope(LONGBRIDGE_US_SCOPE))
                .is_some(),
            "a set that can dial the venue has a generation of its own"
        );
        assert!(
            snapshot.credentials_configured(&longbridge),
            "and the venue reports itself configured"
        );
    }

    #[test]
    fn every_longbridge_value_is_part_of_its_venues_generation() {
        let longbridge = VenueId::Longbridge;
        let mut config = Config::default();
        longbridge_credentials(&mut config, "app-key", "app-secret", "access-token");
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let generation = initial.credential_generation(&longbridge, &scope(LONGBRIDGE_US_SCOPE));
        assert!(
            generation.is_some(),
            "the fixture set has a generation to move"
        );

        for (app_key, app_secret, access_token) in [
            ("us_app-key", "app-secret", "access-token"),
            ("app-key", "us_app-secret", "access-token"),
            ("app-key", "app-secret", "us_access-token"),
        ] {
            longbridge_credentials(&mut config, app_key, app_secret, access_token);
            let updated =
                MarketParticipantNetworkSnapshot::from_previous(config.clone(), Some(&initial));

            assert_ne!(
                updated.credential_generation(&longbridge, &scope(LONGBRIDGE_US_SCOPE)),
                generation,
                "editing a value of the Longbridge credential moves its venue's generation"
            );
        }
    }

    #[test]
    fn a_partially_configured_longbridge_venue_is_invalid() {
        let longbridge = VenueId::Longbridge;
        let mut config = Config::default();
        longbridge_credentials(&mut config, "app-key", "app-secret", "");

        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        assert!(matches!(
            snapshot.resolve_longbridge_credentials(&longbridge, &scope(LONGBRIDGE_US_SCOPE)),
            super::CredentialState::Invalid(_)
        ));
        assert_eq!(
            snapshot.credential_generation(&longbridge, &scope(LONGBRIDGE_US_SCOPE)),
            None
        );
        assert!(!snapshot.credentials_configured(&longbridge));
    }

    #[test]
    fn a_partially_configured_venue_has_no_generation() {
        let okx_venue = VenueId::Okx;
        let mut config = Config::default();
        let Some(VenueSettings::Okx(settings)) = config.venues.get_mut(&okx_venue) else {
            panic!("okx is configured as OKX by the built-in table");
        };
        settings.credentials = Some(OkxCredentials {
            api_key: SmolStr::new_static("okx-key"),
            secret_key: SmolStr::default(),
            passphrase: SmolStr::default(),
        });

        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        assert_eq!(
            snapshot.credential_generation(&okx_venue, &scope(SPOT_SCOPE)),
            None
        );
        assert_eq!(
            snapshot.credential_generation(&okx_venue, &scope(SWAP_SCOPE)),
            None,
            "the venue's whole surface signs with the one set, so both scopes answer the same"
        );
        assert!(matches!(
            snapshot.resolve_okx_credentials(&okx_venue, &scope(SPOT_SCOPE)),
            super::CredentialState::Invalid(_)
        ));
    }

    #[test]
    fn a_partially_configured_binance_market_fails_closed() {
        let binance = VenueId::Binance;
        let mut config = Config::default();
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&binance) else {
            panic!("binance is configured as Binance by the built-in table");
        };
        settings.spot_credentials = Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("k"),
            private_key: SmolStr::default(),
            passphrase: SmolStr::default(),
        });
        settings.usd_m_perp_credentials = None;

        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        assert_eq!(
            snapshot.resolve_binance_credentials(&binance, &scope(SPOT_SCOPE)),
            CredentialState::Invalid(MarketParticipantsConfigError::IncompleteCredentials {
                venue: binance,
                scope: Some(SmolStr::new_static(SPOT_SCOPE)),
            }),
            "the spot market stores a set missing its private key, so it is refused rather \
             than resolved"
        );
        assert_eq!(
            snapshot.resolve_binance_credentials(&binance, &scope(PERPETUAL_SCOPE)),
            CredentialState::Unconfigured,
            "the sibling market stores no set at all"
        );
        assert_eq!(
            snapshot.credential_generation(&binance, &scope(SPOT_SCOPE)),
            None,
            "an incomplete set signs nothing, so it answers no generation"
        );
        assert_eq!(
            snapshot.credential_generation(&binance, &scope(PERPETUAL_SCOPE)),
            None,
            "the unconfigured market answers no generation either"
        );
        assert!(
            !snapshot.credentials_configured(&binance),
            "a venue whose primary market is half filled is not configured"
        );
    }

    #[test]
    fn a_venue_holding_another_kinds_credentials_answers_nothing() {
        let binance = VenueId::Binance;
        let mut config = Config::default();
        config.venues.insert(
            binance,
            VenueSettings::Okx(OkxSettings {
                credentials: Some(OkxCredentials {
                    api_key: SmolStr::new_static("okx-key"),
                    secret_key: SmolStr::new_static("okx-secret"),
                    passphrase: SmolStr::new_static("okx-passphrase"),
                }),
                ..OkxSettings::default()
            }),
        );
        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        assert!(matches!(
            snapshot.resolve_binance_credentials(&binance, &scope(SPOT_SCOPE)),
            super::CredentialState::Unconfigured
        ));
        assert!(matches!(
            snapshot.resolve_hyperliquid_credentials(&binance, &scope(CORE_PERP_SCOPE)),
            super::CredentialState::Unconfigured
        ));
        assert!(
            snapshot
                .credential_generation(&binance, &scope(SPOT_SCOPE))
                .is_some(),
            "the row is still complete as the kind it was written as"
        );
    }

    #[test]
    fn an_unconfigured_venue_is_unconfigured_rather_than_rejected() {
        let snapshot = MarketParticipantNetworkSnapshot::new(Config::default());

        assert_eq!(
            snapshot.credential_generation(&VenueId::Binance, &scope(SPOT_SCOPE)),
            None
        );
        assert!(matches!(
            snapshot.resolve_binance_credentials(&VenueId::Binance, &scope(SPOT_SCOPE)),
            super::CredentialState::Unconfigured
        ));
        assert!(matches!(
            snapshot.resolve_okx_credentials(&VenueId::Okx, &scope(SWAP_SCOPE)),
            super::CredentialState::Unconfigured
        ));
        assert!(matches!(
            snapshot
                .resolve_hyperliquid_credentials(&VenueId::Hyperliquid, &scope(CORE_PERP_SCOPE)),
            super::CredentialState::Unconfigured
        ));
        assert!(matches!(
            snapshot.resolve_hyperliquid_credentials(&VenueId::Binance, &scope(CORE_PERP_SCOPE)),
            super::CredentialState::Unconfigured
        ));
    }

    #[test]
    fn a_venue_without_settings_answers_nothing() {
        let binance = VenueId::Binance;
        let mut config = Config::default();
        config.venues.clear();
        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        assert!(snapshot.venue_settings(&binance).is_none());
        assert_eq!(snapshot.origins(&binance, &scope(SPOT_SCOPE)), None);
        assert_eq!(
            snapshot.credential_generation(&binance, &scope(SPOT_SCOPE)),
            None
        );
        assert!(
            !snapshot.serves(
                &binance,
                &scope(SPOT_SCOPE),
                &"https://api.binance.com/api/v3/exchangeInfo"
                    .parse()
                    .expect("the origin parses")
            ),
            "a scope whose venue configures no pair is not served"
        );
        assert!(matches!(
            snapshot.resolve_binance_credentials(&binance, &scope(SPOT_SCOPE)),
            super::CredentialState::Unconfigured
        ));
    }

    #[test]
    fn a_configured_venue_resolves_its_own_credentials() {
        let hyperliquid_venue = VenueId::Hyperliquid;
        let mut config = Config::default();
        hyperliquid(&mut config, "0xabc", "key");
        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        for scope_name in [CORE_PERP_SCOPE, "perp-dex/xyz"] {
            assert!(
                matches!(
                    snapshot
                        .resolve_hyperliquid_credentials(&hyperliquid_venue, &scope(scope_name)),
                    super::CredentialState::Configured(_)
                ),
                "{scope_name} signs with the venue's one credential set"
            );
            assert!(
                snapshot
                    .credential_generation(&hyperliquid_venue, &scope(scope_name))
                    .is_some(),
                "{scope_name} answers the venue's generation"
            );
        }
    }

    #[test]
    fn a_proxy_decision_rotates_only_its_own_venue() {
        let mut config = Config::default();
        config.proxy_default = ProxyRoute::Direct;
        let mut network = config.network_config();
        network
            .venue_routes
            .insert(VenueId::Binance, ProxyRoute::Direct);
        network
            .route_generations
            .insert(VenueId::Binance, NetworkRouteGeneration::new(9));
        network
            .route_generations
            .insert(VenueId::Okx, NetworkRouteGeneration::new(3));
        config.set_network_generations(NetworkRuntimeGenerations {
            default_route: network.default_route_generation,
            venues: network.route_generations.clone(),
            services: BTreeMap::new(),
        });
        let initial = MarketParticipantNetworkSnapshot::new(config);

        assert_eq!(
            initial.network_route_generation(&VenueId::Binance),
            NetworkRouteGeneration::new(9)
        );
        assert_eq!(
            initial.network_route_generation(&VenueId::Okx),
            NetworkRouteGeneration::new(3)
        );
        assert_eq!(
            initial.network_route_generation(&VenueId::Hyperliquid),
            NetworkRouteGeneration::default(),
            "an unbound venue shares the default route generation"
        );
    }

    #[test]
    fn a_dialled_origin_is_served_only_by_the_venue_that_configures_it() {
        let snapshot = MarketParticipantNetworkSnapshot::new(Config::default());
        let binance = VenueId::Binance;

        assert!(
            snapshot.serves(
                &binance,
                &scope(SPOT_SCOPE),
                &"https://api.binance.com/api/v3/exchangeInfo"
                    .parse()
                    .expect("the origin parses")
            ),
            "a path the adapter appended does not move the origin"
        );
        assert!(
            !snapshot.serves(
                &binance,
                &scope(SPOT_SCOPE),
                &"https://api.binance.com.evil.test/api".parse().unwrap()
            ),
            "another host is not this venue's origin"
        );
        assert!(
            !snapshot.serves(
                &binance,
                &scope(SPOT_SCOPE),
                &"https://fapi.binance.com/fapi/v1/exchangeInfo"
                    .parse()
                    .unwrap()
            ),
            "another market's host is not this scope's origin"
        );
        assert!(
            snapshot.serves(
                &binance,
                &scope(SPOT_SCOPE),
                &"wss://stream.binance.com:9443/ws".parse().unwrap()
            ),
            "the stream origin is the one the scope's WebSocket role names"
        );
        assert!(
            !snapshot.serves(
                &binance,
                &scope(SPOT_SCOPE),
                &"wss://fstream.binance.com/ws".parse().unwrap()
            ),
            "a socket dialled on the other market's stream is not this scope's"
        );
        assert!(
            snapshot.serves(
                &binance,
                &scope("perp-dex/xyz"),
                &"https://api.binance.com/api".parse().unwrap()
            ),
            "a scope the venue deploys answers its primary scope's pair"
        );
        assert!(
            !snapshot.serves(
                &binance,
                &scope(PERPETUAL_SCOPE),
                &"https://api.binance.com/api".parse().unwrap()
            ),
            "and a market the venue configures apart is not reached through it"
        );
    }

    #[test]
    fn the_snapshot_never_interprets_an_upstream_generation() {
        let mut config = Config::default();
        let mut network = config.network_config();
        network.default_route_generation = NetworkRouteGeneration::new(41);
        network.route_generations =
            BTreeMap::from([(VenueId::Binance, NetworkRouteGeneration::new(42))]);
        config.set_network_generations(NetworkRuntimeGenerations {
            default_route: network.default_route_generation,
            venues: network.route_generations,
            services: BTreeMap::new(),
        });
        let snapshot = MarketParticipantNetworkSnapshot::new(config);

        assert_eq!(snapshot.network().default_route_generation.get(), 41);
        assert_eq!(
            snapshot.network_route_generation(&VenueId::Binance).get(),
            42
        );
        assert_eq!(
            snapshot.credential_generation(&VenueId::Binance, &scope(SPOT_SCOPE)),
            None::<CredentialGeneration>,
            "no venue is configured with credentials, so nothing can authenticate"
        );
    }

    #[test]
    fn every_venue_of_the_built_in_table_is_configured() {
        let snapshot = MarketParticipantNetworkSnapshot::new(Config::default());

        for venue in VenueId::ALL {
            let settings = snapshot
                .venue_settings(&venue)
                .expect("the built-in table configures every venue");
            assert!(
                snapshot
                    .origins(&venue, &scope(settings.primary_scope()))
                    .is_some(),
                "{venue} resolves the origins of its own primary scope"
            );
        }
        assert_eq!(default_venues().len(), VenueId::ALL.len());
    }
}
