use std::collections::BTreeMap;

use market_participants_model::identifier::VenueId;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::SerializeSeq};
use smol_str::SmolStr;
use thiserror::Error;

pub(crate) use crate::network::{ProxyConfig, ProxyRoute, ServiceTarget};
use crate::{
    change::ConfigChangeSet,
    market_participants::{
        origins::{
            BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, HyperliquidHttpOrigin,
            HyperliquidWebsocketOrigin, LongbridgeRegion, OkxRegion,
        },
        settings::{BinanceOrigins, VenueSettings, default_venues, deserialize_venues},
    },
    network::{Config as NetworkConfig, NetworkRouteGeneration, NetworkRuntimeGenerations},
    time_zone::DataTimeZone,
    workspace::WorkspacesConfig,
};

/// Wallet account currently associated with this installation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub user_id: String,
    pub wallet_address: String,
    pub wallet_kind: AccountWalletKind,
}

/// Wallet family that owns [`AccountConfig::wallet_address`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountWalletKind {
    Ethereum,
    Solana,
}

/// Application display language.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::ZhCn, Self::En];

    pub const fn locale(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }
}

/// Application color appearance preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Appearance {
    /// Always use the light style.
    Light,
    /// Always use the dark style.
    Dark,
    /// Follow the operating system window appearance.
    #[default]
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Complete application settings snapshot.
pub struct Config {
    /// Application color appearance preference.
    #[serde(default)]
    pub appearance: Appearance,
    /// Application display language.
    #[serde(default)]
    pub language: Language,
    /// The time zone every data timestamp is displayed in.
    #[serde(default)]
    pub data_time_zone: DataTimeZone,
    /// Wallet account currently associated with this installation.
    #[serde(default)]
    pub account: Option<AccountConfig>,
    /// DiceBear SVG for the current wallet account.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Named SOCKS5 proxies available to network capability settings.
    pub proxies: BTreeMap<SmolStr, ProxyConfig>,
    /// Default outbound used by targets without an override.
    pub proxy_default: ProxyRoute,
    /// Per-target outbound overrides. Absent keys follow [`Self::proxy_default`].
    #[serde(
        serialize_with = "serialize_proxy_applications",
        deserialize_with = "deserialize_proxy_applications"
    )]
    pub proxy_applications: BTreeMap<ServiceTarget, ProxyRoute>,
    /// Every configured venue, keyed by venue.
    ///
    /// Required, and complete: a file that does not name every venue this build
    /// speaks is not this shape and is refused whole at decode time, so a reader
    /// never has to answer what a missing venue means. The same holds for the
    /// scopes a venue's own row names: they must be the scopes this build addresses
    /// statically.
    #[serde(rename = "venues", deserialize_with = "deserialize_venues")]
    pub venues: BTreeMap<VenueId, VenueSettings>,
    /// Workspaces of this installation. A file without them has none, which is
    /// what a first start looks like.
    #[serde(default)]
    pub workspaces: WorkspacesConfig,
    #[serde(skip)]
    network_generations: NetworkRuntimeGenerations,
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        self.appearance == other.appearance
            && self.language == other.language
            && self.data_time_zone == other.data_time_zone
            && self.account == other.account
            && self.avatar == other.avatar
            && self.proxies == other.proxies
            && self.proxy_default == other.proxy_default
            && self.proxy_applications == other.proxy_applications
            && self.venues == other.venues
            && self.workspaces == other.workspaces
    }
}

impl Eq for Config {}

#[derive(Serialize, Deserialize)]
struct ProxyApplication {
    target: ServiceTarget,
    route: ProxyRoute,
}

fn serialize_proxy_applications<S>(
    applications: &BTreeMap<ServiceTarget, ProxyRoute>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut sequence = serializer.serialize_seq(Some(applications.len()))?;
    for (target, route) in applications {
        sequence.serialize_element(&ProxyApplication {
            target: target.clone(),
            route: route.clone(),
        })?;
    }
    sequence.end()
}

fn deserialize_proxy_applications<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<ServiceTarget, ProxyRoute>, D::Error>
where
    D: Deserializer<'de>,
{
    let applications = Vec::<ProxyApplication>::deserialize(deserializer)?;
    let mut decoded = BTreeMap::new();
    for application in applications {
        let ProxyApplication { target, route } = application;
        if decoded.insert(target.clone(), route).is_some() {
            return Err(D::Error::custom(format_args!(
                "duplicate proxy application target: {target}"
            )));
        }
    }
    Ok(decoded)
}

/// A proxy replacement could not preserve the proxy catalog's invariants.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReplaceProxyError {
    /// The proxy being replaced is absent from the catalog.
    #[error("proxy {name:?} does not exist")]
    SourceMissing { name: SmolStr },
    /// Another proxy already owns the requested replacement name.
    #[error("proxy {name:?} already exists")]
    TargetExists { name: SmolStr },
    /// The requested name collides with a reserved route token.
    #[error("proxy {name:?} uses a reserved name")]
    ReservedName { name: SmolStr },
}

/// Route-value prefixes reserved for settings encoding (`@inherit`, `@direct`, …).
pub fn proxy_name_is_reserved(name: &str) -> bool {
    name.starts_with('@')
}

impl Config {
    /// Replaces the runtime route generations stamped onto this snapshot.
    ///
    /// Only the settings manager stamps generations; every other writer leaves
    /// them untouched so unchanged routing keeps its resolved connections.
    pub fn set_network_generations(&mut self, generations: NetworkRuntimeGenerations) {
        self.network_generations = generations;
    }

    /// Creates the network-only view of this application snapshot.
    pub fn network_config(&self) -> NetworkConfig {
        let route_generations = self.network_generations.venues.clone();
        let service_routes = self
            .proxy_applications
            .iter()
            .map(|(target, route)| (target.clone(), route.clone()))
            .collect();
        NetworkConfig {
            proxies: self.proxies.clone(),
            proxy_default: self.proxy_default.clone(),
            proxy_applications: service_routes,
            // A venue that names no line of its own is left out rather than
            // pinned to a placeholder route: the network snapshot answers the
            // default line for exactly the venues it holds no route for.
            venue_routes: self
                .venues
                .iter()
                .filter_map(|(venue, config)| config.proxy().map(|route| (*venue, route)))
                .collect(),
            default_route_generation: self.network_generations.default_route,
            route_generations,
            service_route_generations: self.network_generations.services.clone(),
        }
    }

    /// Returns one venue's settings, or nothing for a venue this build does not
    /// speak.
    ///
    /// A venue this build *does* speak always answers: the decoded shape covers the
    /// built-in list by construction, so absence here means "this build does not
    /// speak that venue" rather than a gap a caller has to defend against. Nothing is
    /// defaulted in: a caller that wanted the built-in table asks
    /// [`crate::market_participants::settings::default_venues`].
    pub fn venue(&self, venue: &VenueId) -> Option<&VenueSettings> {
        self.venues.get(venue)
    }

    /// Replaces the outbound line one venue's requests take; `None` makes the venue
    /// follow the document's default line again.
    ///
    /// Returns whether the venue is configured at all: a venue this build does not
    /// speak has no row to edit, and editing one is not this shape.
    pub fn set_venue_proxy(&mut self, venue: &VenueId, proxy: Option<ProxyRoute>) -> bool {
        let Some(settings) = self.venues.get_mut(venue) else {
            return false;
        };
        settings.set_proxy(proxy);
        true
    }

    /// Replaces the region an OKX account belongs to.
    ///
    /// Returns whether the venue is configured as OKX at all: the region is a
    /// property of that venue alone, so a row of another shape has nowhere to put
    /// one.
    pub fn set_okx_region(&mut self, venue: &VenueId, region: OkxRegion) -> bool {
        let Some(VenueSettings::Okx(settings)) = self.venues.get_mut(venue) else {
            return false;
        };
        settings.region = region;
        true
    }

    /// Replaces the region a Longbridge account belongs to.
    ///
    /// Returns whether the venue is configured as Longbridge at all: the region is a
    /// property of that venue alone, so a row of another shape has nowhere to put
    /// one.
    pub fn set_longbridge_region(&mut self, venue: &VenueId, region: LongbridgeRegion) -> bool {
        let Some(VenueSettings::Longbridge(settings)) = self.venues.get_mut(venue) else {
            return false;
        };
        settings.region = region;
        true
    }

    /// Replaces one Binance market's origin choices.
    ///
    /// Returns whether the venue holds a Binance row that publishes `market` *and* both
    /// choices are ones `market` itself is reached on: the choices belong to that market
    /// alone, so a row of another shape, a market the row does not configure, or its
    /// sibling's host has nowhere to put them. A choice its own market does not publish is
    /// refused here for the same reason a file naming one is refused at decode — a spot
    /// request sent to the futures API is a request to the wrong product.
    pub fn set_binance_market_origins(
        &mut self,
        venue: &VenueId,
        market: BinanceMarket,
        origins: &BinanceOrigins,
    ) -> bool {
        if !BinanceHttpOrigin::for_market(market).contains(&origins.http)
            || !BinanceWebsocketOrigin::for_market(market).contains(&origins.websocket)
        {
            return false;
        }
        let Some(VenueSettings::Binance(settings)) = self.venues.get_mut(venue) else {
            return false;
        };
        let Some(configured) = settings.origins.get_mut(market.scope()) else {
            return false;
        };
        configured.clone_from(origins);
        true
    }

    /// Replaces the address HyperCore is reached on.
    ///
    /// Returns whether the venue holds a HyperCore row at all: the address belongs
    /// to that venue alone.
    pub fn set_hyperliquid_origins(
        &mut self,
        venue: &VenueId,
        http: HyperliquidHttpOrigin,
        websocket: HyperliquidWebsocketOrigin,
    ) -> bool {
        let Some(VenueSettings::Hyperliquid(settings)) = self.venues.get_mut(venue) else {
            return false;
        };
        settings.http = http;
        settings.websocket = websocket;
        true
    }

