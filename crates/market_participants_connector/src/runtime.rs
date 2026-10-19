//! Typed market-participants/network runtime snapshots.
//!
//! The snapshot answers every transport question a request asks: which origins the
//! request's **venue** gives its scope, which generation that venue's routing
//! decision is on, and which generation its credentials are on. A venue is the
//! connection configuration, so nothing here is keyed by an endpoint or a
//! credential set: the venue key *is* the key, and a scope the venue's settings do
//! not publish — a deployed perpetual DEX most of all — is reached through the pair
//! the venue answers its own non-scoped conversations on.

use std::{collections::BTreeMap, fmt, sync::Arc};

use app_config::{
    market_participants::{
        error::MarketParticipantsConfigError,
        origins::{BinanceMarket, OriginPair},
        settings::VenueSettings,
    },
    network::NetworkRouteGeneration,
    settings::Config,
};
use market_participants_model::identifier::{VenueId, VenueScopeId};
use smol_str::SmolStr;
use url::Url;

use crate::routing::CredentialGeneration;

/// Resolved state for one complete credential set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialState<T> {
    /// Every field in the credential set is empty.
    Unconfigured,
    /// Every required field is present; cryptographic validation is deferred.
    Configured(T),
    /// The stored set is missing, mismatched, or partial.
    Invalid(MarketParticipantsConfigError),
}

impl<T> CredentialState<T> {
    /// Returns whether this state holds a set that can sign a request.
    const fn is_configured(&self) -> bool {
        matches!(self, Self::Configured(_))
    }
}

/// Original Binance credential values preserved for client construction.
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

/// Original OKX credential values preserved for client construction.
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

/// Original Hyperliquid credential values preserved for signer construction.
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

/// Original Longbridge credential values preserved for the venue's own requests.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LongbridgeCredentialValues {
    /// The OpenAPI application key.
    pub app_key: SmolStr,
    /// The OpenAPI application secret.
    pub app_secret: SmolStr,
    /// The access token issued for the application.
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

/// What every credential set one venue stores resolves to, as one comparable value.
///
/// The fingerprint follows the venue, never a scope: it covers **every** set the
/// venue holds, so completing a set, emptying one or editing a value in any of them
/// moves the venue's generation. Binance signs each of its markets with its own
/// key, so both of its sets are part of the one venue's credentials; every other
/// venue signs its whole surface with one set, and the set a scope the venue
/// deploys signs with is that same one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CredentialFingerprint {
    /// One resolution per market, in `BinanceMarket::ALL` order.
    Binance([CredentialState<BinanceCredentialValues>; 2]),
    Okx(CredentialState<OkxCredentialValues>),
    Hyperliquid(CredentialState<HyperliquidCredentialValues>),
    /// Longbridge's one stored set, which its own adapter resolves.
    ///
    /// The three values are one credential — the access token is what an
    /// authenticated socket presents, and the key and secret are what it is issued
    /// and re-signed with — so editing any of them moves the venue's generation, and
    /// a socket dialled for one data centre is never reused for another.
    Longbridge(CredentialState<LongbridgeCredentialValues>),
}

impl CredentialFingerprint {
    /// Returns whether any scope of this venue would authenticate with what is stored.
    fn is_configured(&self) -> bool {
        match self {
            Self::Binance(markets) => markets.iter().any(CredentialState::is_configured),
            Self::Okx(state) => state.is_configured(),
            Self::Hyperliquid(state) => state.is_configured(),
            Self::Longbridge(state) => state.is_configured(),
        }
    }
}

#[derive(Clone, Debug)]
struct CredentialRuntime {
    fingerprint: CredentialFingerprint,
    generation: Option<CredentialGeneration>,
}

/// Runtime configuration consumed by market-participant adapters and connection pools.
///
/// The snapshot deliberately contains only the generic network projection and the
/// persisted venue settings. Application settings, UI state, and ConfigManager are
/// outside this crate's boundary.
#[derive(Clone, Debug)]
pub struct MarketParticipantNetworkSnapshot {
    network: Arc<app_config::network::Config>,
    venues: BTreeMap<VenueId, VenueSettings>,
    credentials: BTreeMap<VenueId, CredentialRuntime>,
    /// The next counter value this snapshot may hand out.
    next_generation: u64,
}

impl Default for MarketParticipantNetworkSnapshot {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl MarketParticipantNetworkSnapshot {
    /// Creates an initial runtime snapshot from the persisted application settings.
    pub fn new(config: Config) -> Self {
        Self::from_previous(config, None)
    }

