//! The persisted settings of every venue this build speaks.
//!
//! A venue and its scopes are facts of the code, not of the file: Binance publishes
//! a spot scope and a USDⓈ-M perpetual scope, OKX publishes a spot scope and a swap
//! scope, and HyperCore publishes a spot scope and a perpetual scope. What the file
//! holds is the operator's *choices* — which published address each scope is reached
//! on, how the venue is authenticated, and which line its requests take — so this
//! module is a closed enum with one variant per venue and one field per choice. A
//! venue added to the identity is a compile error here until its settings are stated,
//! and a file that names a venue, a scope or an address this build does not publish
//! is refused whole at decode time rather than completed from the table below.
//!
//! Nothing here discovers anything. A scope a venue deploys at runtime — HyperCore's
//! `perp-dex/*` — is not a configuration key and never becomes one: it is reached
//! through the venue's [`VenueSettings::primary_scope`], the scope the venue answers
//! its own non-scoped conversations on.
//!
//! [`default_venues`] is what a fresh installation stores, and it is also the list
//! the settings interface and the decoders are held against.

use std::collections::BTreeMap;

use market_participants_model::identifier::VenueId;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use smol_str::SmolStr;
#[cfg(feature = "test-support")]
use url::Url;

use crate::{
    market_participants::{
        credentials::{
            BinanceCredentials, HyperliquidCredentials, LongbridgeCredentials, OkxCredentials,
            VenueCredentials,
        },
        error::MarketParticipantsConfigError,
        origins::{
            BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, HyperliquidHttpOrigin,
            HyperliquidWebsocketOrigin, LongbridgeRegion, OkxRegion, OriginPair, OriginRef,
            published_url,
        },
    },
    network::ProxyRoute,
};

/// The canonical name of Binance's spot scope.
pub const BINANCE_SPOT: &str = "spot";
/// The canonical name of Binance's USDⓈ-M perpetual scope.
pub const BINANCE_USD_M_PERP: &str = "usd-m-perp";
/// The canonical name of OKX's spot scope.
pub const OKX_SPOT: &str = "spot";
/// The canonical name of OKX's swap scope.
pub const OKX_SWAP: &str = "swap";
/// The canonical name of the HyperCore spot scope.
pub const HYPERLIQUID_CORE_SPOT: &str = "core-spot";
/// The canonical name of the HyperCore perpetual scope.
pub const HYPERLIQUID_CORE_PERP: &str = "core-perp";
/// The canonical name of Longbridge's US-equity scope.
pub const LONGBRIDGE_US: &str = "us";

/// One venue's settings, exactly as the file stores them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "venue")]
pub enum VenueSettings {
    #[serde(rename = "binance")]
    Binance(BinanceSettings),
    #[serde(rename = "hyperliquid")]
    Hyperliquid(HyperliquidSettings),
    #[serde(rename = "okx")]
    Okx(OkxSettings),
    #[serde(rename = "longbridge")]
    Longbridge(LongbridgeSettings),
}

/// The addresses one venue's scopes are dialed at instead of the ones it publishes.
///
/// A fresh installation holds none, and neither does a configuration decoded from a file:
/// every scope is reached where its venue publishes it. A test that has to reach a fixture
/// holds one — see `crate::market_participants::fixture` — and a request then dials the fixture's
/// sockets while the settings keep the published spellings a file stores.
///
/// The value is the running snapshot's rather than the file's: it is never written, and the
/// venue rows compare as equal whatever it holds. A settings manager persists a candidate only
/// when it differs from what it holds, so a fixture that moved an address without changing a
/// setting would otherwise look like a change and stamp a new generation onto a venue nothing
/// about it moved.
#[cfg(feature = "test-support")]
pub type Bindings = BTreeMap<SmolStr, FixturePair>;

/// The addresses one venue's scopes are dialed at, which no build but a test's has.
#[cfg(not(feature = "test-support"))]
pub type Bindings = ();

/// The sockets one fixture answers a venue's scopes on, by role.
///
/// The choices a venue's settings hold are what a file stores and what its page draws; this is
/// what a request dials instead while a test drives its own servers.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixturePair {
    /// The socket the venue's requests are sent to.
    pub http: Url,
    /// The socket the venue's streams are opened on.
    pub websocket: Url,
}

#[cfg(feature = "test-support")]
impl FixturePair {
    /// Returns the fixture's pair of addresses.
    pub fn origins(&self) -> OriginPair {
        OriginPair {
            http: self.http.clone(),
            websocket: self.websocket.clone(),
        }
    }
}

/// Binance's settings: one origin choice and one credential set per market.
///
/// The venue's two products are configured apart because they are reached apart and
/// signed apart: the spot API answers on its own hosts and authenticates with its own
/// key, and the USDⓈ-M futures API does neither with the spot API's.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BinanceSettings {
    /// The line this venue's requests take, or nothing while it follows the
    /// document's default line.
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    /// The origin each of the venue's markets is reached on, keyed by the market's
    /// canonical scope name.
    #[serde(rename = "origins", deserialize_with = "deserialize_market_origins")]
    pub origins: BTreeMap<SmolStr, BinanceOrigins>,
    /// The credentials each of the venue's markets signs with.
    #[serde(rename = "credentials")]
    pub credentials: BinanceCredentials,
    /// The addresses this venue's markets are dialed at, which a test's fixture fills in.
    ///
    /// Public because it is a field of a public struct, and a caller outside this crate that
    /// builds one from `..Default::default()` has to be able to name it. Nothing else writes
    /// it: a decoded file answers the default, which is a venue reached where it
    /// publishes its addresses.
    #[serde(skip)]
    pub bindings: Bindings,
}

/// Two Binance rows are one row when the settings they hold are one: the fixture addresses a
/// test installs are the running snapshot's, not the installation's.
impl PartialEq for BinanceSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.origins == other.origins
            && self.credentials == other.credentials
    }
}