    /// Replaces one credential value of one venue.
    ///
    /// `field` is the wire name the credential set stores, and the write lands in
    /// the venue's own shape: a venue that holds a different kind of credential
    /// stores nothing, because a credential set is a value of exactly one venue's
    /// shape and there is no second place to put it. Returns whether the write
    /// landed.
    pub fn set_venue_credential(&mut self, venue: &VenueId, field: &str, value: &str) -> bool {
        let Some(settings) = self.venues.get_mut(venue) else {
            return false;
        };
        write_credential(settings, field, value)
    }

    /// Returns the process-local generation of one venue's proxy decision.
    pub fn network_route_generation(&self, venue: &VenueId) -> NetworkRouteGeneration {
        self.network_generations
            .venues
            .get(venue)
            .copied()
            .unwrap_or(self.network_generations.default_route)
    }

    /// Removes a proxy and every named binding to its name.
    ///
    /// A venue that named it is a binding like any other: leaving one pointing at
    /// a removed entry would fail every request of that venue, which is what the
    /// reset to the default line avoids.
    pub fn remove_proxy(&mut self, name: &str) -> Option<ProxyConfig> {
        let removed = self.proxies.remove(name)?;

        self.proxy_applications
            .retain(|_, route| route.named_name() != Some(name));
        if self.proxy_default.named_name() == Some(name) {
            self.proxy_default = ProxyRoute::Direct;
        }
        for venue in self.venues.values_mut() {
            if venue
                .proxy()
                .and_then(|route| route.named_name().map(SmolStr::new))
                .as_deref()
                == Some(name)
            {
                venue.set_proxy(None);
            }
        }
        Some(removed)
    }

    /// Replaces a proxy and, when renamed, updates every named route binding.
    ///
    /// The source and target names are validated before any mutation, so an
    /// error leaves the complete configuration unchanged.
    pub fn replace_proxy(
        &mut self,
        old_name: &str,
        new_name: SmolStr,
        proxy: ProxyConfig,
    ) -> Result<(), ReplaceProxyError> {
        if !self.proxies.contains_key(old_name) {
            return Err(ReplaceProxyError::SourceMissing {
                name: SmolStr::new(old_name),
            });
        }

        let renamed = old_name != new_name.as_str();
        if renamed && self.proxies.contains_key(new_name.as_str()) {
            return Err(ReplaceProxyError::TargetExists { name: new_name });
        }
        if proxy_name_is_reserved(new_name.as_str()) {
            return Err(ReplaceProxyError::ReservedName { name: new_name });
        }

        if renamed {
            let _ = self.proxies.remove(old_name);
        }
        self.proxies.insert(new_name.clone(), proxy);

        if renamed {
            self.proxy_default.rename_catalog_proxy(old_name, &new_name);
            for route in self.proxy_applications.values_mut() {
                route.rename_catalog_proxy(old_name, &new_name);
            }
            for venue in self.venues.values_mut() {
                if let Some(route) = venue.proxy_mut() {
                    route.rename_catalog_proxy(old_name, &new_name);
                }
            }
        }

        Ok(())
    }

    /// Effective outbound choice for one Kytrix service after applying the
    /// default line.
    ///
    /// Named for its subject rather than sharing a name with the venue-keyed
    /// accessor of the network projection: a service target is routed by
    /// `proxy_applications`, a venue by the line its own configuration names or by
    /// the default line it names none of, and that answer is read through
    /// [`Self::network_config`].
    pub fn service_proxy_route_for(&self, target: &ServiceTarget) -> ProxyRoute {
        self.proxy_applications
            .get(target)
            .cloned()
            .unwrap_or_else(|| self.proxy_default.clone())
    }

    /// Returns every configuration item `previous` and `self` disagree on.
    ///
    /// The comparison follows the hand-written `PartialEq` of every nested
    /// settings type: the runtime generations are not settings, and the
    /// test-support `bindings` of a venue row is not settings either. The
    /// workspaces are not reported because nothing outside their single writer
    /// reads them.
    ///
    /// The destructuring is exhaustive on purpose: a field added to [`Config`]
    /// is a compile error here until it is classified as reported or ignored.
    pub fn changes_since(&self, previous: &Self) -> ConfigChangeSet {
        let Config {
            appearance: previous_appearance,
            language: previous_language,
            data_time_zone: previous_data_time_zone,
            account: previous_account,
            avatar: previous_avatar,
            proxies: previous_proxies,
            proxy_default: previous_proxy_default,
            proxy_applications: previous_proxy_applications,
            venues: previous_venues,
            // The workspaces have one writer and no external reader: a layout
            // write must not wake a single subscriber. The field is still named
            // rather than matched with `_`, which the workspace forbids.
            workspaces: WorkspacesConfig { .. },
            // Generations are runtime identity, not settings: they never take
            // part in a diff (see `PartialEq for Config`).
            network_generations: NetworkRuntimeGenerations { .. },
        } = previous;

        let mut changes = ConfigChangeSet {
            proxies: Vec::with_capacity(previous_proxies.len().saturating_add(self.proxies.len())),
            proxy_applications: Vec::with_capacity(
                previous_proxy_applications
                    .len()
                    .saturating_add(self.proxy_applications.len()),
            ),
            venues: Vec::with_capacity(previous_venues.len().saturating_add(self.venues.len())),
            ..ConfigChangeSet::default()
        };

        if *previous_appearance != self.appearance {
            changes.appearance = Some(self.appearance);
        }
        if *previous_language != self.language {
            changes.language = Some(self.language);
        }
        if *previous_data_time_zone != self.data_time_zone {
            changes.data_time_zone = Some(self.data_time_zone);
        }
        if previous_account != &self.account || previous_avatar != &self.avatar {
            changes.account_changed = true;
        }
        for (name, proxy) in &self.proxies {
            if previous_proxies.get(name) != Some(proxy) {
                changes.proxies.push(name.clone());
            }
        }
        for name in previous_proxies.keys() {
            if !self.proxies.contains_key(name) {
                changes.proxies.push(name.clone());
            }
        }
        if previous_proxy_default != &self.proxy_default {
            changes.proxy_default_changed = true;
        }
        for (target, route) in &self.proxy_applications {
            if previous_proxy_applications.get(target) != Some(route) {
                changes.proxy_applications.push(target.clone());
            }
        }
        for target in previous_proxy_applications.keys() {
            if !self.proxy_applications.contains_key(target) {
                changes.proxy_applications.push(target.clone());
            }
        }
        for (venue, settings) in &self.venues {
            if previous_venues.get(venue) != Some(settings) {
                changes.venues.push(*venue);
            }
        }
        for venue in previous_venues.keys() {
            if !self.venues.contains_key(venue) {
                changes.venues.push(*venue);
            }
        }

        changes
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            language: Language::default(),
            data_time_zone: DataTimeZone::default(),
            account: None,
            avatar: None,
            proxies: BTreeMap::new(),
            proxy_default: ProxyRoute::Direct,
            proxy_applications: BTreeMap::new(),
            venues: default_venues(),
            workspaces: WorkspacesConfig::default(),
            network_generations: NetworkRuntimeGenerations::default(),
        }
    }
}