    /// Creates a new snapshot while preserving generations for unchanged values.
    pub fn from_previous(config: Config, previous: Option<&Self>) -> Self {
        let network = Arc::new(config.network_config());
        let mut snapshot = Self {
            network,
            venues: config.venues,
            credentials: BTreeMap::new(),
            next_generation: previous.map_or(1, |previous| previous.next_generation),
        };
        // What a generation is keyed by is read from the configuration the snapshot
        // now holds, so the fingerprint and the credential lookups a request makes
        // are the same answer read twice.
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

    /// Returns the persisted network configuration.
    pub fn network(&self) -> &Arc<app_config::network::Config> {
        &self.network
    }

    /// Returns the settings of one venue this build speaks.
    pub(crate) fn venue_settings(&self, venue: &VenueId) -> Option<&VenueSettings> {
        self.venues.get(venue)
    }

    /// Reports whether one venue holds a credential set complete enough to sign.
    ///
    /// The answer is the venue's own primary scope: a venue that signs its markets apart
    /// is half configured when its primary market is, which is exactly the question the
    /// load gate asks.
    pub(crate) fn credentials_configured(&self, venue: &VenueId) -> bool {
        let Some(settings) = self.venue_settings(venue) else {
            return false;
        };
        matches!(
            settings.validate_credentials(*venue, settings.primary_scope()),
            Ok(true)
        )
    }

    /// Returns the origins one scope of one venue is reached on.
    ///
    /// A scope the venue publishes answers the pair its own settings give it. A
    /// scope the venue *deploys* — a perpetual DEX most of all — is not a settings
    /// key and never becomes one: it is reached through
    /// [`VenueSettings::deployed_origins`], the pair of the scope the venue answers
    /// its own non-scoped conversations on. A venue this build does not configure
    /// answers nothing.
    pub(crate) fn origins(&self, venue: &VenueId, scope: &VenueScopeId) -> Option<OriginPair> {
        let settings = self.venue_settings(venue)?;
        Some(
            settings
                .origins(scope.as_str())
                .unwrap_or_else(|| settings.deployed_origins()),
        )
    }

    /// Returns whether one venue still resolves the origin a connection dialled.
    ///
    /// This is the venue half of "is this connection still current": a connection
    /// opened through an origin the venue's settings no longer give its scope is
    /// stale. The comparison is by origin rather than by URL, because an adapter
    /// dials the configured origin with its own path appended, and the role is read
    /// off the dialled URL's scheme: a `ws`/`wss` connection was dialled on the
    /// WebSocket origin and any other scheme on the HTTP one. Comparing against
    /// both would answer "still served" for a socket whose WebSocket origin moved
    /// to the HTTP field, which is exactly the reconfiguration a pool has to
    /// notice.
    pub(crate) fn serves(&self, venue: &VenueId, scope: &VenueScopeId, dialled: &Url) -> bool {
        self.origins(venue, scope).is_some_and(|origins| {
            let configured = if is_websocket(dialled) {
                &origins.websocket
            } else {
                &origins.http
            };
            configured.origin() == dialled.origin()
        })
    }

    /// Returns the proxy generation of one venue's routing decision.
    pub(crate) fn network_route_generation(&self, venue: &VenueId) -> NetworkRouteGeneration {
        self.network.network_route_generation(venue)
    }

    /// Returns the credential generation of one scope of one venue.
    ///
    /// The generation is the venue's own — one counter for every set it stores — but
    /// it is only answered for a scope whose own set can sign: the scope is mapped
    /// to the set that signs it before anything is read, so a scope the venue
    /// deploys answers the generation of the scope this build routes it through,
    /// while a set that is empty or only partly filled answers none at all.
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

    /// Resolves the Binance credentials one scope signs with.
    ///
    /// Binance signs each of its markets with its own set, so the scope names the set
    /// to read. A scope Binance does not publish is a product this venue holds no key
    /// for, and it answers nothing rather than another market's key. A venue row of
    /// another kind answers nothing either: credentials are typed per venue, and
    /// presenting one venue's keys to another host is the one thing a credential
    /// lookup may never do.
    pub(crate) fn resolve_binance_credentials(
        &self,
        venue: &VenueId,
        scope: &VenueScopeId,
    ) -> CredentialState<BinanceCredentialValues> {
        let Some(VenueSettings::Binance(settings)) = self.venue_settings(venue) else {
            return CredentialState::Unconfigured;
        };
        let Some(market) = binance_market(scope.as_str()) else {
            return CredentialState::Unconfigured;
        };
        let stored = settings.credentials.market(market.credential_market());
        match stored.validate(*venue, Some(market.scope())) {
            Ok(false) => CredentialState::Unconfigured,
            Err(error) => CredentialState::Invalid(error),
            Ok(true) => CredentialState::Configured(BinanceCredentialValues {
                api_key: SmolStr::new(stored.api_key),
                private_key: SmolStr::new(stored.private_key),
                passphrase: (!stored.passphrase.trim().is_empty())
                    .then(|| SmolStr::new(stored.passphrase)),
            }),
        }
    }

    /// Resolves the OKX credentials one scope signs with.
    ///
    /// OKX authenticates its whole surface with one set, so the scope is mapped to
    /// the one whose credentials sign it: a scope the venue deploys signs with the
    /// same keys as the scope this build routes it through.
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
            Ok(true) => CredentialState::Configured(OkxCredentialValues {
                api_key: okx.credentials.api_key.clone(),
                secret_key: okx.credentials.secret_key.clone(),
                passphrase: okx.credentials.passphrase.clone(),
            }),
        }
    }