impl Eq for BinanceSettings {}

impl BinanceSettings {
    /// Returns the origin choices of one market, or nothing for a market the file
    /// does not configure.
    pub fn market_origins(&self, market: BinanceMarket) -> Option<BinanceOrigins> {
        self.origins.get(market.scope()).cloned()
    }

    /// Returns the addresses one market is dialed at.
    ///
    /// What the market's own choices state, unless a test's fixture pointed it elsewhere: the
    /// choices are untouched either way, so a file this build writes still says what the venue
    /// publishes.
    pub fn market_addresses(&self, market: BinanceMarket) -> OriginPair {
        let origins = self
            .market_origins(market)
            .unwrap_or_else(|| BinanceOrigins::new(market, market.http(), market.websocket()));

        #[cfg(feature = "test-support")]
        if let Some(fixture) = self.bindings.get(market.scope()) {
            return fixture.origins();
        }

        origins.addresses()
    }
}

impl BinanceOrigins {
    /// Declares one market's two address choices.
    ///
    /// The market is what tells the choices apart — spot and USDⓈ-M publish disjoint hosts —
    /// so a pair built in code is built from the market it is for rather than from the venue's
    /// whole table. A file is read through this type's decoder instead, which holds each
    /// market's key to that same market's addresses.
    ///
    /// # Panics
    ///
    /// Panics when a choice is not one `market` publishes: a caller that reaches here with one
    /// has taken an address from the venue's table rather than from its own market's.
    pub fn new(
        market: BinanceMarket,
        http: BinanceHttpOrigin,
        websocket: BinanceWebsocketOrigin,
    ) -> Self {
        assert!(
            BinanceHttpOrigin::for_market(market).contains(&http),
            "Binance's {} market publishes no HTTP address {:?}",
            market.scope(),
            http.as_str()
        );
        assert!(
            BinanceWebsocketOrigin::for_market(market).contains(&websocket),
            "Binance's {} market publishes no WebSocket address {:?}",
            market.scope(),
            websocket.as_str()
        );
        Self { http, websocket }
    }

    /// Returns the addresses this pair of choices is dialed at.
    pub fn addresses(&self) -> OriginPair {
        OriginPair {
            http: published_url(&OriginRef::published(self.http.as_str())),
            websocket: published_url(&OriginRef::published(self.websocket.as_str())),
        }
    }
}

impl Default for BinanceSettings {
    fn default() -> Self {
        Self {
            proxy: None,
            origins: BinanceMarket::ALL
                .into_iter()
                .map(|market| {
                    (
                        SmolStr::new_static(market.scope()),
                        BinanceOrigins::new(market, market.http(), market.websocket()),
                    )
                })
                .collect(),
            credentials: BinanceCredentials::default(),
            bindings: Bindings::default(),
        }
    }
}

/// One market's two origin choices.
///
/// Both choices are required, and each is a variant of a closed set the venue publishes:
/// a market is always reached on an address this build knows, so no request path has to
/// decide what a missing one means. Which of the closed set belongs to which market is
/// [`BinanceOrigins::new`]'s question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinanceOrigins {
    #[serde(rename = "http")]
    pub http: BinanceHttpOrigin,
    #[serde(rename = "websocket")]
    pub websocket: BinanceWebsocketOrigin,
}

/// OKX's settings: the service region the account belongs to, and its credentials.
///
/// The venue publishes one API per region and refuses the others, so the region —
/// not an address — is what the operator chooses, and every scope of the venue is
/// reached through the one the region answers on.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OkxSettings {
    /// The line this venue's requests take, or nothing while it follows the
    /// document's default line.
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    /// The service region this account belongs to.
    #[serde(rename = "region")]
    pub region: OkxRegion,
    /// The credentials the venue's authenticated requests sign with.
    #[serde(rename = "credentials")]
    pub credentials: OkxCredentials,
    /// The addresses this venue's scopes are dialed at, which a test's fixture fills in.
    ///
    /// Public for the reason [`BinanceSettings::bindings`] is.
    #[serde(skip)]
    pub bindings: Bindings,
}

/// Two OKX rows are one row when the settings they hold are one: the fixture addresses a test
/// installs are the running snapshot's, not the installation's.
impl PartialEq for OkxSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.region == other.region
            && self.credentials == other.credentials
    }
}

impl Eq for OkxSettings {}

impl OkxSettings {
    /// Returns the addresses the venue is dialed at.
    ///
    /// The region's own pair, unless a test's fixture pointed the venue elsewhere: the region
    /// the settings hold is untouched either way, so a file this build writes still says which
    /// region the operator chose.
    ///
    /// Every scope of the venue answers this one pair, so the fixture's own primary scope is the
    /// binding to read: a fixture points a venue by venue rather than by scope, and the two keys
    /// hold the same sockets.
    pub fn addresses(&self) -> OriginPair {
        #[cfg(feature = "test-support")]
        if let Some(fixture) = self.bindings.get(OKX_SPOT) {
            return fixture.origins();
        }

        self.region.origins()
    }
}

/// HyperCore's settings: one address of each kind, and the API Agent's credentials.
///
/// The venue publishes exactly one production host for requests and one for streams,
/// so each choice has one answer. It is still a choice — and still stored — because
/// the setting exists, and a stored choice is what makes the address a value the
/// settings own rather than one this build happens to compile in.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct HyperliquidSettings {
    /// The line this venue's requests take, or nothing while it follows the
    /// document's default line.
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    /// The address the venue's requests are sent to.
    #[serde(rename = "http")]
    pub http: HyperliquidHttpOrigin,
    /// The address the venue's streams are opened on.
    #[serde(rename = "websocket")]
    pub websocket: HyperliquidWebsocketOrigin,
    /// The credentials the venue's authenticated requests sign with.
    #[serde(rename = "credentials")]
    pub credentials: HyperliquidCredentials,
    /// The addresses this venue's scopes are dialed at, which a test's fixture fills in.
    ///
    /// Public for the reason [`BinanceSettings::bindings`] is.
    #[serde(skip)]
    pub bindings: Bindings,
}