/// Writes one credential value into the row of the venue it belongs to.
///
/// `field` is the name the credential set stores the value under, so the write lands
/// in the venue's own shape and nowhere else: a field of another venue's credentials
/// is not a value this row has, and writing it would be a credential the venue's
/// adapters never read. Returns whether the write landed.
fn write_credential(settings: &mut VenueSettings, field: &str, value: &str) -> bool {
    let value = || SmolStr::new(value);
    match settings {
        VenueSettings::Binance(binance) => match field {
            "spot_api_key" => binance.credentials.spot_api_key = value(),
            "spot_private_key" => binance.credentials.spot_private_key = value(),
            "spot_passphrase" => binance.credentials.spot_passphrase = value(),
            "usd_m_perp_api_key" => binance.credentials.usd_m_perp_api_key = value(),
            "usd_m_perp_private_key" => binance.credentials.usd_m_perp_private_key = value(),
            "usd_m_perp_passphrase" => binance.credentials.usd_m_perp_passphrase = value(),
            _ => return false,
        },
        VenueSettings::Okx(okx) => match field {
            "api_key" => okx.credentials.api_key = value(),
            "secret_key" => okx.credentials.secret_key = value(),
            "passphrase" => okx.credentials.passphrase = value(),
            _ => return false,
        },
        VenueSettings::Hyperliquid(hyperliquid) => match field {
            "main_account_address" => hyperliquid.credentials.main_account_address = value(),
            "api_agent_private_key" => hyperliquid.credentials.api_agent_private_key = value(),
            _ => return false,
        },
        VenueSettings::Longbridge(longbridge) => match field {
            "app_key" => longbridge.credentials.app_key = value(),
            "app_secret" => longbridge.credentials.app_secret = value(),
            "access_token" => longbridge.credentials.access_token = value(),
            _ => return false,
        },
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use market_participants_model::identifier::VenueId;
    use serde_json::json;
    use smol_str::SmolStr;

    use crate::{
        change::ConfigChangeSet,
        market_participants::{
            credentials::{
                BinanceCredentials, HyperliquidCredentials, LongbridgeCredentials, OkxCredentials,
            },
            error::MarketParticipantsConfigError,
            origins::{
                BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, HyperliquidHttpOrigin,
                HyperliquidWebsocketOrigin, LongbridgeRegion, OkxRegion,
            },
            settings::{
                BINANCE_SPOT, BINANCE_USD_M_PERP, BinanceOrigins, HYPERLIQUID_CORE_PERP,
                LONGBRIDGE_US, OKX_SPOT, OKX_SWAP, VenueSettings, default_venues,
            },
        },
        network::{
            KytrixServiceCapability, NetworkRouteGeneration, NetworkRuntimeGenerations,
            ProxyAuthorizationConfig, ProxyConfig, ProxyRoute, ServiceTarget,
        },
        settings::{
            AccountConfig, AccountWalletKind, Appearance, Config, Language, ReplaceProxyError,
        },
        time_zone::DataTimeZone,
        workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
    };

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the venue should be canonical")
    }

    /// A settings snapshot whose venue map holds no row at all.
    ///
    /// The decoded shape covers every venue this build speaks, so the only way a
    /// venue has no row is a configuration that was taken apart by hand — which is
    /// what a writer has to defend against.
    fn without_rows() -> Config {
        let mut config = Config::default();
        config.venues.clear();
        config
    }

    /// The name of the line one venue names for itself, or `None` while it names
    /// none — and therefore follows the default line.
    fn venue_named_route(config: &Config, venue: &VenueId) -> Option<SmolStr> {
        let route = config.venue(venue)?.proxy()?;
        route.named_name().map(SmolStr::new)
    }
    fn x_media_target() -> ServiceTarget {
        ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::XMedia,
        }
    }

    /// Runtime generations that differ from [`NetworkRuntimeGenerations::default`]
    /// in every field.
    fn different_generations() -> NetworkRuntimeGenerations {
        NetworkRuntimeGenerations {
            default_route: NetworkRouteGeneration::new(7),
            venues: BTreeMap::from([(venue("binance"), NetworkRouteGeneration::new(8))]),
            services: BTreeMap::from([(x_media_target(), NetworkRouteGeneration::new(9))]),
        }
    }

    fn local_proxy() -> ProxyConfig {
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port: 1080,
            authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                username: SmolStr::new_static("proxy-user"),
                password: SmolStr::new_static("proxy-password"),
            },
            resolve_dns_at_remote: true,
        }
    }

    fn replacement_proxy() -> ProxyConfig {
        ProxyConfig {
            addr: SmolStr::new_static("10.0.0.1"),
            port: 2080,
            authorization: ProxyAuthorizationConfig::None,
            resolve_dns_at_remote: false,
        }
    }

    /// The configured venue section: every venue with the addresses it is reached on
    /// and, where an operator wrote them, its credentials.
    fn configured_venues() -> BTreeMap<VenueId, VenueSettings> {
        let mut venues = default_venues();
        {
            let VenueSettings::Binance(binance) = venues
                .get_mut(&venue("binance"))
                .expect("binance is a built-in venue")
            else {
                panic!("binance is configured as Binance");
            };
            binance.credentials = BinanceCredentials {
                spot_api_key: SmolStr::new_static("binance-spot-key"),
                spot_private_key: SmolStr::new_static("binance-spot-private"),
                spot_passphrase: SmolStr::new_static("binance-spot-passphrase"),
                usd_m_perp_api_key: SmolStr::new_static("binance-perp-key"),
                usd_m_perp_private_key: SmolStr::new_static("binance-perp-private"),
                usd_m_perp_passphrase: SmolStr::new_static("binance-perp-passphrase"),
            };
        }
        {
            let VenueSettings::Okx(okx) = venues
                .get_mut(&venue("okx"))
                .expect("okx is a built-in venue")
            else {
                panic!("okx is configured as OKX");
            };
            okx.credentials = OkxCredentials {
                api_key: SmolStr::new_static("okx-api-key"),
                secret_key: SmolStr::new_static("okx-secret-key"),
                passphrase: SmolStr::new_static("okx-passphrase"),
            };
            // An operator-chosen region: the shape the settings interface edits.
            okx.region = OkxRegion::Europe;
        }
        {
            let VenueSettings::Hyperliquid(hyperliquid) = venues
                .get_mut(&venue("hyperliquid"))
                .expect("hyperliquid is a built-in venue")
            else {
                panic!("hyperliquid is configured as HyperCore");
            };
            hyperliquid.credentials = HyperliquidCredentials {
                main_account_address: SmolStr::new_static(" 0xmain-account "),
                api_agent_private_key: SmolStr::new_static(" agent-private-key "),
            };
        }
        {
            let VenueSettings::Longbridge(longbridge) = venues
                .get_mut(&venue("longbridge"))
                .expect("longbridge is a built-in venue")
            else {
                panic!("longbridge is configured as Longbridge");
            };
            longbridge.credentials = LongbridgeCredentials {
                app_key: SmolStr::new_static("longbridge-app-key"),
                app_secret: SmolStr::new_static("longbridge-app-secret"),
                access_token: SmolStr::new_static("longbridge-access-token"),
            };
            // An operator-chosen region: the shape the settings interface edits.
            longbridge.region = LongbridgeRegion::China;
        }
        venues
    }

    fn configured_settings() -> Config {
        let mut config = Config {
            account: Some(AccountConfig {
                user_id: "11111111-1111-4111-8111-111111111111".into(),
                wallet_address: "0x0123456789abcdef".into(),
                wallet_kind: AccountWalletKind::Ethereum,
            }),
            avatar: Some("<svg>configured-avatar</svg>".to_owned()),
            ..Config::default()
        };
        config
            .proxies
            .insert(SmolStr::new_static("local"), local_proxy());
        config
            .proxy_applications
            .extend([(x_media_target(), ProxyRoute::named("local"))]);
        config.venues = configured_venues();

        config
    }

    /// A fresh installation configures every venue this build speaks, on the
    /// addresses the venues publish and with no credentials: the venue list is an
    /// invariant of the shape rather than something the operator writes first.
    #[test]
    fn default_should_configure_every_built_in_venue() {
        let config = Config::default();
        assert_eq!(config.venues, default_venues());
        assert_eq!(
            config
                .venues
                .keys()
                .map(|venue| venue.as_str())
                .collect::<Vec<_>>(),
            ["binance", "hyperliquid", "longbridge", "okx"],
            "a fresh installation covers every venue this build speaks"
        );
        assert_eq!(
            config
                .venue(&venue("binance"))
                .map(VenueSettings::scope_ids),
            Some(&[BINANCE_SPOT, BINANCE_USD_M_PERP][..]),
            "and names the scopes Binance publishes"
        );
        assert_eq!(
            config
                .venue(&venue("binance"))
                .expect("binance is configured")
                .validate_credentials(venue("binance"), BINANCE_SPOT),
            Ok(false),
            "an installation that wrote no credentials is unconfigured, not invalid"
        );
    }

    /// The venue list is required: a configuration that omits a venue this build
    /// speaks is not this shape, and is refused rather than completed.
    #[test]
    fn a_configuration_without_venues_is_refused() {
        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("venues");

        assert!(
            serde_json::from_value::<Config>(json).is_err(),
            "a file without venues is not this shape"
        );
    }

    /// One venue dropped from an otherwise complete file is refused too: the
    /// invariant is per venue, not "the key exists".
    #[test]
    fn a_configuration_missing_a_built_in_venue_is_refused() {
        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        json["venues"]
            .as_object_mut()
            .expect("venues serializes as an object")
            .remove("okx");

        let refused = serde_json::from_value::<Config>(json);
        assert!(
            refused.is_err(),
            "a file that does not name every venue this build speaks is refused whole"
        );
        let message = refused.expect_err("the file is refused").to_string();
        assert!(
            message.contains("okx"),
            "the refusal names the venue that is missing: {message}"
        );
    }

    /// A venue this build does not speak is not this shape either: it would be a
    /// row nothing can route, authenticate or draw.
    #[test]
    fn a_configuration_naming_an_unknown_venue_is_refused() {
        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        let venues = json["venues"]
            .as_object_mut()
            .expect("venues serializes as an object");
        let kraken = venues
            .get("okx")
            .cloned()
            .expect("the okx row is a venue-shaped fixture");
        venues.insert("kraken".to_owned(), kraken);

        assert!(
            serde_json::from_value::<Config>(json).is_err(),
            "a venue this build does not speak is refused whole"
        );
    }

    /// The origin rows of one venue are the scopes that venue publishes, and a row
    /// for a scope this build does not route is refused: a scope the venue deploys
    /// at runtime is never a configuration key.
    #[test]
    fn a_configuration_naming_a_scope_the_build_does_not_address_is_refused() {
        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        let origins = json["venues"]["binance"]["origins"]
            .as_object_mut()
            .expect("a venue's markets serialize as an object");
        assert!(
            !origins.contains_key("perp-dex/xyz"),
            "this build never writes a deployed scope into the file"
        );
        let span = origins
            .get(BINANCE_SPOT)
            .cloned()
            .expect("the spot market is a row of its own");
        origins.insert("perp-dex/xyz".to_owned(), span);

        let refused = serde_json::from_value::<Config>(json);

        assert!(
            refused.is_err(),
            "a scope this build does not address is not this shape"
        );
        let message = refused.expect_err("the file is refused").to_string();
        assert!(
            message.contains("perp-dex/xyz") && message.contains("usd-m-perp"),
            "the refusal names the offending scope and the ones the build does address: {message}"
        );
    }

    /// One market's address choices are edited apart from its sibling's, and both
    /// keep the address the other market publishes.
    #[test]
    fn a_market_is_configured_apart_from_its_sibling() {
        let mut config = Config::default();
        let binance = venue("binance");
        let spot = BinanceOrigins {
            http: BinanceHttpOrigin::Api3,
            websocket: BinanceWebsocketOrigin::StreamAlt,
        };

        assert!(config.set_binance_market_origins(&binance, BinanceMarket::Spot, &spot));

        let settings = config.venue(&binance).expect("the venue is configured");
        assert_eq!(
            settings.origins(BINANCE_SPOT).map(|o| o.http.to_string()),
            Some("https://api3.binance.com/".to_owned())
        );
        assert_eq!(
            settings
                .origins(BINANCE_USD_M_PERP)
                .map(|o| o.http.to_string()),
            Some("https://fapi.binance.com/".to_owned()),
            "the sibling market keeps the address it publishes"
        );

        // A venue row of another shape has nowhere to put a Binance market's
        // choices, and neither has a venue this build does not speak.
        assert!(!config.set_binance_market_origins(&venue("okx"), BinanceMarket::Spot, &spot));
        let mut without_rows = without_rows();
        assert!(!without_rows.set_binance_market_origins(&binance, BinanceMarket::Spot, &spot));

        // A market reached on its sibling's host is nothing to write: the futures API is
        // not an address the spot API answers on, whatever the venue publishes elsewhere.
        let before = config.venue(&binance).cloned();
        assert!(
            !config.set_binance_market_origins(
                &binance,
                BinanceMarket::Spot,
                &BinanceOrigins {
                    http: BinanceHttpOrigin::FuturesApi,
                    websocket: BinanceWebsocketOrigin::FuturesStream,
                },
            ),
            "a market's choices are the ones its own API publishes"
        );
        assert_eq!(
            config.venue(&binance).cloned(),
            before,
            "and a refused write leaves the market exactly as it was"
        );
    }

    /// The OKX region is one value of the venue's own row: choosing it moves every
    /// scope of the venue, and no address is stored beside it.
    #[test]
    fn an_okx_region_moves_every_scope_of_the_venue() {
        let mut config = Config::default();
        let okx = venue("okx");

        assert!(config.set_okx_region(&okx, OkxRegion::UnitedStatesAustralia));

        let settings = config.venue(&okx).expect("the venue is configured");
        assert_eq!(
            settings.origins(OKX_SPOT).map(|o| o.http.to_string()),
            Some("https://us.okx.com/".to_owned())
        );
        assert_eq!(
            settings.origins(OKX_SWAP).map(|o| o.websocket.to_string()),
            Some("wss://wsus.okx.com/".to_owned()),
            "both scopes are reached through the region the operator selected"
        );

        let serialized = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(
            serialized["venues"]["okx"]["region"],
            json!("us-au"),
            "the file stores the region, not an address"
        );
        assert!(
            serialized["venues"]["okx"].get("origins").is_none(),
            "OKX stores no origin table of its own"
        );

        assert!(!config.set_okx_region(&venue("binance"), OkxRegion::Europe));
    }

    /// The Longbridge region is one value of the venue's own row: choosing it moves the
    /// venue's scope, and no address is stored beside it.
    #[test]
    fn a_longbridge_region_moves_the_scope_of_the_venue() {
        let mut config = Config::default();
        let longbridge = venue("longbridge");

        assert!(config.set_longbridge_region(&longbridge, LongbridgeRegion::China));

        let settings = config.venue(&longbridge).expect("the venue is configured");
        assert_eq!(
            settings.origins(LONGBRIDGE_US).map(|o| o.http.to_string()),
            Some("https://openapi.longbridge.cn/".to_owned())
        );
        assert_eq!(
            settings
                .origins(LONGBRIDGE_US)
                .map(|o| o.websocket.to_string()),
            Some("wss://openapi-quote.longbridge.cn/v2".to_owned()),
            "both roles are the region's own"
        );

        let serialized = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(
            serialized["venues"]["longbridge"]["region"],
            json!("china"),
            "the file stores the region, not an address"
        );
        assert!(
            serialized["venues"]["longbridge"].get("origins").is_none(),
            "Longbridge stores no origin table of its own"
        );

        assert!(!config.set_longbridge_region(&venue("okx"), LongbridgeRegion::Global));
    }

    /// Longbridge's three credential values are written into the venue's own shape and
    /// nowhere else: a field of another venue's credentials is not a value this row has.
    #[test]
    fn a_longbridge_credential_lands_in_the_venue_that_stores_it() {
        let mut config = Config::default();
        let longbridge = venue("longbridge");

        assert!(config.set_venue_credential(&longbridge, "app_key", "app-key"));
        assert!(config.set_venue_credential(&longbridge, "app_secret", "app-secret"));
        assert!(config.set_venue_credential(&longbridge, "access_token", "access-token"));
        assert!(
            !config.set_venue_credential(&longbridge, "api_key", "okx-key"),
            "an OKX field is not a field of Longbridge's shape"
        );

        assert_eq!(
            config
                .venue(&longbridge)
                .expect("the venue is configured")
                .validate_credentials(longbridge, LONGBRIDGE_US),
            Ok(true),
            "the three values the venue signs with are one complete set"
        );

        let serialized = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(
            serialized["venues"]["longbridge"]["credentials"],
            json!({
                "app_key": "app-key",
                "app_secret": "app-secret",
                "access_token": "access-token",
            })
        );
    }

    /// HyperCore's two addresses are one choice of each kind, and the row stores
    /// both.
    #[test]
    fn hypercore_stores_the_one_address_of_each_kind() {
        let mut config = Config::default();
        let hyperliquid = venue("hyperliquid");

        assert!(config.set_hyperliquid_origins(
            &hyperliquid,
            HyperliquidHttpOrigin::Api,
            HyperliquidWebsocketOrigin::Api,
        ));

        let settings = config.venue(&hyperliquid).expect("the venue is configured");
        assert_eq!(
            settings
                .origins(HYPERLIQUID_CORE_PERP)
                .map(|o| (o.http.to_string(), o.websocket.to_string())),
            Some((
                "https://api.hyperliquid.xyz/".to_owned(),
                "wss://api.hyperliquid.xyz/ws".to_owned(),
            ))
        );
        assert!(!config.set_hyperliquid_origins(
            &venue("okx"),
            HyperliquidHttpOrigin::Api,
            HyperliquidWebsocketOrigin::Api,
        ));
    }

    /// A credential value is written into the venue's own shape and nowhere else: a
    /// field of another venue's credentials is not a value this row has.
    #[test]
    fn a_credential_value_lands_in_the_venue_that_stores_it() {
        let mut config = Config::default();
        let binance = venue("binance");

        assert!(config.set_venue_credential(&binance, "spot_api_key", "spot-key"));
        assert!(
            config.set_venue_credential(&binance, "usd_m_perp_api_key", "perp-key"),
            "each market stores its own key"
        );
        assert!(
            !config.set_venue_credential(&binance, "api_key", "okx-key"),
            "an OKX field is not a field of Binance's shape"
        );
        assert!(
            !config.set_venue_credential(&binance, "ed_25519_api_key", "superseded"),
            "the shape this build replaced is not this shape"
        );

        let serialized = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(
            serialized["venues"]["binance"]["credentials"],
            json!({
                "spot_api_key": "spot-key",
                "spot_private_key": "",
                "spot_passphrase": "",
                "usd_m_perp_api_key": "perp-key",
                "usd_m_perp_private_key": "",
                "usd_m_perp_passphrase": "",
            })
        );
    }

    /// A credential set of one market is what signs that market's requests: the two
    /// sets of one venue are validated apart.
    #[test]
    fn each_binance_market_validates_its_own_credentials() {
        let mut config = Config::default();
        let binance = venue("binance");

        assert!(config.set_venue_credential(&binance, "spot_api_key", "spot-key"));
        let settings = config.venue(&binance).expect("the venue is configured");

        assert_eq!(
            settings.validate_credentials(binance, BINANCE_SPOT),
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue: binance,
                scope: Some(SmolStr::new_static(BINANCE_SPOT)),
            }),
            "the spot half is only partly filled"
        );
        assert_eq!(
            settings.validate_credentials(binance, BINANCE_USD_M_PERP),
            Ok(false),
            "the perpetual half was never started"
        );

        config.set_venue_credential(&binance, "spot_private_key", "spot-private");
        assert_eq!(
            config
                .venue(&binance)
                .expect("the venue is configured")
                .validate_credentials(binance, BINANCE_SPOT),
            Ok(true)
        );
    }

    /// Credentials and addresses are stored per venue, and one venue is one key in
    /// the persisted file.
    #[test]
    fn venue_credentials_and_origins_should_be_stored_per_venue() {
        let config = Config {
            venues: configured_venues(),
            ..Config::default()
        };
        let serialized = serde_json::to_value(&config).expect("config should serialize");

        assert_eq!(
            serialized["venues"]["binance"]["credentials"]["spot_api_key"],
            json!("binance-spot-key")
        );
        assert_eq!(
            serialized["venues"]["binance"]["credentials"]["usd_m_perp_api_key"],
            json!("binance-perp-key")
        );
        assert_eq!(
            serialized["venues"]["okx"]["region"],
            json!("eu"),
            "an operator-selected region is what the settings interface edits"
        );
        assert_eq!(
            serialized["venues"]["hyperliquid"]["http"],
            json!("https://api.hyperliquid.xyz/")
        );
        assert_eq!(
            serialized["venues"]["longbridge"]["region"],
            json!("china"),
            "a venue reached per region stores the region rather than an address"
        );
        assert_eq!(
            serialized["venues"]["longbridge"]["credentials"]["access_token"],
            json!("longbridge-access-token")
        );
        assert_eq!(
            serde_json::from_value::<Config>(serialized).expect("config should deserialize"),
            config
        );
    }

    /// A venue's row carries exactly the settings its own venue has: no scope
    /// dimension beyond the markets a venue publishes, and no address at all where
    /// the venue publishes a region instead.
    #[test]
    fn a_venue_row_carries_only_its_own_settings() {
        let config = Config {
            venues: configured_venues(),
            ..Config::default()
        };
        let serialized = serde_json::to_value(&config).expect("config should serialize");

        let binance = serialized["venues"]["binance"]
            .as_object()
            .expect("a venue serializes as an object");
        let mut keys: Vec<&str> = binance.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["credentials", "origins", "proxy", "venue"],
            "a venue names its markets, its credentials and its own line"
        );
        assert_eq!(
            binance["origins"]
                .as_object()
                .expect("the markets serialize as an object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [BINANCE_SPOT, BINANCE_USD_M_PERP],
            "only the scopes the venue publishes are named"
        );

        let okx = serialized["venues"]["okx"]
            .as_object()
            .expect("a venue serializes as an object");
        let mut keys: Vec<&str> = okx.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["credentials", "proxy", "region", "venue"],
            "OKX configures a region rather than an address"
        );

        let wire = serialized.to_string();
        assert!(
            !wire.contains("perp-dex/"),
            "a scope the venue deploys at runtime is never a configuration key: {wire}"
        );
        assert!(
            !wire.contains("default_origin")
                && !wire.contains("endpoint")
                && !wire.contains("auth_profile"),
            "the removed default-origin, endpoint and profile dimensions are gone from the shape"
        );
    }

    /// A file that carries only the shape this build replaced — no venues, or a
    /// venue keyed by scope with a default origin — is refused whole, which is what
    /// resets it.
    ///
    /// The refused part of the second file is that its row names *fewer* origins than the venue
    /// publishes — the superseded shape kept one address per scope and a default beside them,
    /// where this one keeps a choice per market and both markets. A key this build does not know
    /// is ignored rather than read, so `default_origin` beside a complete row would decode: what
    /// makes the old file unreadable is what it is missing, not what it left behind.
    #[test]
    fn the_superseded_venue_shape_is_refused() {
        let mut old = serde_json::to_value(Config::default()).expect("config should serialize");
        old.as_object_mut()
            .expect("config serializes as an object")
            .remove("venues");
        old["exchanges"] = json!({ "endpoints": {}, "credential_profiles": {} });
        assert!(
            serde_json::from_value::<Config>(old).is_err(),
            "an old file is refused whole, which is what resets it"
        );

        let mut scoped = serde_json::to_value(Config::default()).expect("config should serialize");
        scoped["venues"]["binance"] = json!({
            "proxy": null,
            "credentials": { "kind": "binance", "ed_25519_api_key": "key" },
            "default_origin": {
                "http": "https://api.binance.com/",
                "websocket": "wss://stream.binance.com:9443/",
            },
            "origins": {
                "spot": {
                    "http": "https://api.binance.com/",
                    "websocket": "wss://stream.binance.com:9443/",
                },
            },
        });
        assert!(
            serde_json::from_value::<Config>(scoped).is_err(),
            "the scope-keyed venue shape is not this shape"
        );

        // The refusal is the missing market, not the keys the old shape left behind: a row that
        // names both markets and still carries a `default_origin` is a file that holds everything
        // this build reads, and the key it no longer has is not what reaching the venue turns on.
        let mut complete =
            serde_json::to_value(Config::default()).expect("config should serialize");
        complete["venues"]["binance"]["default_origin"] = json!("https://api.binance.com/");
        let decoded = serde_json::from_value::<Config>(complete)
            .expect("a complete row decodes whatever else it carries");
        assert_eq!(
            decoded
                .venue(&venue("binance"))
                .and_then(|settings| settings.origins(BINANCE_SPOT))
                .map(|origins| origins.http.to_string()),
            Some("https://api.binance.com/".to_owned()),
            "and the address it is reached on is the one the row's own choice names"
        );
        assert!(
            !serde_json::to_value(&decoded)
                .expect("config should serialize")
                .to_string()
                .contains("default_origin"),
            "because what is written back is the shape this build has"
        );
    }

    #[test]
    fn default_appearance_should_follow_the_system() {
        assert_eq!(Config::default().appearance, Appearance::System);
    }

    #[test]
    fn default_language_should_be_simplified_chinese() {
        assert_eq!(Config::default().language, Language::ZhCn);
    }

    #[test]
    fn language_should_round_trip_stable_json_values() {
        for (language, value) in [(Language::ZhCn, "zh-CN"), (Language::En, "en")] {
            let json = serde_json::to_value(language).expect("language should serialize");
            let decoded =
                serde_json::from_value(json.clone()).expect("language should deserialize");

            assert_eq!((json, decoded), (serde_json::json!(value), language));
        }
    }

    #[test]
    fn appearance_should_use_stable_json_values() {
        let values = [Appearance::Light, Appearance::Dark, Appearance::System].map(|appearance| {
            serde_json::to_value(appearance).expect("appearance should serialize")
        });

        assert_eq!(
            values,
            [
                serde_json::json!("Light"),
                serde_json::json!("Dark"),
                serde_json::json!("System"),
            ]
        );
    }

    #[test]
    fn proxy_applications_and_default_should_be_required_in_the_current_wire_shape() {
        let mut json =
            serde_json::to_value(Config::default()).expect("default config should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("proxy_applications");
        assert!(serde_json::from_value::<Config>(json).is_err());

        let mut json =
            serde_json::to_value(Config::default()).expect("default config should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("proxy_default");
        assert!(serde_json::from_value::<Config>(json).is_err());
    }

    /// A venue that names no line of its own stores no line: a fresh installation
    /// writes the key as nothing rather than leaving it out, so a reader never has
    /// to answer what an absent field means. A file that leaves the key out all the
    /// same says the same thing, which is how an optional field is read.
    #[test]
    fn a_venue_that_names_no_line_should_store_none() {
        let serialized =
            serde_json::to_value(Config::default()).expect("default config should serialize");
        let binance = serialized["venues"]["binance"]
            .as_object()
            .expect("a venue serializes as an object");
        assert!(
            binance.contains_key("proxy"),
            "a venue that follows the default line still stores the key: {binance:?}"
        );
        assert_eq!(
            binance["proxy"],
            serde_json::Value::Null,
            "and it stores no line of its own under it"
        );

        let mut json =
            serde_json::to_value(Config::default()).expect("default config should serialize");
        json["venues"]["binance"]
            .as_object_mut()
            .expect("a venue serializes as an object")
            .remove("proxy");

        let decoded: Config =
            serde_json::from_value(json).expect("a venue without a line key should decode");
        assert_eq!(
            decoded
                .venue(&venue("binance"))
                .and_then(VenueSettings::proxy),
            None,
            "an absent key is a venue that names no line of its own"
        );
    }

    #[test]
    fn proxy_applications_should_use_an_array_and_reject_old_object_wire() {
        let mut config = Config::default();
        config
            .proxy_applications
            .insert(x_media_target(), ProxyRoute::named("local"));
        let serialized = serde_json::to_value(&config).expect("config should serialize");
        assert_eq!(
            serialized["proxy_default"],
            serde_json::json!({ "kind": "direct" })
        );
        assert_eq!(
            serialized["proxy_applications"],
            serde_json::json!([{
                "target": {
                    "kind": "kytrix_service",
                    "capability": "x_media",
                },
                "route": { "kind": "named", "name": "local" },
            }])
        );
        assert_eq!(
            serde_json::from_value::<Config>(serialized).expect("config should deserialize"),
            config
        );

        let mut old_wire =
            serde_json::to_value(Config::default()).expect("config should serialize");
        old_wire["proxy_applications"] = serde_json::json!({
            "Exchange::Binance::Spot::Candlesticks": "local"
        });
        assert!(serde_json::from_value::<Config>(old_wire).is_err());

        let mut old_proxy_field =
            serde_json::to_value(Config::default()).expect("config should serialize");
        old_proxy_field["proxy_applications"] = serde_json::json!([{
            "target": {
                "kind": "venue",
                "target": {
                    "venue": "binance",
                    "scope": "core/spot",
                    "capability": "candlesticks",
                },
            },
            "proxy": "local",
        }]);
        assert!(serde_json::from_value::<Config>(old_proxy_field).is_err());
    }

    #[test]
    fn proxy_applications_should_reject_the_removed_opaque_route_target() {
        for removed in [
            serde_json::json!({
                "target": { "kind": "route", "id": 1 },
                "route": { "kind": "named", "name": "local" },
            }),
            serde_json::json!({
                "target": { "kind": "route", "id": 0 },
                "route": { "kind": "direct" },
            }),
            serde_json::json!({
                "target": {
                    "kind": "exchange",
                    "venue": "binance",
                    "market": "Spot",
                    "capability": "candlesticks",
                },
                "route": { "kind": "direct" },
            }),
        ] {
            let mut json =
                serde_json::to_value(Config::default()).expect("config should serialize");
            json["proxy_applications"] = serde_json::json!([removed]);

            assert!(
                serde_json::from_value::<Config>(json).is_err(),
                "an opaque route target must not decode"
            );
        }
    }

    #[test]
    fn proxy_applications_should_reject_duplicate_targets_and_ignore_unknown_fields() {
        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        let target = serde_json::json!({
            "kind": "kytrix_service",
            "capability": "x_media",
        });
        json["proxy_applications"] = serde_json::json!([
            { "target": target, "route": { "kind": "named", "name": "local" } },
            { "target": target, "route": { "kind": "named", "name": "backup" } },
        ]);
        assert!(serde_json::from_value::<Config>(json).is_err());

        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        json["proxy_applications"] = serde_json::json!([
            {
                "target": {
                    "kind": "kytrix_service",
                    "capability": "x_media",
                    "unexpected": true,
                },
                "route": { "kind": "named", "name": "local" },
            }
        ]);
        let decoded = serde_json::from_value::<Config>(json)
            .expect("unknown proxy application fields should be ignored");
        assert_eq!(decoded.proxy_applications.len(), 1);

        let mut json = serde_json::to_value(Config::default()).expect("config should serialize");
        json["proxy_applications"] = serde_json::json!([
            {
                "target": target,
                "route": { "kind": "named", "name": "local" },
                "unexpected": true,
            }
        ]);
        let decoded = serde_json::from_value::<Config>(json)
            .expect("unknown network target fields should be ignored");
        assert_eq!(decoded.proxy_applications.len(), 1);
    }

    /// A venue that names no line of its own is routed by the document's default
    /// line, exactly as a Kytrix service without an override is.
    #[test]
    fn a_venue_follows_the_default_route_until_it_has_one_of_its_own() {
        let mut config = Config::default();
        assert_eq!(
            config.network_config().proxy_route_for(&venue("binance")),
            ProxyRoute::Direct,
            "a fresh venue follows a default line that is no proxy"
        );

        config.proxies.insert(
            SmolStr::new_static("anonymous"),
            ProxyConfig {
                addr: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::None,
                resolve_dns_at_remote: true,
            },
        );
        config.proxy_default = ProxyRoute::named("anonymous");

        assert_eq!(
            config.network_config().proxy_route_for(&venue("binance")),
            ProxyRoute::named("anonymous"),
            "a venue that names no line of its own answers the default"
        );

        config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::Direct));
        let network = config.network_config();
        assert_eq!(
            network.proxy_route_for(&venue("binance")),
            ProxyRoute::Direct,
            "the venue's own line is what routes it"
        );
        assert_eq!(
            network.proxy_route_for(&venue("okx")),
            ProxyRoute::named("anonymous"),
            "another venue keeps following the default"
        );
        assert_eq!(
            network.service_proxy_route_for(&x_media_target()),
            ProxyRoute::named("anonymous"),
            "and a Kytrix service without an override follows it too"
        );

        config.set_venue_proxy(&venue("binance"), None);
        assert_eq!(
            config.network_config().proxy_route_for(&venue("binance")),
            ProxyRoute::named("anonymous"),
            "giving the line up puts the venue back on the default"
        );
        let mut without_rows = without_rows();
        assert!(
            !without_rows.set_venue_proxy(&venue("binance"), Some(ProxyRoute::Direct)),
            "a venue with no row has nowhere to write a line"
        );
    }

    #[test]
    fn remove_proxy_should_clear_all_behavior_bindings() {
        let mut config = configured_settings();

        config.remove_proxy("local");

        assert!(config.proxy_applications.is_empty());
    }

    /// A venue bound to a removed entry is a binding like any other: the line it
    /// named is gone, so the venue follows the default line again rather than
    /// keeping a route nothing can resolve or falling back to a line of its own.
    #[test]
    fn remove_proxy_should_put_a_bound_venue_back_on_the_default_line() {
        let mut config = configured_settings();
        let backup = config
            .proxies
            .get("local")
            .expect("configured settings should contain local proxy")
            .clone();
        config.proxies.insert(SmolStr::new_static("backup"), backup);
        config.proxy_default = ProxyRoute::named("backup");
        config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::named("local")));

        config.remove_proxy("local");

        assert_eq!(
            config
                .venue(&venue("binance"))
                .expect("the venue is configured")
                .proxy(),
            None,
            "the venue names no line of its own again"
        );
        assert_eq!(
            config.network_config().proxy_route_for(&venue("binance")),
            ProxyRoute::named("backup"),
            "so it is the default line, and not another route, that routes it"
        );
    }

    #[test]
    fn remove_proxy_should_reset_named_default_to_direct() {
        let mut config = configured_settings();
        config.proxy_default = ProxyRoute::named("local");

        config.remove_proxy("local");

        assert_eq!(config.proxy_default, ProxyRoute::Direct);
    }

    #[test]
    fn replace_proxy_should_reject_reserved_names() {
        let mut config = configured_settings();
        let expected = config.clone();

        let result = config.replace_proxy("local", "@direct".into(), replacement_proxy());

        assert_eq!(
            (result, config),
            (
                Err(ReplaceProxyError::ReservedName {
                    name: SmolStr::new_static("@direct"),
                }),
                expected,
            )
        );
    }

    #[test]
    fn remove_proxy_should_preserve_other_proxy_bindings() {
        let mut config = configured_settings();
        let backup = config
            .proxies
            .get("local")
            .expect("configured settings should contain local proxy")
            .clone();
        config.proxies.insert(SmolStr::new_static("backup"), backup);
        config
            .proxy_applications
            .insert(x_media_target(), ProxyRoute::named("backup"));
        config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::named("backup")));
        config.set_venue_proxy(&venue("okx"), Some(ProxyRoute::named("backup")));

        let removed = config.remove_proxy("local");

        assert_eq!(
            (
                removed.is_some(),
                config.proxies.contains_key("local"),
                config.proxies.contains_key("backup"),
                config.proxy_applications.len(),
                config
                    .proxy_applications
                    .values()
                    .all(|route| route.named_name() == Some("backup")),
                config
                    .venues
                    .keys()
                    .filter(|venue| venue_named_route(&config, venue).as_deref() == Some("backup"))
                    .count(),
            ),
            (true, false, true, 1, true, 2)
        );
    }

    #[test]
    fn replace_proxy_should_move_the_catalog_entry_and_replace_its_profile() {
        let mut config = configured_settings();
        let replacement = replacement_proxy();

        let result = config.replace_proxy("local", "renamed".into(), replacement.clone());

        assert_eq!(
            (
                result,
                config.proxies.contains_key("local"),
                config.proxies.len(),
                config.proxies.get("renamed"),
            ),
            (Ok(()), false, 1, Some(&replacement))
        );
    }

    #[test]
    fn replace_proxy_should_allow_updating_a_profile_without_renaming_it() {
        let mut config = configured_settings();
        let replacement = replacement_proxy();
        let mut expected = config.clone();
        expected
            .proxies
            .insert(SmolStr::new_static("local"), replacement.clone());

        let result = config.replace_proxy("local", "local".into(), replacement);

        assert_eq!((result, config), (Ok(()), expected));
    }

    #[test]
    fn replace_proxy_should_update_all_behavior_bindings_when_renamed() {
        let mut config = configured_settings();
        config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::named("local")));
        config.set_venue_proxy(&venue("okx"), Some(ProxyRoute::named("local")));

        let result = config.replace_proxy("local", "renamed".into(), replacement_proxy());

        assert_eq!(
            (
                result,
                config.proxy_applications.len(),
                config
                    .proxy_applications
                    .values()
                    .all(|route| route.named_name() == Some("renamed")),
                config
                    .venues
                    .keys()
                    .filter(|venue| venue_named_route(&config, venue).as_deref() == Some("renamed"))
                    .count(),
            ),
            (Ok(()), 1, true, 2),
            "every binding follows the rename, the venues' own routes included"
        );
    }

    #[test]
    fn replace_proxy_should_preserve_references_to_other_profiles() {
        let mut config = configured_settings();
        let backup = config
            .proxies
            .get("local")
            .expect("configured settings should contain local proxy")
            .clone();
        config.proxies.insert(SmolStr::new_static("backup"), backup);
        config
            .proxy_applications
            .insert(x_media_target(), ProxyRoute::named("local"));
        config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::named("backup")));
        config.set_venue_proxy(&venue("okx"), Some(ProxyRoute::named("backup")));

        let result = config.replace_proxy("local", "renamed".into(), replacement_proxy());

        assert_eq!(
            (
                result,
                venue_named_route(&config, &venue("binance")),
                venue_named_route(&config, &venue("okx")),
                config.proxy_applications[&x_media_target()].named_name(),
            ),
            (
                Ok(()),
                Some(SmolStr::new_static("backup")),
                Some(SmolStr::new_static("backup")),
                Some("renamed")
            )
        );
    }

    #[test]
    fn replace_proxy_should_leave_config_unchanged_when_source_is_missing() {
        let mut config = configured_settings();
        let expected = config.clone();

        let result = config.replace_proxy("missing", "renamed".into(), replacement_proxy());

        assert_eq!(
            (result, config),
            (
                Err(ReplaceProxyError::SourceMissing {
                    name: SmolStr::new_static("missing"),
                }),
                expected,
            )
        );
    }

    #[test]
    fn replace_proxy_should_leave_config_unchanged_when_target_exists() {
        let mut config = configured_settings();
        config
            .proxies
            .insert(SmolStr::new_static("existing"), ProxyConfig::default());
        let expected = config.clone();

        let result = config.replace_proxy("local", "existing".into(), replacement_proxy());

        assert_eq!(
            (result, config),
            (
                Err(ReplaceProxyError::TargetExists {
                    name: SmolStr::new_static("existing"),
                }),
                expected,
            )
        );
    }

    #[test]
    fn legacy_config_without_appearance_should_follow_the_system() {
        let expected = configured_settings();
        let mut json =
            serde_json::to_value(&expected).expect("configured settings should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("appearance");

        let decoded: Config =
            serde_json::from_value(json).expect("legacy config should deserialize");

        assert_eq!(decoded.appearance, Appearance::System);
    }

    #[test]
    fn legacy_config_without_language_should_use_simplified_chinese() {
        let expected = configured_settings();
        let mut json =
            serde_json::to_value(&expected).expect("configured settings should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("language");

        let decoded: Config =
            serde_json::from_value(json).expect("legacy config should deserialize");

        assert_eq!(decoded.language, Language::ZhCn);
    }

    #[test]
    fn default_data_time_zone_should_be_utc() {
        assert_eq!(Config::default().data_time_zone, DataTimeZone::UTC);
    }

    /// The setting stores the number of hours it adds to UTC, and what is written
    /// back is what a reader converts by.
    #[test]
    fn data_time_zone_should_round_trip_the_stored_hours() {
        let config = Config {
            data_time_zone: DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset"),
            ..Config::default()
        };
        let serialized = serde_json::to_value(&config).expect("config should serialize");

        assert_eq!(serialized["data_time_zone"], json!(8));
        assert_eq!(
            serde_json::from_value::<Config>(serialized).expect("config should deserialize"),
            config
        );
    }

    /// A file written before the setting existed holds every other value and no
    /// offset; it reads as UTC, which is the clock those timestamps were drawn in.
    #[test]
    fn legacy_config_without_data_time_zone_should_use_utc() {
        let mut json = serde_json::to_value(configured_settings())
            .expect("configured settings should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("data_time_zone");

        let decoded: Config =
            serde_json::from_value(json).expect("legacy config should deserialize");

        assert_eq!(decoded.data_time_zone, DataTimeZone::UTC);
    }

    /// An offset no real zone is at is not this shape, exactly as a venue this
    /// build does not speak is not: the value is refused rather than clamped.
    #[test]
    fn a_data_time_zone_outside_the_offered_range_should_be_refused() {
        for hours in [-13, 15] {
            let mut json =
                serde_json::to_value(Config::default()).expect("default config should serialize");
            json["data_time_zone"] = json!(hours);

            assert!(
                serde_json::from_value::<Config>(json).is_err(),
                "{hours} is not an offset the setting offers"
            );
        }
    }

    /// The manager persists a candidate only when it differs from what it holds, so
    /// an equality that ignored the display time zone would silently drop the edit.
    #[test]
    fn config_equality_should_notice_a_data_time_zone_change() {
        let other = Config {
            data_time_zone: DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset"),
            ..Config::default()
        };

        assert_ne!(Config::default(), other);
    }

    /// A first start has no workspaces, which is what makes the workspace list
    /// create its default one.
    #[test]
    fn a_config_without_workspaces_should_decode_as_none() {
        let mut json = serde_json::to_value(configured_settings())
            .expect("configured settings should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("workspaces");

        let decoded: Config =
            serde_json::from_value(json).expect("a config without workspaces should deserialize");

        assert_eq!(decoded.workspaces, WorkspacesConfig::default());
        assert!(decoded.workspaces.entries.is_empty());
        assert_eq!(decoded.workspaces.active, None);
    }

    /// The manager persists a candidate only when it differs from what it holds,
    /// so an equality that ignored workspaces would silently drop every workspace
    /// change.
    #[test]
    fn config_equality_should_notice_a_workspace_change() {
        let with_workspace = |active: &'static str| Config {
            workspaces: WorkspacesConfig {
                entries: vec![
                    WorkspaceConfig {
                        name: SmolStr::new_static("Workspace 1"),
                        layout: WorkspaceLayout::single_pane(),
                    },
                    WorkspaceConfig {
                        name: SmolStr::new_static("Workspace 2"),
                        layout: WorkspaceLayout::single_pane(),
                    },
                ],
                active: Some(SmolStr::new_static(active)),
            },
            ..Config::default()
        };

        assert_ne!(with_workspace("Workspace 1"), with_workspace("Workspace 2"));
        assert_ne!(Config::default(), with_workspace("Workspace 1"));
    }

    /// The manager persists a candidate only when it differs from what it holds, so
    /// an equality that ignored a venue's own settings would silently drop the edit.
    #[test]
    fn config_equality_should_notice_a_venue_change() {
        let configured = Config {
            venues: configured_venues(),
            ..Config::default()
        };
        let mut different = configured.clone();
        let binance = venue("binance");
        different
            .venues
            .get_mut(&binance)
            .expect("the venue is configured")
            .set_proxy(Some(ProxyRoute::Direct));

        assert_ne!(configured, different);
        assert_ne!(Config::default(), configured);
    }

    #[test]
    fn account_config_should_use_stable_json_values() {
        let ethereum = AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".into(),
            wallet_address: "0xabc".into(),
            wallet_kind: AccountWalletKind::Ethereum,
        };
        let solana = AccountConfig {
            user_id: "22222222-2222-4222-8222-222222222222".into(),
            wallet_address: "solana-address".into(),
            wallet_kind: AccountWalletKind::Solana,
        };
        let ethereum_json =
            serde_json::to_value(ethereum).expect("Ethereum account should serialize");
        let solana_json = serde_json::to_value(solana).expect("Solana account should serialize");
        let default_json =
            serde_json::to_value(Config::default()).expect("default config should serialize");

        assert_eq!(
            (ethereum_json, solana_json, &default_json["account"]),
            (
                serde_json::json!({
                    "user_id": "11111111-1111-4111-8111-111111111111",
                    "wallet_address": "0xabc",
                    "wallet_kind": "Ethereum",
                }),
                serde_json::json!({
                    "user_id": "22222222-2222-4222-8222-222222222222",
                    "wallet_address": "solana-address",
                    "wallet_kind": "Solana",
                }),
                &serde_json::Value::Null,
            )
        );
    }

    #[test]
    fn avatar_should_serialize_as_svg_or_null() {
        let default_json =
            serde_json::to_value(Config::default()).expect("default config should serialize");
        let svg = "<svg viewBox=\"0 0 100 100\"></svg>";
        let config = Config {
            avatar: Some(svg.to_owned()),
            ..Config::default()
        };
        let configured_json =
            serde_json::to_value(config).expect("configured avatar should serialize");

        assert_eq!(default_json["avatar"], serde_json::Value::Null);
        assert_eq!(configured_json["avatar"], serde_json::json!(svg));
    }

    #[test]
    fn config_without_avatar_should_decode_as_none() {
        let mut expected = configured_settings();
        expected.avatar = None;
        let mut json =
            serde_json::to_value(&expected).expect("configured settings should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("avatar");

        let decoded: Config =
            serde_json::from_value(json).expect("config without avatar should deserialize");

        assert_eq!(decoded, expected);
    }

    #[test]
    fn config_without_account_should_decode_as_unassociated() {
        let mut expected = configured_settings();
        expected.account = None;
        let mut json =
            serde_json::to_value(&expected).expect("configured settings should serialize");
        json.as_object_mut()
            .expect("config should serialize as an object")
            .remove("account");

        let decoded: Config = serde_json::from_value(json).expect("config should deserialize");

        assert_eq!(decoded, expected);
    }

    #[test]
    fn malformed_account_fields_should_be_rejected() {
        let malformed_accounts = [
            serde_json::json!({
                "user_id": "11111111-1111-4111-8111-111111111111",
                "wallet_address": "0xabc",
                "wallet_kind": "Bitcoin",
            }),
            serde_json::json!("not-an-account-object"),
        ];

        for malformed_account in malformed_accounts {
            let mut json =
                serde_json::to_value(configured_settings()).expect("settings should serialize");
            json["account"] = malformed_account;

            assert!(serde_json::from_value::<Config>(json).is_err());
        }
    }

    #[test]
    fn unknown_account_fields_should_be_ignored() {
        let mut json =
            serde_json::to_value(configured_settings()).expect("settings should serialize");
        json["account"] = serde_json::json!({
            "user_id": "11111111-1111-4111-8111-111111111111",
            "wallet_address": "0xabc",
            "wallet_kind": "Ethereum",
            "unknown": "value",
        });

        let decoded: Config =
            serde_json::from_value(json).expect("unknown account fields should be ignored");

        assert_eq!(
            decoded.account,
            Some(AccountConfig {
                user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
                wallet_address: "0xabc".to_owned(),
                wallet_kind: AccountWalletKind::Ethereum,
            })
        );
    }

    /// A scalar change carries the new value, not the old one and not merely the
    /// fact that something moved: that value is what a subscriber projects.
    #[test]
    fn a_changed_scalar_should_be_reported_with_its_new_value() {
        let previous = Config {
            appearance: Appearance::Light,
            language: Language::En,
            data_time_zone: DataTimeZone::from_hours(-5).expect("UTC-5 is an offered offset"),
            ..Config::default()
        };

        let appearance = Config {
            appearance: Appearance::Dark,
            ..previous.clone()
        };
        assert_eq!(
            appearance.changes_since(&previous),
            ConfigChangeSet {
                appearance: Some(Appearance::Dark),
                ..ConfigChangeSet::default()
            }
        );

        let language = Config {
            language: Language::ZhCn,
            ..previous.clone()
        };
        assert_eq!(
            language.changes_since(&previous),
            ConfigChangeSet {
                language: Some(Language::ZhCn),
                ..ConfigChangeSet::default()
            }
        );

        let data_time_zone = Config {
            data_time_zone: DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset"),
            ..previous.clone()
        };
        assert_eq!(
            data_time_zone.changes_since(&previous),
            ConfigChangeSet {
                data_time_zone: Some(
                    DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset")
                ),
                ..ConfigChangeSet::default()
            }
        );
    }

    #[test]
    fn an_unchanged_config_should_report_nothing() {
        let config = configured_settings();

        assert_eq!(config.changes_since(&config), ConfigChangeSet::default());
        assert!(config.changes_since(&config).is_empty());
    }

    /// The account and its avatar are one domain: changing either, or both, is one
    /// account change and nothing else.
    #[test]
    fn an_account_and_an_avatar_should_report_one_account_change() {
        let previous = configured_settings();

        let mut account = previous.clone();
        account
            .account
            .as_mut()
            .expect("the configured settings hold an account")
            .wallet_address = "0xfeed".to_owned();

        let mut avatar = previous.clone();
        avatar.avatar = Some("<svg>another-avatar</svg>".to_owned());

        let mut both = previous.clone();
        both.account
            .as_mut()
            .expect("the configured settings hold an account")
            .user_id = "22222222-2222-4222-8222-222222222222".to_owned();
        both.avatar = Some("<svg>both-changed</svg>".to_owned());

        let mut removed_account = previous.clone();
        removed_account.account = None;

        let mut removed_avatar = previous.clone();
        removed_avatar.avatar = None;

        for (label, changed) in [
            ("the account", account),
            ("the avatar", avatar),
            ("both", both),
            ("a removed account", removed_account),
            ("a removed avatar", removed_avatar),
        ] {
            assert_eq!(
                changed.changes_since(&previous),
                ConfigChangeSet {
                    account_changed: true,
                    ..ConfigChangeSet::default()
                },
                "{label} is one account change"
            );
        }

        let absent = Config {
            account: None,
            avatar: None,
            ..previous
        };
        assert_eq!(
            absent.changes_since(&absent),
            ConfigChangeSet::default(),
            "no account and no avatar before and after is no change"
        );
    }

    /// The proxy catalog is reported by name once per entry: an edit keeps the
    /// name, an addition names the new entry, a removal names the old one, and a
    /// rename is an addition and a removal, so it names both.
    #[test]
    fn a_proxy_catalog_edit_should_report_the_name_once() {
        let mut previous = Config::default();
        previous
            .proxies
            .insert(SmolStr::new_static("local"), local_proxy());

        let mut edited = previous.clone();
        edited
            .proxies
            .insert(SmolStr::new_static("local"), replacement_proxy());
        assert_eq!(
            edited.changes_since(&previous),
            ConfigChangeSet {
                proxies: vec![SmolStr::new_static("local")],
                ..ConfigChangeSet::default()
            },
            "an edited entry is its own name once"
        );

        let mut added = previous.clone();
        added
            .proxies
            .insert(SmolStr::new_static("backup"), replacement_proxy());
        assert_eq!(
            added.changes_since(&previous),
            ConfigChangeSet {
                proxies: vec![SmolStr::new_static("backup")],
                ..ConfigChangeSet::default()
            },
            "an added entry is its own name once"
        );

        let mut removed = previous.clone();
        assert_eq!(
            removed.proxies.remove("local"),
            Some(local_proxy()),
            "the fixture removes the entry it added"
        );
        assert_eq!(
            removed.changes_since(&previous),
            ConfigChangeSet {
                proxies: vec![SmolStr::new_static("local")],
                ..ConfigChangeSet::default()
            },
            "a removed entry is its old name once"
        );

        let mut renamed = previous.clone();
        renamed
            .replace_proxy("local", SmolStr::new_static("renamed"), replacement_proxy())
            .expect("the source entry exists");
        assert_eq!(
            renamed.changes_since(&previous),
            ConfigChangeSet {
                proxies: vec![SmolStr::new_static("renamed"), SmolStr::new_static("local"),],
                ..ConfigChangeSet::default()
            },
            "a rename is the added pass first and the removed pass second"
        );
    }

    #[test]
    fn a_default_route_change_should_report_the_flag() {
        let previous = Config::default();
        let changed = Config {
            proxy_default: ProxyRoute::System,
            ..previous.clone()
        };

        assert_eq!(
            changed.changes_since(&previous),
            ConfigChangeSet {
                proxy_default_changed: true,
                ..ConfigChangeSet::default()
            }
        );
    }

    #[test]
    fn a_service_application_edit_should_report_the_target_once() {
        let previous = configured_settings();
        let wallet = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::EthereumWallet,
        };

        let mut added = previous.clone();
        added
            .proxy_applications
            .insert(wallet.clone(), ProxyRoute::named("local"));
        assert_eq!(
            added.changes_since(&previous),
            ConfigChangeSet {
                proxy_applications: vec![wallet],
                ..ConfigChangeSet::default()
            },
            "an added application is its target once"
        );

        let mut edited = previous.clone();
        edited
            .proxy_applications
            .insert(x_media_target(), ProxyRoute::Direct);
        assert_eq!(
            edited.changes_since(&previous),
            ConfigChangeSet {
                proxy_applications: vec![x_media_target()],
                ..ConfigChangeSet::default()
            },
            "an edited application is its target once"
        );

        let mut removed = previous.clone();
        assert_eq!(
            removed.proxy_applications.remove(&x_media_target()),
            Some(ProxyRoute::named("local")),
            "the fixture removes the application it added"
        );
        assert_eq!(
            removed.changes_since(&previous),
            ConfigChangeSet {
                proxy_applications: vec![x_media_target()],
                ..ConfigChangeSet::default()
            },
            "a removed application is its target once"
        );
    }

    /// A venue is reported once however its row moved: an added or removed row,
    /// a credential, an origin choice and its own line are all settings of the
    /// same venue.
    #[test]
    fn a_venue_edit_should_report_the_venue_once() {
        let previous = configured_settings();
        let binance = venue("binance");
        let okx = venue("okx");
        let longbridge = venue("longbridge");

        let mut inserted = without_rows();
        assert!(
            inserted
                .venues
                .insert(longbridge, default_venues()[&longbridge].clone())
                .is_none(),
            "the fixture adds a row the empty snapshot does not hold"
        );
        assert_eq!(
            inserted.changes_since(&without_rows()),
            ConfigChangeSet {
                venues: vec![longbridge],
                ..ConfigChangeSet::default()
            },
            "an added row is its venue once"
        );

        let mut removed = previous.clone();
        assert!(
            removed.venues.remove(&longbridge).is_some(),
            "the fixture removes a row the configured snapshot holds"
        );
        assert_eq!(
            removed.changes_since(&previous),
            ConfigChangeSet {
                venues: vec![longbridge],
                ..ConfigChangeSet::default()
            },
            "a removed row is its venue once"
        );

        let mut credentials = previous.clone();
        assert!(credentials.set_venue_credential(&binance, "spot_api_key", "rotated-key"));
        assert_eq!(
            credentials.changes_since(&previous),
            ConfigChangeSet {
                venues: vec![binance],
                ..ConfigChangeSet::default()
            },
            "a credential edit is its venue once"
        );

        let mut origins = previous.clone();
        assert!(origins.set_binance_market_origins(
            &binance,
            BinanceMarket::Spot,
            &BinanceOrigins {
                http: BinanceHttpOrigin::Api3,
                websocket: BinanceWebsocketOrigin::StreamAlt,
            },
        ));
        assert_eq!(
            origins.changes_since(&previous),
            ConfigChangeSet {
                venues: vec![binance],
                ..ConfigChangeSet::default()
            },
            "an origin edit is its venue once"
        );

        let mut proxy = previous.clone();
        assert!(proxy.set_venue_proxy(&okx, Some(ProxyRoute::named("local"))));
        assert_eq!(
            proxy.changes_since(&previous),
            ConfigChangeSet {
                venues: vec![okx],
                ..ConfigChangeSet::default()
            },
            "a line edit is its venue once"
        );
    }

    /// The workspaces have one writer and no external reader: a layout write is a
    /// real configuration difference that wakes nobody.
    #[test]
    fn a_workspace_only_change_should_report_nothing() {
        let previous = configured_settings();
        let mut changed = previous.clone();
        changed.workspaces = WorkspacesConfig {
            entries: vec![WorkspaceConfig {
                name: SmolStr::new_static("Workspace 1"),
                layout: WorkspaceLayout::single_pane(),
            }],
            active: Some(SmolStr::new_static("Workspace 1")),
        };

        assert_ne!(changed, previous, "a workspace write is a real difference");
        assert_eq!(changed.changes_since(&previous), ConfigChangeSet::default());
    }

    /// Every settings field the change set covers can reach it: a field that
    /// could not would be one the exhaustive destructuring names as ignored, and
    /// this is the runtime guard on that classification.
    ///
    /// The two ignored fields are checked apart: a workspace write has its own
    /// test, and the network generations are runtime identity that `PartialEq`
    /// does not compare at all.
    #[test]
    fn every_non_workspace_difference_should_report_something() {
        let previous = configured_settings();
        let with_change = |update: fn(&mut Config)| {
            let mut config = previous.clone();
            update(&mut config);
            config
        };
        let changes = [
            (
                "appearance",
                with_change(|config| config.appearance = Appearance::Light),
            ),
            (
                "language",
                with_change(|config| config.language = Language::En),
            ),
            (
                "display time zone",
                with_change(|config| {
                    config.data_time_zone =
                        DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
                }),
            ),
            (
                "account",
                with_change(|config| {
                    config
                        .account
                        .as_mut()
                        .expect("the configured settings hold an account")
                        .wallet_address = "0xfeed".to_owned();
                }),
            ),
            (
                "avatar",
                with_change(|config| config.avatar = Some("<svg>another-avatar</svg>".to_owned())),
            ),
            (
                "proxy catalog",
                with_change(|config| {
                    config
                        .proxies
                        .insert(SmolStr::new_static("local"), replacement_proxy());
                }),
            ),
            (
                "default route",
                with_change(|config| config.proxy_default = ProxyRoute::System),
            ),
            (
                "service application",
                with_change(|config| {
                    config
                        .proxy_applications
                        .insert(x_media_target(), ProxyRoute::Direct);
                }),
            ),
            (
                "venue",
                with_change(|config| {
                    assert!(config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::Direct)));
                }),
            ),
        ];

        for (domain, changed) in changes {
            assert_ne!(changed, previous, "the {domain} fixture must differ");
            assert!(
                !changed.changes_since(&previous).is_empty(),
                "a {domain} difference must be reported"
            );
        }

        let mut regenerated = previous.clone();
        regenerated.set_network_generations(different_generations());
        assert_eq!(
            regenerated, previous,
            "the runtime generations are not settings, so the snapshots compare equal"
        );
        assert_eq!(
            regenerated.changes_since(&previous),
            ConfigChangeSet::default(),
            "a generation-only change reports nothing"
        );
    }
}