    /// Resolves the Hyperliquid credentials one scope signs with.
    ///
    /// HyperCore authenticates its whole surface with one set, so the scope is mapped
    /// to the one whose credentials sign it: a scope the venue deploys signs with the
    /// same keys as the scope this build routes it through.
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
            Ok(true) => CredentialState::Configured(HyperliquidCredentialValues {
                main_account_address: hypercore.credentials.main_account_address.clone(),
                api_agent_private_key: hypercore.credentials.api_agent_private_key.clone(),
            }),
        }
    }

    /// Resolves the Longbridge credentials one scope authenticates with.
    ///
    /// The venue's one socket session is authenticated by the credential set its
    /// region belongs to, so the scope is mapped to the one whose credentials
    /// authenticate it: the region the settings hold decides the endpoints, and the
    /// three values are what the socket carries to them.
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
            Ok(true) => CredentialState::Configured(LongbridgeCredentialValues {
                app_key: longbridge.credentials.app_key.clone(),
                app_secret: longbridge.credentials.app_secret.clone(),
                access_token: longbridge.credentials.access_token.clone(),
            }),
        }
    }

    /// Resolves what one venue's stored credentials are, as one comparable value.
    ///
    /// A venue that authenticates as a whole is read through its primary scope: every
    /// other scope it serves — one it deploys most of all — signs with that very set,
    /// so the primary scope's set is the whole venue's.
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
        }
    }
}

/// Answers whether one dialled URL is a WebSocket one.
fn is_websocket(dialled: &Url) -> bool {
    matches!(dialled.scheme(), "ws" | "wss")
}

/// Returns the Binance market one scope addresses, or nothing for a scope Binance
/// does not publish.
fn binance_market(scope: &str) -> Option<BinanceMarket> {
    BinanceMarket::ALL
        .into_iter()
        .find(|market| market.scope() == scope)
}