/// Two HyperCore rows are one row when the settings they hold are one: the fixture addresses a
/// test installs are the running snapshot's, not the installation's.
impl PartialEq for HyperliquidSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.http == other.http
            && self.websocket == other.websocket
            && self.credentials == other.credentials
    }
}

impl Eq for HyperliquidSettings {}

impl HyperliquidSettings {
    /// Returns the addresses the venue is dialed at.
    ///
    /// The choices the settings hold, unless a test's fixture pointed the venue elsewhere: the
    /// choices are untouched either way, so a file this build writes still says which address
    /// the venue publishes.
    pub fn addresses(&self) -> OriginPair {
        #[cfg(feature = "test-support")]
        if let Some(fixture) = self.bindings.get(HYPERLIQUID_CORE_PERP) {
            return fixture.origins();
        }

        OriginPair {
            http: published_url(&OriginRef::published(self.http.as_str())),
            websocket: published_url(&OriginRef::published(self.websocket.as_str())),
        }
    }
}

/// Longbridge's settings: the service region the account belongs to, and its credentials.
///
/// The venue publishes one site per region and refuses the other, so the region — not an
/// address — is what the operator chooses, and the venue's one scope is reached through the
/// region the account was opened on.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LongbridgeSettings {
    /// The line this venue's requests take, or nothing while it follows the
    /// document's default line.
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    /// The service region this account belongs to.
    #[serde(rename = "region")]
    pub region: LongbridgeRegion,
    /// The credentials the venue's authenticated requests sign with.
    #[serde(rename = "credentials")]
    pub credentials: LongbridgeCredentials,
    /// The addresses this venue's scopes are dialed at, which a test's fixture fills in.
    ///
    /// Public for the reason [`BinanceSettings::bindings`] is.
    #[serde(skip)]
    pub bindings: Bindings,
}

/// Two Longbridge rows are one row when the settings they hold are one: the fixture addresses a
/// test installs are the running snapshot's, not the installation's.
impl PartialEq for LongbridgeSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.region == other.region
            && self.credentials == other.credentials
    }
}

impl Eq for LongbridgeSettings {}

impl LongbridgeSettings {
    /// Returns the addresses the venue is dialed at.
    ///
    /// The region's own pair, unless a test's fixture pointed the venue elsewhere: the region
    /// the settings hold is untouched either way, so a file this build writes still says which
    /// region the operator chose.
    ///
    /// Every scope of the venue answers this one pair, so the fixture's own primary scope is the
    /// binding to read: a fixture points a venue by venue rather than by scope.
    pub fn addresses(&self) -> OriginPair {
        #[cfg(feature = "test-support")]
        if let Some(fixture) = self.bindings.get(LONGBRIDGE_US) {
            return fixture.origins();
        }

        self.region.origins()
    }
}

impl VenueSettings {
    /// Returns the scopes this venue publishes statically, in the order its page
    /// draws them.
    ///
    /// A scope is the routing namespace a request is addressed by, and the settings
    /// name exactly the scopes this build routes: the list is stated here because the
    /// names a route is spelled with live in `market_participants_connector`'s routing table, which
    /// depends on this crate rather than the other way round. That crate's own test holds
    /// the two lists together. It is not a discovery mechanism and never grows at runtime.
    pub fn scope_ids(&self) -> &'static [&'static str] {
        match self {
            Self::Binance(_) => &[BINANCE_SPOT, BINANCE_USD_M_PERP],
            Self::Hyperliquid(_) => &[HYPERLIQUID_CORE_SPOT, HYPERLIQUID_CORE_PERP],
            Self::Okx(_) => &[OKX_SPOT, OKX_SWAP],
            Self::Longbridge(_) => &[LONGBRIDGE_US],
        }
    }

    /// Returns whether this venue publishes one scope statically.
    pub fn publishes(&self, scope: &str) -> bool {
        self.scope_ids().contains(&scope)
    }

    /// Returns the scope the venue answers its own non-scoped conversations on.
    ///
    /// A venue that discovers what it deploys asks before any of its scopes is
    /// registered, so the reading addresses the venue rather than a scope: this is
    /// the scope it is dialed through, and a scope the venue deploys afterwards is
    /// reached through it as well.
    pub const fn primary_scope(&self) -> &'static str {
        match self {
            Self::Binance(_) => BINANCE_SPOT,
            Self::Hyperliquid(_) => HYPERLIQUID_CORE_PERP,
            Self::Okx(_) => OKX_SPOT,
            Self::Longbridge(_) => LONGBRIDGE_US,
        }
    }

    /// Returns the scope whose credentials a request on `scope` signs with.
    ///
    /// Binance signs each market with its own key, so the answer is the scope itself
    /// there. Every other venue authenticates its whole surface with one set, so a
    /// scope the venue deploys — one it never declares — signs with the same
    /// credentials as the scope this build routes it through.
    pub fn credential_scope<'a>(&self, scope: &'a str) -> &'a str {
        match self {
            Self::Binance(_) => scope,
            Self::Hyperliquid(_) => HYPERLIQUID_CORE_PERP,
            Self::Okx(_) => OKX_SPOT,
            Self::Longbridge(_) => LONGBRIDGE_US,
        }
    }

    /// Returns the addresses one scope of this venue is dialed at.
    ///
    /// The answer is total over everything this build routes: every statically published scope
    /// has its own choice, and a scope the venue deploys is dialed on the addresses of
    /// [`Self::primary_scope`]. A name that is neither — a hand-written file, a route from
    /// another build — is answered by nobody.
    pub fn origins(&self, scope: &str) -> Option<OriginPair> {
        match self {
            Self::Binance(settings) => BinanceMarket::ALL
                .into_iter()
                .find(|market| market.scope() == scope)
                .map(|market| settings.market_addresses(market)),
            Self::Okx(settings) => {
                matches!(scope, OKX_SPOT | OKX_SWAP).then(|| settings.addresses())
            }
            Self::Longbridge(settings) => (scope == LONGBRIDGE_US).then(|| settings.addresses()),
            Self::Hyperliquid(settings) => {
                matches!(scope, HYPERLIQUID_CORE_SPOT | HYPERLIQUID_CORE_PERP)
                    .then(|| settings.addresses())
            }
        }
    }

    /// Returns the origins a scope this build does not route is reached on.
    ///
    /// A scope a venue deploys — HyperCore's `perp-dex/*` — is one of these: it is
    /// not a configuration key, and it is reached through the venue's primary scope
    /// rather than through anything of its own.
    pub fn deployed_origins(&self) -> OriginPair {
        self.origins(self.primary_scope())
            .expect("a venue's primary scope is one of its own statically published scopes")
    }

    /// Returns the outbound line this venue's requests take.
    ///
    /// The line is the venue's own value, so the answer is a copy of it: a caller
    /// that means to rewrite it asks for [`Self::proxy_mut`].
    pub fn proxy(&self) -> Option<ProxyRoute> {
        match self {
            Self::Binance(settings) => settings.proxy.clone(),
            Self::Hyperliquid(settings) => settings.proxy.clone(),
            Self::Okx(settings) => settings.proxy.clone(),
            Self::Longbridge(settings) => settings.proxy.clone(),
        }
    }

    /// Returns the outbound line this venue's requests take, for rewriting.
    ///
    /// A rename reaches every binding through this: the line is the venue's own
    /// value, so it is edited where it is stored rather than written back from a
    /// copy.
    pub fn proxy_mut(&mut self) -> &mut Option<ProxyRoute> {
        match self {
            Self::Binance(settings) => &mut settings.proxy,
            Self::Hyperliquid(settings) => &mut settings.proxy,
            Self::Okx(settings) => &mut settings.proxy,
            Self::Longbridge(settings) => &mut settings.proxy,
        }
    }

    /// Points every scope this venue publishes at one fixture's sockets.
    ///
    /// The choices this venue's settings hold are untouched: what moves is where a request
    /// dials. [`crate::market_participants::fixture`] is the only caller, and it exists only in a build
    /// that carries the `test-support` feature.
    #[cfg(feature = "test-support")]
    pub fn repoint(&mut self, address: &crate::market_participants::fixture::FixtureAddress) {
        let bind = |scope: &'static str| (SmolStr::new_static(scope), address.bindings());
        match self {
            Self::Binance(settings) => {
                settings.bindings = BinanceMarket::ALL
                    .into_iter()
                    .map(|market| bind(market.scope()))
                    .collect();
            }
            Self::Okx(settings) => {
                settings.bindings = [OKX_SPOT, OKX_SWAP].into_iter().map(bind).collect();
            }
            Self::Longbridge(settings) => {
                settings.bindings = [LONGBRIDGE_US].into_iter().map(bind).collect();
            }
            Self::Hyperliquid(settings) => {
                settings.bindings = [HYPERLIQUID_CORE_SPOT, HYPERLIQUID_CORE_PERP]
                    .into_iter()
                    .map(bind)
                    .collect();
            }
        }
    }

    /// Replaces the outbound line this venue's requests take.
    pub fn set_proxy(&mut self, proxy: Option<ProxyRoute>) {
        match self {
            Self::Binance(settings) => settings.proxy = proxy,
            Self::Hyperliquid(settings) => settings.proxy = proxy,
            Self::Okx(settings) => settings.proxy = proxy,
            Self::Longbridge(settings) => settings.proxy = proxy,
        }
    }

    /// Reports whether the credentials one scope's requests sign with are complete,
    /// empty, or only partly filled.
    pub fn validate_credentials(
        &self,
        venue: VenueId,
        scope: &str,
    ) -> Result<bool, MarketParticipantsConfigError> {
        let credential_scope = self.credential_scope(scope);
        match self {
            Self::Binance(settings) => {
                let Some(market) = BinanceMarket::ALL
                    .into_iter()
                    .find(|market| market.scope() == credential_scope)
                else {
                    return Err(MarketParticipantsConfigError::MismatchedShape {
                        venue,
                        scope: Some(SmolStr::new(scope)),
                    });
                };
                let credentials = settings.credentials.market(market.credential_market());
                credentials.validate(venue, Some(credential_scope))
            }
            Self::Okx(settings) => {
                let credentials = VenueCredentials::Okx(settings.credentials.clone());
                credentials.validate_venue(venue)
            }
            Self::Hyperliquid(settings) => {
                let credentials = VenueCredentials::Hyperliquid(settings.credentials.clone());
                credentials.validate_venue(venue)
            }
            Self::Longbridge(settings) => {
                let credentials = VenueCredentials::Longbridge(settings.credentials.clone());
                credentials.validate_venue(venue)
            }
        }
    }
}

/// Returns the settings of every venue this build speaks.
///
/// A fresh installation stores these: every venue on the addresses its own
/// documentation publishes, no line of its own, and no credentials. The rows are
/// built by walking [`VenueId::ALL`] and answering each venue from its own arm, so a
/// venue added to the identity is a compile error here until its settings are stated.
pub fn default_venues() -> BTreeMap<VenueId, VenueSettings> {
    VenueId::ALL
        .into_iter()
        .map(|venue| {
            let settings = match venue {
                VenueId::Binance => VenueSettings::Binance(BinanceSettings::default()),
                VenueId::Hyperliquid => VenueSettings::Hyperliquid(HyperliquidSettings::default()),
                VenueId::Okx => VenueSettings::Okx(OkxSettings::default()),
                VenueId::Longbridge => VenueSettings::Longbridge(LongbridgeSettings::default()),
            };
            (venue, settings)
        })
        .collect()
}