/// One statically published scope name, as the routing identity a request addresses.
fn published_scope(name: &str) -> VenueScopeId {
    VenueScopeId::new(name).expect("a statically published scope name is canonical")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use app_config::{
        market_participants::{
            credentials::{LongbridgeCredentials, OkxCredentials},
            origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
            settings::{BinanceOrigins, OkxSettings, VenueSettings, default_venues},
        },
        network::{NetworkRouteGeneration, NetworkRuntimeGenerations, ProxyRoute},
        settings::Config,
    };
    use market_participants_model::identifier::{VenueId, VenueScopeId};
    use smol_str::SmolStr;

    use super::{CredentialGeneration, MarketParticipantNetworkSnapshot};
    use crate::routing::{
        CORE_PERP_SCOPE, LONGBRIDGE_US_SCOPE, PERPETUAL_SCOPE, SPOT_SCOPE, SWAP_SCOPE,
    };

    fn scope(name: &str) -> VenueScopeId {
        VenueScopeId::new(name).expect("the scope should be canonical")
    }

    /// Writes one complete Binance spot credential set into the configuration.
    fn binance_spot(config: &mut Config, api_key: &str, private_key: &str) {
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&VenueId::Binance)
        else {
            panic!("binance is configured as Binance by the built-in table");
        };
        settings.credentials.spot_api_key = SmolStr::new(api_key);
        settings.credentials.spot_private_key = SmolStr::new(private_key);
    }

    /// Writes one complete Binance USD-M perpetual credential set into the configuration.
    fn binance_perpetual(config: &mut Config, api_key: &str, private_key: &str) {
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&VenueId::Binance)
        else {
            panic!("binance is configured as Binance by the built-in table");
        };
        settings.credentials.usd_m_perp_api_key = SmolStr::new(api_key);
        settings.credentials.usd_m_perp_private_key = SmolStr::new(private_key);
    }

    /// Writes one complete OKX credential set into the configuration.
    fn okx(config: &mut Config, api_key: &str) {
        let Some(VenueSettings::Okx(settings)) = config.venues.get_mut(&VenueId::Okx) else {
            panic!("okx is configured as OKX by the built-in table");
        };
        settings.credentials = OkxCredentials {
            api_key: SmolStr::new(api_key),
            secret_key: SmolStr::new_static("okx-secret"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        };
    }

    /// Writes one complete HyperCore credential set into the configuration.
    fn hyperliquid(config: &mut Config, address: &str, private_key: &str) {
        let Some(VenueSettings::Hyperliquid(settings)) =
            config.venues.get_mut(&VenueId::Hyperliquid)
        else {
            panic!("hyperliquid is configured as HyperCore by the built-in table");
        };
        settings.credentials.main_account_address = SmolStr::new(address);
        settings.credentials.api_agent_private_key = SmolStr::new(private_key);
    }

    /// Writes one Longbridge credential set into the configuration.
    fn longbridge_credentials(config: &mut Config, app_key: &str, app_secret: &str, token: &str) {
        let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&VenueId::Longbridge)
        else {
            panic!("longbridge is configured as Longbridge by the built-in table");
        };
        settings.credentials = LongbridgeCredentials {
            app_key: SmolStr::new(app_key),
            app_secret: SmolStr::new(app_secret),
            access_token: SmolStr::new(token),
        };
    }

    /// The Binance spot market's own second address choice.
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

    /// A scope the venue deploys is reached through the venue's own primary scope,
    /// never through a pair of its own: the settings name no scope for it.
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

    /// One venue's generation covers every set it stores, and it is answered per
    /// scope: Binance signs each market apart, so filling in the market the operator
    /// had not started moves the venue's generation and only then answers for that
    /// market.
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

    /// One complete Longbridge credential set resolves, and it answers the venue's
    /// generation: the three values are the credential its socket authenticates with,
    /// so a set that is complete is a set this build can dial.
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

    /// Editing any one of the three Longbridge values moves the venue's generation.
    ///
    /// The generation is what a connection key carries, so a value that moved without
    /// moving the key would let a socket dialled under the old credentials answer for
    /// the new ones. All three values are edited one at a time, because all three are
    /// the one credential: the region `x-dc-region` is derived from is a prefix of any
    /// of them.
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

    /// A half-filled Longbridge set fails closed: it resolves as invalid rather than
    /// as the two values that happen to be present.
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

    /// A venue whose set is only partly filled answers nothing, and it fails closed
    /// rather than falling back to whatever half of the set happens to be present.
    #[test]
    fn a_partially_configured_venue_has_no_generation() {
        let okx_venue = VenueId::Okx;
        let mut config = Config::default();
        let Some(VenueSettings::Okx(settings)) = config.venues.get_mut(&okx_venue) else {
            panic!("okx is configured as OKX by the built-in table");
        };
        settings.credentials.api_key = SmolStr::new_static("okx-key");

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

    /// A venue row holding another kind of credential answers nothing for the kind it
    /// is not: credentials are typed per venue, and one venue's keys are never
    /// presented to another venue's host.
    #[test]
    fn a_venue_holding_another_kinds_credentials_answers_nothing() {
        let binance = VenueId::Binance;
        let mut config = Config::default();
        config.venues.insert(
            binance,
            VenueSettings::Okx(OkxSettings {
                credentials: OkxCredentials {
                    api_key: SmolStr::new_static("okx-key"),
                    secret_key: SmolStr::new_static("okx-secret"),
                    passphrase: SmolStr::new_static("okx-passphrase"),
                },
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

    /// A venue this build configures nothing for answers nothing at all: no origins,
    /// no generation, no credentials — and nothing it dials is served.
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

    /// Credentials written as Hyperliquid ones are read as Hyperliquid ones, and a
    /// scope the venue deploys signs with the very same set.
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

    /// A dialled origin is served by the venue that configures it, for the role the
    /// dialled URL plays and for the scope it was dialled under. A venue this build
    /// configures nothing for serves nothing.
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

    /// The built-in table is what every other fixture starts from, so a snapshot of it
    /// answers every venue this build speaks.
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