/// Decodes the venue map, refusing a configuration that is not this shape.
///
/// The key set is exact: every venue this build speaks has a row and the map has no
/// other, so a settings interface that draws its rows from the static venue list can
/// never find a row with no configuration behind it. A file that omits one — an older
/// shape, a hand-edited file — is refused whole, which is what sends it down the
/// reset path rather than being completed from the built-in table. Completing it here
/// would be a migration, and the shape it would migrate has no venues at all.
///
/// A venue this build does not speak is refused by the identity itself, one step
/// earlier: the keys of this map are a closed set, so a name outside it is not a
/// venue the file could have meant.
pub(crate) fn deserialize_venues<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<VenueId, VenueSettings>, D::Error>
where
    D: Deserializer<'de>,
{
    let venues = BTreeMap::<VenueId, VenueSettings>::deserialize(deserializer)?;
    let missing: Vec<&str> = VenueId::ALL
        .into_iter()
        .filter(|venue| !venues.contains_key(venue))
        .map(VenueId::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(D::Error::custom(format_args!(
            "venues does not name every venue this build speaks: missing {missing:?}"
        )));
    }
    Ok(venues)
}

/// Decodes Binance's origin choices, refusing a scope the venue does not publish.
///
/// The map is keyed by the market's canonical scope name, so a file that names a
/// scope this build does not route — a deployed one most of all — is refused rather
/// than carried as a row nothing reads.
pub(crate) fn deserialize_market_origins<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<SmolStr, BinanceOrigins>, D::Error>
where
    D: Deserializer<'de>,
{
    let origins = BTreeMap::<SmolStr, BinanceOrigins>::deserialize(deserializer)?;

    let missing: Vec<&str> = BinanceMarket::ALL
        .into_iter()
        .filter(|market| !origins.contains_key(market.scope()))
        .map(BinanceMarket::scope)
        .collect();
    if !missing.is_empty() {
        return Err(D::Error::custom(format_args!(
            "Binance's origins do not name every market this build speaks: missing {missing:?}"
        )));
    }

    for (name, configured) in &origins {
        let Some(market) = BinanceMarket::ALL
            .into_iter()
            .find(|market| market.scope() == name)
        else {
            return Err(D::Error::custom(format_args!(
                "Binance publishes no scope named {name:?}; its markets are {:?}",
                BinanceMarket::ALL.map(BinanceMarket::scope)
            )));
        };

        // Each market reaches its own hosts: the venue's spot API publishes a primary host
        // beside four mirrors and its USDⓈ-M API one host, and a market reached on its
        // sibling's host is a request to the wrong product.
        if !BinanceHttpOrigin::for_market(market).contains(&configured.http) {
            return Err(D::Error::custom(format_args!(
                "Binance's {} market publishes no HTTP address {:?}; its own are {:?}",
                market.scope(),
                configured.http.as_str(),
                BinanceHttpOrigin::for_market(market)
                    .iter()
                    .map(BinanceHttpOrigin::as_str)
                    .collect::<Vec<_>>()
            )));
        }
        if !BinanceWebsocketOrigin::for_market(market).contains(&configured.websocket) {
            return Err(D::Error::custom(format_args!(
                "Binance's {} market publishes no WebSocket address {:?}; its own are {:?}",
                market.scope(),
                configured.websocket.as_str(),
                BinanceWebsocketOrigin::for_market(market)
                    .iter()
                    .map(BinanceWebsocketOrigin::as_str)
                    .collect::<Vec<_>>()
            )));
        }
    }

    Ok(origins)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the venue should be canonical")
    }

    fn at<'a>(venues: &'a BTreeMap<VenueId, VenueSettings>, name: &str) -> &'a VenueSettings {
        &venues[&venue(name)]
    }

    /// Every venue this build speaks is configured, and each is reached on the
    /// addresses its own documentation publishes.
    #[test]
    fn every_built_in_venue_has_its_own_origins() {
        let venues = default_venues();
        assert_eq!(
            venues
                .keys()
                .map(|venue| venue.as_str())
                .collect::<Vec<_>>(),
            ["binance", "hyperliquid", "longbridge", "okx"],
            "the built-in table names every venue this build speaks"
        );

        let binance = at(&venues, "binance");
        assert_eq!(
            binance.scope_ids(),
            [BINANCE_SPOT, BINANCE_USD_M_PERP],
            "Binance publishes the scopes its routes are addressed by"
        );
        assert_eq!(
            binance.origins(BINANCE_SPOT).map(|o| o.http.to_string()),
            Some("https://api.binance.com/".to_owned())
        );
        assert_eq!(
            binance
                .origins(BINANCE_USD_M_PERP)
                .map(|o| o.websocket.to_string()),
            Some("wss://fstream.binance.com/".to_owned())
        );

        let okx = at(&venues, "okx");
        assert_eq!(okx.scope_ids(), [OKX_SPOT, OKX_SWAP]);
        assert_eq!(
            okx.origins(OKX_SPOT).map(|o| o.http.to_string()),
            okx.origins(OKX_SWAP).map(|o| o.http.to_string()),
            "both OKX scopes are served by the region's one host"
        );
        assert_eq!(
            okx.origins(OKX_SPOT).map(|o| o.http.to_string()),
            Some("https://openapi.okx.com/".to_owned()),
            "a fresh installation speaks to the international API"
        );

        let hyperliquid = at(&venues, "hyperliquid");
        assert_eq!(
            hyperliquid.scope_ids(),
            [HYPERLIQUID_CORE_SPOT, HYPERLIQUID_CORE_PERP]
        );
        assert_eq!(
            hyperliquid.origins(HYPERLIQUID_CORE_SPOT),
            hyperliquid.origins(HYPERLIQUID_CORE_PERP),
            "HyperCore serves its spot and perpetual scopes from one host"
        );
        assert_eq!(
            hyperliquid
                .origins(HYPERLIQUID_CORE_PERP)
                .map(|o| o.http.to_string()),
            Some("https://api.hyperliquid.xyz/".to_owned())
        );

        let longbridge = at(&venues, "longbridge");
        assert_eq!(
            longbridge.scope_ids(),
            [LONGBRIDGE_US],
            "the venue publishes the one scope its route is addressed by"
        );
        assert_eq!(
            longbridge
                .origins(LONGBRIDGE_US)
                .map(|o| (o.http.to_string(), o.websocket.to_string())),
            Some((
                "https://openapi.longbridge.com/".to_owned(),
                "wss://openapi-quote.longbridge.com/v2".to_owned(),
            )),
            "a fresh installation speaks to the international site"
        );
        assert_eq!(
            longbridge.origins("global"),
            None,
            "the region is a setting rather than a scope"
        );
    }

    /// A venue that deploys a scope reaches it through its primary scope, and never
    /// through a key of its own: the deployed name is not a scope the settings know.
    #[test]
    fn a_deployed_scope_is_reached_through_the_primary_scope() {
        let venues = default_venues();
        let hyperliquid = at(&venues, "hyperliquid");

        assert!(
            !hyperliquid.publishes("perp-dex/xyz"),
            "a deployed scope is not a scope the settings publish"
        );
        assert_eq!(
            hyperliquid.origins("perp-dex/xyz"),
            None,
            "and it is not a key of its own"
        );
        assert_eq!(
            hyperliquid.deployed_origins(),
            hyperliquid
                .origins(HYPERLIQUID_CORE_PERP)
                .expect("the primary scope is published"),
            "a deployed scope is reached through the venue's own primary scope"
        );
        assert_eq!(hyperliquid.primary_scope(), HYPERLIQUID_CORE_PERP);

        for (venue, settings) in &venues {
            assert!(
                !settings.publishes("perp-dex/xyz"),
                "{venue} never publishes a deployed scope"
            );
            assert!(
                settings.origins("perp-dex/xyz").is_none(),
                "{venue} answers no origins for a name it does not publish"
            );
            assert!(
                settings.origins("core/spot").is_none(),
                "{venue} answers no origins for a spelling it does not use"
            );
            assert!(
                settings.publishes(settings.primary_scope()),
                "{venue}'s primary scope is one of its own"
            );
        }
    }

    /// One market's pair of choices is built from that market, and a pair built from the wrong
    /// one is refused where it is built rather than where it is dialled.
    ///
    /// The two markets' hosts are disjoint, so the only thing that tells a valid pair from an
    /// invalid one is which market it is for: this is the check the decoder and the settings
    /// setter hold a file and a page to, stated once for the code that builds a pair directly.
    #[test]
    fn a_markets_pair_is_built_from_that_market() {
        for market in BinanceMarket::ALL {
            let pair = BinanceOrigins::new(market, market.http(), market.websocket());
            assert_eq!(
                pair,
                BinanceOrigins {
                    http: market.http(),
                    websocket: market.websocket(),
                },
                "a market's own addresses build that market's pair"
            );
        }

        for (market, http, websocket) in [
            (
                BinanceMarket::Spot,
                BinanceHttpOrigin::FuturesApi,
                BinanceWebsocketOrigin::Stream,
            ),
            (
                BinanceMarket::Spot,
                BinanceHttpOrigin::Api,
                BinanceWebsocketOrigin::FuturesStream,
            ),
            (
                BinanceMarket::UsdMPerp,
                BinanceHttpOrigin::Api1,
                BinanceWebsocketOrigin::FuturesStream,
            ),
            (
                BinanceMarket::UsdMPerp,
                BinanceHttpOrigin::FuturesApi,
                BinanceWebsocketOrigin::StreamAlt,
            ),
        ] {
            assert!(
                std::panic::catch_unwind(|| BinanceOrigins::new(market, http, websocket)).is_err(),
                "{} is reached on neither {:?} nor {:?}",
                market.scope(),
                http.as_str(),
                websocket.as_str()
            );
        }
    }

    /// A request signs with its own market's credentials where the venue configures
    /// its markets apart, and with the venue's one set everywhere else.
    #[test]
    fn a_credential_scope_is_the_venues_own_answer() {
        let venues = default_venues();

        let binance = at(&venues, "binance");
        assert_eq!(binance.credential_scope(BINANCE_SPOT), BINANCE_SPOT);
        assert_eq!(
            binance.credential_scope(BINANCE_USD_M_PERP),
            BINANCE_USD_M_PERP
        );

        let okx = at(&venues, "okx");
        assert_eq!(okx.credential_scope(OKX_SPOT), OKX_SPOT);
        assert_eq!(okx.credential_scope(OKX_SWAP), OKX_SPOT);

        let hyperliquid = at(&venues, "hyperliquid");
        assert_eq!(
            hyperliquid.credential_scope(HYPERLIQUID_CORE_SPOT),
            HYPERLIQUID_CORE_PERP
        );
        assert_eq!(
            hyperliquid.credential_scope("perp-dex/xyz"),
            HYPERLIQUID_CORE_PERP,
            "a deployed scope signs with the venue's one credential set"
        );
    }

    /// Each market's stored choices are its own, and an address no documentation
    /// publishes is refused rather than dialed.
    #[test]
    fn every_origin_choice_is_one_the_venue_publishes() {
        let venues = default_venues();
        let VenueSettings::Binance(binance) = at(&venues, "binance") else {
            panic!("binance is configured as Binance");
        };

        for market in BinanceMarket::ALL {
            let origins = binance
                .market_origins(market)
                .expect("the built-in table configures every market");
            assert!(BinanceHttpOrigin::for_market(market).contains(&origins.http));
            assert!(BinanceWebsocketOrigin::for_market(market).contains(&origins.websocket));
        }

        let decoded = serde_json::from_value::<VenueSettings>(json!({
            "venue": "binance",
            "proxy": null,
            "origins": {
                "spot": {
                    "http": "https://api3.binance.com",
                    "websocket": "wss://stream.binance.com:443",
                },
                "usd-m-perp": {
                    "http": "https://fapi.binance.com",
                    "websocket": "wss://fstream.binance.com",
                },
            },
            "credentials": BinanceCredentials::default(),
        }))
        .expect("a published address decodes");
        let VenueSettings::Binance(decoded) = decoded else {
            panic!("the row is Binance's");
        };
        assert_eq!(
            decoded.market_origins(BinanceMarket::Spot),
            Some(BinanceOrigins {
                http: BinanceHttpOrigin::Api3,
                websocket: BinanceWebsocketOrigin::StreamAlt,
            }),
            "the choices are stored as they were written"
        );

        let refused = serde_json::from_value::<VenueSettings>(json!({
            "venue": "binance",
            "proxy": null,
            "origins": {
                "spot": {
                    "http": "https://spot.example",
                    "websocket": "wss://stream.binance.com:9443",
                },
                "usd-m-perp": {
                    "http": "https://fapi.binance.com",
                    "websocket": "wss://fstream.binance.com",
                },
            },
            "credentials": BinanceCredentials::default(),
        }));
        assert!(
            refused.is_err(),
            "an address no documentation publishes is refused"
        );

        // A market reached on its sibling's hosts is refused too: every address here is one
        // the *venue* publishes, so only the market each belongs to tells them apart — and a
        // spot request sent to the futures API is a request to the wrong product.
        let borrowed = serde_json::from_value::<VenueSettings>(json!({
            "venue": "binance",
            "proxy": null,
            "origins": {
                "spot": {
                    "http": "https://fapi.binance.com",
                    "websocket": "wss://fstream.binance.com",
                },
                "usd-m-perp": {
                    "http": "https://fapi.binance.com",
                    "websocket": "wss://fstream.binance.com",
                },
            },
            "credentials": BinanceCredentials::default(),
        }));
        assert!(
            borrowed.is_err(),
            "the spot market publishes no futures host, so a file naming one is not this shape"
        );
    }

    /// A Binance row names every market the venue publishes: one that names fewer is refused
    /// rather than completed from the built-in table, exactly as a file missing a venue is.
    #[test]
    fn a_binance_row_short_of_a_market_is_refused() {
        for markets in [
            json!({
                "spot": {
                    "http": "https://api.binance.com",
                    "websocket": "wss://stream.binance.com:9443",
                },
            }),
            json!({}),
        ] {
            let refused = serde_json::from_value::<VenueSettings>(json!({
                "venue": "binance",
                "proxy": null,
                "origins": markets,
                "credentials": BinanceCredentials::default(),
            }));

            assert!(
                refused.is_err(),
                "a row that does not configure every market is not this shape"
            );
        }
    }

    /// The origin rows are keyed by scope name, and a name the venue does not
    /// publish is refused whole.
    #[test]
    fn a_scope_the_venue_does_not_publish_is_refused() {
        let span = json!({
            "http": "https://api.binance.com",
            "websocket": "wss://stream.binance.com:9443",
        });
        let refused = serde_json::from_value::<VenueSettings>(json!({
            "venue": "binance",
            "proxy": null,
            "origins": { "spot": span, "perp-dex/xyz": span },
            "credentials": BinanceCredentials::default(),
        }));

        assert!(
            refused.is_err(),
            "a scope this build does not route is not a settings key"
        );
    }

    /// A venue's row carries the settings its own venue has: another venue's name, another
    /// venue's credentials, or another venue's keys are all files this build refuses.
    ///
    /// Each case is refused by its own missing or mismatched field, which is what the third one
    /// shows: a HyperCore row is refused because it names no address of its own — `origins` is a
    /// key no venue's row has, so it is ignored rather than read.
    #[test]
    fn a_venue_row_of_another_shape_should_not_decode() {
        for (wrong, refusal) in [
            (
                json!({
                    "venue": "okx",
                    "proxy": null,
                    "http": "https://api.hyperliquid.xyz/",
                    "websocket": "wss://api.hyperliquid.xyz/ws",
                    "credentials": HyperliquidCredentials::default(),
                }),
                "an OKX row is not addressed, so naming an address does not make one",
            ),
            (
                json!({
                    "venue": "binance",
                    "proxy": null,
                    "region": "global",
                    "credentials": OkxCredentials::default(),
                }),
                "a Binance row has two markets of addresses rather than one region",
            ),
            (
                json!({
                    "venue": "hyperliquid",
                    "proxy": null,
                    "origins": {},
                    "credentials": BinanceCredentials::default(),
                }),
                "a HyperCore row names its own two addresses, and this one names neither",
            ),
            (
                json!({
                    "venue": "longbridge",
                    "proxy": null,
                    "origins": {},
                    "credentials": OkxCredentials::default(),
                }),
                "a Longbridge row names the region its account belongs to, and this one names none",
            ),
            (
                json!({ "venue": "kraken", "proxy": null }),
                "a venue this build does not speak is not a row",
            ),
        ] {
            let decoded = serde_json::from_value::<VenueSettings>(wrong.clone());
            assert!(
                decoded.is_err(),
                "{wrong} decoded, but {refusal}: {decoded:?}"
            );
        }
    }

    /// The settings of one venue are one row of the persisted shape, stored under
    /// the venue's canonical name.
    #[test]
    fn a_venue_row_is_stored_under_its_own_name() {
        assert_eq!(
            serde_json::to_value(VenueSettings::Hyperliquid(HyperliquidSettings::default()))
                .expect("a row serializes"),
            json!({
                "venue": "hyperliquid",
                "proxy": null,
                "http": "https://api.hyperliquid.xyz/",
                "websocket": "wss://api.hyperliquid.xyz/ws",
                "credentials": {
                    "main_account_address": "",
                    "api_agent_private_key": "",
                },
            })
        );
        assert_eq!(
            serde_json::to_value(VenueSettings::Okx(OkxSettings::default()))
                .expect("a row serializes"),
            json!({
                "venue": "okx",
                "proxy": null,
                "region": "global",
                "credentials": { "api_key": "", "secret_key": "", "passphrase": "" },
            })
        );
        assert_eq!(
            serde_json::to_value(VenueSettings::Binance(BinanceSettings::default()))
                .expect("a row serializes")["origins"],
            json!({
                "spot": {
                    "http": "https://api.binance.com",
                    "websocket": "wss://stream.binance.com:9443",
                },
                "usd-m-perp": {
                    "http": "https://fapi.binance.com",
                    "websocket": "wss://fstream.binance.com",
                },
            }),
            "the Binance row carries one choice per scope it publishes"
        );
    }

    /// The Longbridge row is one region beside the three values the venue authenticates
    /// with, and nothing else: the region is what decides where the venue is reached, so the
    /// row stores a region rather than a pair of addresses.
    #[test]
    fn a_longbridge_row_stores_its_region_beside_its_credentials() {
        assert_eq!(
            serde_json::to_value(VenueSettings::Longbridge(LongbridgeSettings::default()))
                .expect("a row serializes"),
            json!({
                "venue": "longbridge",
                "proxy": null,
                "region": "global",
                "credentials": { "app_key": "", "app_secret": "", "access_token": "" },
            })
        );

        let decoded = serde_json::from_value::<VenueSettings>(json!({
            "venue": "longbridge",
            "proxy": null,
            "region": "china",
            "credentials": {
                "app_key": "app-key",
                "app_secret": "app-secret",
                "access_token": "access-token",
            },
        }))
        .expect("a row of this shape decodes");
        let VenueSettings::Longbridge(decoded) = decoded else {
            panic!("the row is Longbridge's");
        };
        assert_eq!(decoded.region, LongbridgeRegion::China);
        assert!(decoded.credentials.is_complete());
        assert_eq!(
            decoded.addresses(),
            LongbridgeRegion::China.origins(),
            "the region is what decides where the venue is dialed"
        );
    }

    /// A stored region is one this build speaks: a spelling no table publishes is refused
    /// rather than dialled.
    #[test]
    fn a_region_the_venue_does_not_publish_is_refused() {
        for region in ["us", "eu", "mainland", "China", ""] {
            let refused = serde_json::from_value::<VenueSettings>(json!({
                "venue": "longbridge",
                "proxy": null,
                "region": region,
                "credentials": LongbridgeCredentials::default(),
            }));

            assert!(
                refused.is_err(),
                "{region:?} is not a service region this build speaks"
            );
        }
    }

    /// The venue's three credential values are one set: nothing typed is unconfigured, all
    /// three is configured, and anything between is refused rather than signed with.
    #[test]
    fn longbridge_credentials_are_validated_as_one_set() {
        let longbridge = venue("longbridge");
        let row = |credentials| {
            VenueSettings::Longbridge(LongbridgeSettings {
                credentials,
                ..LongbridgeSettings::default()
            })
        };

        assert_eq!(
            row(LongbridgeCredentials::default()).validate_credentials(longbridge, LONGBRIDGE_US),
            Ok(false),
            "a venue nothing was typed into is unconfigured rather than invalid"
        );
        assert_eq!(
            row(LongbridgeCredentials {
                app_key: SmolStr::new_static("app-key"),
                ..LongbridgeCredentials::default()
            })
            .validate_credentials(longbridge, LONGBRIDGE_US),
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue: longbridge,
                scope: None,
            }),
            "a set the user has to finish is refused wherever the venue is asked about"
        );
        assert_eq!(
            row(LongbridgeCredentials {
                app_key: SmolStr::new_static("app-key"),
                app_secret: SmolStr::new_static("app-secret"),
                access_token: SmolStr::new_static("access-token"),
            })
            .validate_credentials(longbridge, LONGBRIDGE_US),
            Ok(true)
        );
    }

    /// The map a file stores is the one the settings decode, and a file short of a
    /// venue this build speaks is refused.
    #[test]
    fn the_built_in_list_is_what_a_configuration_must_cover() {
        let venues = default_venues();
        let encoded = serde_json::to_value(&venues).expect("the built-in table serializes");
        assert_eq!(
            serde_json::from_value::<BTreeMap<VenueId, VenueSettings>>(encoded)
                .expect("the built-in table decodes"),
            venues
        );

        let short = json!({
            "binance": VenueSettings::Binance(BinanceSettings::default()),
            "hyperliquid": VenueSettings::Hyperliquid(HyperliquidSettings::default()),
        });
        assert!(
            deserialize_venues(short).is_err(),
            "a configuration that omits a venue this build speaks is not this shape"
        );

        // A file written before Longbridge was a venue holds the three rows it had: the map
        // is exact, so the file is refused whole and recovered rather than completed from the
        // built-in table. Adding the row here would be a migration, and there is none.
        let short_of_longbridge = json!({
            "binance": VenueSettings::Binance(BinanceSettings::default()),
            "hyperliquid": VenueSettings::Hyperliquid(HyperliquidSettings::default()),
            "okx": VenueSettings::Okx(OkxSettings::default()),
        });
        assert!(
            deserialize_venues(short_of_longbridge).is_err(),
            "a configuration short of the venue this build added is refused whole"
        );
    }

    /// The line a venue names is the venue's own, and it is the only network fact
    /// these settings carry.
    #[test]
    fn a_venue_names_its_own_line() {
        let mut settings = VenueSettings::Okx(OkxSettings::default());
        assert_eq!(settings.proxy(), None);

        settings.set_proxy(Some(ProxyRoute::named("hk")));
        assert_eq!(settings.proxy(), Some(ProxyRoute::named("hk")));
    }
}
