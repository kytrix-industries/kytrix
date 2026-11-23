use std::collections::BTreeMap;

use market_participants_model::identifier::VenueId;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use smol_str::SmolStr;
#[cfg(feature = "test-support")]
use url::Url;

use crate::{
    market_participants::{
        credentials::{
            BinanceMarketCredentials, HyperliquidCredentials, LongbridgeCredentials, OkxCredentials,
        },
        error::MarketParticipantsConfigError,
        origins::{
            BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, HyperliquidHttpOrigin,
            HyperliquidWebsocketOrigin, LongbridgeRegion, OkxRegion, OriginPair, OriginRef,
            PolymarketClobOrigin, PolymarketGammaOrigin, PolymarketWebsocketOrigin, ServiceOrigin,
            VenueEndpoints, published_url,
        },
    },
    network::ProxyRoute,
};

pub const BINANCE_SPOT: &str = "spot";
pub const BINANCE_USD_M_PERP: &str = "usd-m-perp";
pub const OKX_SPOT: &str = "spot";
pub const OKX_SWAP: &str = "swap";
pub const HYPERLIQUID_CORE_SPOT: &str = "core-spot";
pub const HYPERLIQUID_CORE_PERP: &str = "core-perp";
pub const LONGBRIDGE_US: &str = "us";
pub const POLYMARKET_PREDICT: &str = "predict";

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
    #[serde(rename = "polymarket")]
    Polymarket(PolymarketSettings),
}

#[cfg(feature = "test-support")]
pub type Bindings = BTreeMap<SmolStr, FixturePair>;

#[cfg(not(feature = "test-support"))]
pub type Bindings = ();

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixturePair {
    pub http: Url,
    pub websocket: Url,
}

#[cfg(feature = "test-support")]
impl FixturePair {
    pub fn origins(&self) -> OriginPair {
        OriginPair {
            http: self.http.clone(),
            websocket: self.websocket.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BinanceSettings {
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    #[serde(rename = "origins", deserialize_with = "deserialize_market_origins")]
    pub origins: BTreeMap<SmolStr, BinanceOrigins>,
    #[serde(rename = "spot_credentials")]
    pub spot_credentials: Option<BinanceMarketCredentials>,
    #[serde(rename = "usd_m_perp_credentials")]
    pub usd_m_perp_credentials: Option<BinanceMarketCredentials>,
    #[serde(skip)]
    pub bindings: Bindings,
}

impl PartialEq for BinanceSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.origins == other.origins
            && self.spot_credentials == other.spot_credentials
            && self.usd_m_perp_credentials == other.usd_m_perp_credentials
    }
}

impl Eq for BinanceSettings {}

#[derive(Deserialize)]
struct BinanceSettingsFile {
    #[serde(rename = "proxy")]
    proxy: Option<ProxyRoute>,
    #[serde(rename = "origins", deserialize_with = "deserialize_market_origins")]
    origins: BTreeMap<SmolStr, BinanceOrigins>,
    #[serde(
        rename = "spot_credentials",
        default,
        deserialize_with = "deserialize_present_credentials"
    )]
    spot_credentials: Option<Option<BinanceMarketCredentials>>,
    #[serde(
        rename = "usd_m_perp_credentials",
        default,
        deserialize_with = "deserialize_present_credentials"
    )]
    usd_m_perp_credentials: Option<Option<BinanceMarketCredentials>>,
}

fn deserialize_present_credentials<'de, D>(
    deserializer: D,
) -> Result<Option<Option<BinanceMarketCredentials>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<BinanceMarketCredentials>::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for BinanceSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let file = BinanceSettingsFile::deserialize(deserializer)?;
        Ok(Self {
            proxy: file.proxy,
            origins: file.origins,
            spot_credentials: file
                .spot_credentials
                .ok_or_else(|| D::Error::missing_field("spot_credentials"))?,
            usd_m_perp_credentials: file
                .usd_m_perp_credentials
                .ok_or_else(|| D::Error::missing_field("usd_m_perp_credentials"))?,
            bindings: Bindings::default(),
        })
    }
}

impl BinanceSettings {
    pub fn market_credentials(&self, market: BinanceMarket) -> Option<&BinanceMarketCredentials> {
        match market {
            BinanceMarket::Spot => self.spot_credentials.as_ref(),
            BinanceMarket::UsdMPerp => self.usd_m_perp_credentials.as_ref(),
        }
    }

    pub fn set_market_credentials(
        &mut self,
        market: BinanceMarket,
        credentials: Option<BinanceMarketCredentials>,
    ) {
        match market {
            BinanceMarket::Spot => self.spot_credentials = credentials,
            BinanceMarket::UsdMPerp => self.usd_m_perp_credentials = credentials,
        }
    }

    pub fn market_origins(&self, market: BinanceMarket) -> Option<BinanceOrigins> {
        self.origins.get(market.scope()).cloned()
    }

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
            spot_credentials: None,
            usd_m_perp_credentials: None,
            bindings: Bindings::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinanceOrigins {
    #[serde(rename = "http")]
    pub http: BinanceHttpOrigin,
    #[serde(rename = "websocket")]
    pub websocket: BinanceWebsocketOrigin,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OkxSettings {
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    #[serde(rename = "region")]
    pub region: OkxRegion,
    #[serde(rename = "credentials")]
    pub credentials: Option<OkxCredentials>,
    #[serde(skip)]
    pub bindings: Bindings,
}

impl PartialEq for OkxSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.region == other.region
            && self.credentials == other.credentials
    }
}

impl Eq for OkxSettings {}

impl OkxSettings {
    pub fn addresses(&self) -> OriginPair {
        #[cfg(feature = "test-support")]
        if let Some(fixture) = self.bindings.get(OKX_SPOT) {
            return fixture.origins();
        }

        self.region.origins()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct HyperliquidSettings {
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    #[serde(rename = "http")]
    pub http: HyperliquidHttpOrigin,
    #[serde(rename = "websocket")]
    pub websocket: HyperliquidWebsocketOrigin,
    #[serde(rename = "credentials")]
    pub credentials: Option<HyperliquidCredentials>,
    #[serde(skip)]
    pub bindings: Bindings,
}

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

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LongbridgeSettings {
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    #[serde(rename = "region")]
    pub region: LongbridgeRegion,
    #[serde(rename = "credentials")]
    pub credentials: Option<LongbridgeCredentials>,
    #[serde(skip)]
    pub bindings: Bindings,
}

impl PartialEq for LongbridgeSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.region == other.region
            && self.credentials == other.credentials
    }
}

impl Eq for LongbridgeSettings {}

impl LongbridgeSettings {
    pub fn addresses(&self) -> OriginPair {
        #[cfg(feature = "test-support")]
        if let Some(fixture) = self.bindings.get(LONGBRIDGE_US) {
            return fixture.origins();
        }

        self.region.origins()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolymarketSettings {
    #[serde(rename = "proxy")]
    pub proxy: Option<ProxyRoute>,
    #[serde(rename = "gamma")]
    pub gamma: PolymarketGammaOrigin,
    #[serde(rename = "clob")]
    pub clob: PolymarketClobOrigin,
    #[serde(rename = "websocket")]
    pub websocket: PolymarketWebsocketOrigin,
    #[serde(skip)]
    pub bindings: Bindings,
}

impl PartialEq for PolymarketSettings {
    fn eq(&self, other: &Self) -> bool {
        self.proxy == other.proxy
            && self.gamma == other.gamma
            && self.clob == other.clob
            && self.websocket == other.websocket
    }
}

impl Eq for PolymarketSettings {}

impl PolymarketSettings {
    pub fn addresses(&self) -> VenueEndpoints {
        #[cfg(feature = "test-support")]
        if let Some(primary) = self.bindings.get(POLYMARKET_PREDICT) {
            return VenueEndpoints {
                primary: primary.origins(),
                metadata: Some(ServiceOrigin {
                    http: self
                        .bindings
                        .get("gamma")
                        .map_or_else(|| primary.http.clone(), |gamma| gamma.http.clone()),
                    websocket: None,
                }),
            };
        }

        VenueEndpoints {
            primary: OriginPair {
                http: published_url(&OriginRef::published(self.clob.as_str())),
                websocket: published_url(&OriginRef::published(self.websocket.as_str())),
            },
            metadata: Some(ServiceOrigin {
                http: published_url(&OriginRef::published(self.gamma.as_str())),
                websocket: None,
            }),
        }
    }
}

impl VenueSettings {
    pub fn scope_ids(&self) -> &'static [&'static str] {
        match self {
            Self::Binance(_) => &[BINANCE_SPOT, BINANCE_USD_M_PERP],
            Self::Hyperliquid(_) => &[HYPERLIQUID_CORE_SPOT, HYPERLIQUID_CORE_PERP],
            Self::Okx(_) => &[OKX_SPOT, OKX_SWAP],
            Self::Longbridge(_) => &[LONGBRIDGE_US],
            Self::Polymarket(_) => &[POLYMARKET_PREDICT],
        }
    }

    pub fn publishes(&self, scope: &str) -> bool {
        self.scope_ids().contains(&scope)
    }

    pub const fn venue_id(&self) -> VenueId {
        match self {
            Self::Binance(_) => VenueId::Binance,
            Self::Hyperliquid(_) => VenueId::Hyperliquid,
            Self::Okx(_) => VenueId::Okx,
            Self::Longbridge(_) => VenueId::Longbridge,
            Self::Polymarket(_) => VenueId::Polymarket,
        }
    }

    pub const fn primary_scope(&self) -> &'static str {
        match self {
            Self::Binance(_) => BINANCE_SPOT,
            Self::Hyperliquid(_) => HYPERLIQUID_CORE_PERP,
            Self::Okx(_) => OKX_SPOT,
            Self::Longbridge(_) => LONGBRIDGE_US,
            Self::Polymarket(_) => POLYMARKET_PREDICT,
        }
    }

    pub fn credential_scope<'a>(&self, scope: &'a str) -> &'a str {
        match self {
            Self::Binance(_) => scope,
            Self::Hyperliquid(_) => HYPERLIQUID_CORE_PERP,
            Self::Okx(_) => OKX_SPOT,
            Self::Longbridge(_) => LONGBRIDGE_US,
            Self::Polymarket(_) => POLYMARKET_PREDICT,
        }
    }

    pub fn origins(&self, scope: &str) -> Option<OriginPair> {
        self.endpoints(scope).map(|endpoints| endpoints.primary)
    }

    pub fn endpoints(&self, scope: &str) -> Option<VenueEndpoints> {
        match self {
            Self::Binance(settings) => BinanceMarket::ALL
                .into_iter()
                .find(|market| market.scope() == scope)
                .map(|market| VenueEndpoints {
                    primary: settings.market_addresses(market),
                    metadata: None,
                }),
            Self::Okx(settings) => matches!(scope, OKX_SPOT | OKX_SWAP).then(|| VenueEndpoints {
                primary: settings.addresses(),
                metadata: None,
            }),
            Self::Longbridge(settings) => (scope == LONGBRIDGE_US).then(|| VenueEndpoints {
                primary: settings.addresses(),
                metadata: None,
            }),
            Self::Hyperliquid(settings) => {
                matches!(scope, HYPERLIQUID_CORE_SPOT | HYPERLIQUID_CORE_PERP).then(|| {
                    VenueEndpoints {
                        primary: settings.addresses(),
                        metadata: None,
                    }
                })
            }
            Self::Polymarket(settings) => {
                (scope == POLYMARKET_PREDICT).then(|| settings.addresses())
            }
        }
    }

    pub fn deployed_origins(&self) -> OriginPair {
        self.origins(self.primary_scope())
            .expect("a venue's primary scope is one of its own statically published scopes")
    }

    pub fn proxy(&self) -> Option<ProxyRoute> {
        match self {
            Self::Binance(settings) => settings.proxy.clone(),
            Self::Hyperliquid(settings) => settings.proxy.clone(),
            Self::Okx(settings) => settings.proxy.clone(),
            Self::Longbridge(settings) => settings.proxy.clone(),
            Self::Polymarket(settings) => settings.proxy.clone(),
        }
    }

    pub fn proxy_mut(&mut self) -> &mut Option<ProxyRoute> {
        match self {
            Self::Binance(settings) => &mut settings.proxy,
            Self::Hyperliquid(settings) => &mut settings.proxy,
            Self::Okx(settings) => &mut settings.proxy,
            Self::Longbridge(settings) => &mut settings.proxy,
            Self::Polymarket(settings) => &mut settings.proxy,
        }
    }

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
            Self::Polymarket(settings) => {
                settings.bindings = [POLYMARKET_PREDICT, "gamma"]
                    .into_iter()
                    .map(bind)
                    .collect();
            }
        }
    }

    pub fn set_proxy(&mut self, proxy: Option<ProxyRoute>) {
        match self {
            Self::Binance(settings) => settings.proxy = proxy,
            Self::Hyperliquid(settings) => settings.proxy = proxy,
            Self::Okx(settings) => settings.proxy = proxy,
            Self::Longbridge(settings) => settings.proxy = proxy,
            Self::Polymarket(settings) => settings.proxy = proxy,
        }
    }

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
                    return Ok(false);
                };
                stored_credentials(
                    settings.market_credentials(market),
                    venue,
                    Some(credential_scope),
                    BinanceMarketCredentials::is_complete,
                )
            }
            Self::Okx(settings) => stored_credentials(
                settings.credentials.as_ref(),
                venue,
                None,
                OkxCredentials::is_complete,
            ),
            Self::Hyperliquid(settings) => stored_credentials(
                settings.credentials.as_ref(),
                venue,
                None,
                HyperliquidCredentials::is_complete,
            ),
            Self::Longbridge(settings) => stored_credentials(
                settings.credentials.as_ref(),
                venue,
                None,
                LongbridgeCredentials::is_complete,
            ),
            Self::Polymarket(_) => Ok(false),
        }
    }
}

fn stored_credentials<T>(
    stored: Option<&T>,
    venue: VenueId,
    scope: Option<&str>,
    is_complete: impl Fn(&T) -> bool,
) -> Result<bool, MarketParticipantsConfigError> {
    match stored {
        None => Ok(false),
        Some(credentials) if is_complete(credentials) => Ok(true),
        Some(_) => Err(MarketParticipantsConfigError::IncompleteCredentials {
            venue,
            scope: scope.map(SmolStr::new),
        }),
    }
}

pub fn default_venues() -> BTreeMap<VenueId, VenueSettings> {
    VenueId::ALL
        .into_iter()
        .map(|venue| {
            let settings = match venue {
                VenueId::Binance => VenueSettings::Binance(BinanceSettings::default()),
                VenueId::Hyperliquid => VenueSettings::Hyperliquid(HyperliquidSettings::default()),
                VenueId::Okx => VenueSettings::Okx(OkxSettings::default()),
                VenueId::Longbridge => VenueSettings::Longbridge(LongbridgeSettings::default()),
                VenueId::Polymarket => VenueSettings::Polymarket(PolymarketSettings::default()),
            };
            (venue, settings)
        })
        .collect()
}

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
    for (venue, settings) in &venues {
        let tagged = settings.venue_id();
        if tagged != *venue {
            return Err(D::Error::custom(format_args!(
                "the venues map names {venue} but its row is tagged {tagged}: every row is stored \
                 under the venue its `venue` tag names"
            )));
        }
    }
    Ok(venues)
}

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

    #[test]
    fn every_built_in_venue_has_its_own_origins() {
        let venues = default_venues();
        assert_eq!(
            venues
                .keys()
                .map(|venue| venue.as_str())
                .collect::<Vec<_>>(),
            ["binance", "hyperliquid", "longbridge", "okx", "polymarket"],
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
            "spot_credentials": null,
            "usd_m_perp_credentials": null,
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
            "spot_credentials": null,
            "usd_m_perp_credentials": null,
        }));
        assert!(
            refused.is_err(),
            "an address no documentation publishes is refused"
        );

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
            "spot_credentials": null,
            "usd_m_perp_credentials": null,
        }));
        assert!(
            borrowed.is_err(),
            "the spot market publishes no futures host, so a file naming one is not this shape"
        );
    }

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
                "spot_credentials": null,
                "usd_m_perp_credentials": null,
            }));

            assert!(
                refused.is_err(),
                "a row that does not configure every market is not this shape"
            );
        }
    }

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
            "spot_credentials": null,
            "usd_m_perp_credentials": null,
        }));

        assert!(
            refused.is_err(),
            "a scope this build does not route is not a settings key"
        );
    }

    fn binance_row() -> serde_json::Value {
        json!({
            "venue": "binance",
            "proxy": null,
            "origins": {
                "spot": {
                    "http": "https://api.binance.com",
                    "websocket": "wss://stream.binance.com:9443",
                },
                "usd-m-perp": {
                    "http": "https://fapi.binance.com",
                    "websocket": "wss://fstream.binance.com",
                },
            },
            "spot_credentials": null,
            "usd_m_perp_credentials": null,
        })
    }

    #[test]
    fn a_binance_row_without_a_market_credential_key_is_refused() {
        for missing in ["spot_credentials", "usd_m_perp_credentials"] {
            let mut row = binance_row();
            row.as_object_mut()
                .expect("a venue row serializes as an object")
                .remove(missing);

            let refused = serde_json::from_value::<VenueSettings>(row.clone());
            let message = refused
                .expect_err("a row without a market's credential key is not this shape")
                .to_string();

            assert!(
                message.contains(missing),
                "the refusal names the key the row is missing: {message}"
            );
        }
    }

    #[test]
    fn a_binance_market_credential_key_may_hold_null() {
        let VenueSettings::Binance(decoded) =
            serde_json::from_value::<VenueSettings>(binance_row()).expect("a null set decodes")
        else {
            panic!("the row is Binance's");
        };

        assert_eq!(decoded.spot_credentials, None);
        assert_eq!(decoded.usd_m_perp_credentials, None);
        assert_eq!(
            serde_json::to_value(VenueSettings::Binance(decoded)).expect("a row serializes")["spot_credentials"],
            json!(null),
            "an unconfigured market is written back as the same null it was read from"
        );
    }

    #[test]
    fn one_binance_market_may_be_configured_without_the_other() {
        let mut row = binance_row();
        row["spot_credentials"] = json!({
            "api_key": "spot-key",
            "private_key": "spot-private",
            "passphrase": "",
        });

        let VenueSettings::Binance(decoded) =
            serde_json::from_value::<VenueSettings>(row).expect("one configured market decodes")
        else {
            panic!("the row is Binance's");
        };

        assert_eq!(
            decoded
                .spot_credentials
                .as_ref()
                .map(|credentials| credentials.api_key.clone()),
            Some(SmolStr::new_static("spot-key"))
        );
        assert_eq!(
            decoded.usd_m_perp_credentials, None,
            "the market the row leaves null is unconfigured"
        );
    }

    #[test]
    fn a_binance_row_of_the_superseded_credential_shape_is_refused() {
        let mut row = binance_row();
        let object = row
            .as_object_mut()
            .expect("a venue row serializes as an object");
        object.remove("spot_credentials");
        object.remove("usd_m_perp_credentials");
        object.insert(
            "credentials".to_owned(),
            json!({
                "spot_api_key": "spot-key",
                "spot_private_key": "spot-private",
                "spot_passphrase": "spot-passphrase",
                "usd_m_perp_api_key": "perp-key",
                "usd_m_perp_private_key": "perp-private",
                "usd_m_perp_passphrase": "perp-passphrase",
            }),
        );

        let refused = serde_json::from_value::<VenueSettings>(row);
        let message = refused
            .expect_err("the six-value credential object is not this shape")
            .to_string();

        assert!(
            message.contains("spot_credentials"),
            "the refusal is the per-market key the old row never named: {message}"
        );
    }

    #[test]
    fn a_venue_row_of_another_shape_should_not_decode() {
        for (wrong, refusal) in [
            (
                json!({
                    "venue": "okx",
                    "proxy": null,
                    "http": "https://api.hyperliquid.xyz/",
                    "websocket": "wss://api.hyperliquid.xyz/ws",
                    "credentials": null,
                }),
                "an OKX row is not addressed, so naming an address does not make one",
            ),
            (
                json!({
                    "venue": "binance",
                    "proxy": null,
                    "region": "global",
                }),
                "a Binance row has two markets of addresses rather than one region",
            ),
            (
                json!({
                    "venue": "hyperliquid",
                    "proxy": null,
                    "origins": {},
                }),
                "a HyperCore row names its own two addresses, and this one names neither",
            ),
            (
                json!({
                    "venue": "longbridge",
                    "proxy": null,
                    "origins": {},
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
                "credentials": null,
            }),
            "a venue nothing was typed into stores no credential set"
        );
        assert_eq!(
            serde_json::to_value(VenueSettings::Okx(OkxSettings::default()))
                .expect("a row serializes"),
            json!({
                "venue": "okx",
                "proxy": null,
                "region": "global",
                "credentials": null,
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

        let binance = serde_json::to_value(VenueSettings::Binance(BinanceSettings::default()))
            .expect("a row serializes");
        assert!(
            binance.get("credentials").is_none(),
            "the venue-wide credential set this shape replaced is gone"
        );
        assert_eq!(
            (
                &binance["spot_credentials"],
                &binance["usd_m_perp_credentials"]
            ),
            (&json!(null), &json!(null)),
            "each market stores its own set, and a fresh installation stores neither"
        );
    }

    #[test]
    fn each_binance_market_stores_its_own_credential_set() {
        let mut settings = BinanceSettings::default();
        assert_eq!(settings.market_credentials(BinanceMarket::Spot), None);
        assert_eq!(settings.market_credentials(BinanceMarket::UsdMPerp), None);

        settings.set_market_credentials(
            BinanceMarket::Spot,
            Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("spot-key"),
                private_key: SmolStr::new_static("spot-private"),
                passphrase: SmolStr::new_static(""),
            }),
        );
        assert_eq!(
            settings
                .market_credentials(BinanceMarket::Spot)
                .map(|credentials| credentials.api_key.clone()),
            Some(SmolStr::new_static("spot-key"))
        );
        assert_eq!(
            settings.market_credentials(BinanceMarket::UsdMPerp),
            None,
            "the sibling market is untouched"
        );

        settings.set_market_credentials(
            BinanceMarket::UsdMPerp,
            Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("perp-key"),
                private_key: SmolStr::new_static("perp-private"),
                passphrase: SmolStr::new_static("perp-passphrase"),
            }),
        );
        settings.set_market_credentials(BinanceMarket::Spot, None);
        assert_eq!(
            settings.market_credentials(BinanceMarket::Spot),
            None,
            "clearing one market clears only that market"
        );
        assert!(
            settings
                .market_credentials(BinanceMarket::UsdMPerp)
                .is_some(),
            "the sibling market keeps the set it holds"
        );

        let encoded =
            serde_json::to_value(VenueSettings::Binance(settings)).expect("a row serializes");
        assert_eq!(
            encoded["spot_credentials"],
            json!(null),
            "a cleared market stores nothing"
        );
        assert_eq!(
            encoded["usd_m_perp_credentials"],
            json!({
                "api_key": "perp-key",
                "private_key": "perp-private",
                "passphrase": "perp-passphrase",
            }),
            "each market's set stores its own three values"
        );
    }

    #[test]
    fn a_stored_set_missing_a_value_is_refused() {
        for refused in [
            json!({ "api_key": "key", "private_key": "private" }),
            json!({ "private_key": "private", "passphrase": "" }),
            json!({ "api_key": "key", "passphrase": "" }),
            json!({ "kind": "binance", "api_key": "key", "private_key": "private" }),
        ] {
            let decoded =
                serde_json::from_value::<Option<BinanceMarketCredentials>>(refused.clone());
            assert!(
                decoded.is_err(),
                "{refused} is not a market's credential set"
            );
        }
        assert_eq!(
            serde_json::from_value::<Option<BinanceMarketCredentials>>(json!({
                "api_key": "key",
                "private_key": "private",
                "passphrase": "",
            }))
            .expect("a complete set decodes"),
            Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("key"),
                private_key: SmolStr::new_static("private"),
                passphrase: SmolStr::new_static(""),
            })
        );
        assert_eq!(
            serde_json::from_value::<Option<BinanceMarketCredentials>>(json!(null))
                .expect("an unconfigured market decodes"),
            None
        );
    }

    #[test]
    fn a_longbridge_row_stores_its_region_beside_its_credentials() {
        assert_eq!(
            serde_json::to_value(VenueSettings::Longbridge(LongbridgeSettings::default()))
                .expect("a row serializes"),
            json!({
                "venue": "longbridge",
                "proxy": null,
                "region": "global",
                "credentials": null,
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
        assert_eq!(
            decoded
                .credentials
                .as_ref()
                .map(LongbridgeCredentials::is_complete),
            Some(true),
            "the stored set is complete, and it is one set rather than three fields"
        );
        assert_eq!(
            decoded.addresses(),
            LongbridgeRegion::China.origins(),
            "the region is what decides where the venue is dialed"
        );
    }

    #[test]
    fn a_region_the_venue_does_not_publish_is_refused() {
        for region in ["us", "eu", "mainland", "China", ""] {
            let refused = serde_json::from_value::<VenueSettings>(json!({
                "venue": "longbridge",
                "proxy": null,
                "region": region,
            }));

            assert!(
                refused.is_err(),
                "{region:?} is not a service region this build speaks"
            );
        }
    }

    #[test]
    fn a_venues_credentials_are_validated_as_one_stored_set() {
        let longbridge = venue("longbridge");
        let okx = venue("okx");
        let hyperliquid = venue("hyperliquid");
        let binance = venue("binance");
        let longbridge_row = |credentials| {
            VenueSettings::Longbridge(LongbridgeSettings {
                credentials,
                ..LongbridgeSettings::default()
            })
        };

        assert_eq!(
            longbridge_row(None).validate_credentials(longbridge, LONGBRIDGE_US),
            Ok(false),
            "a venue nothing was typed into is unconfigured rather than invalid"
        );
        assert_eq!(
            longbridge_row(Some(LongbridgeCredentials {
                app_key: SmolStr::new_static("app-key"),
                app_secret: SmolStr::new_static("app-secret"),
                access_token: SmolStr::new_static(""),
            }))
            .validate_credentials(longbridge, LONGBRIDGE_US),
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue: longbridge,
                scope: None,
            }),
            "a set the user has to finish is refused wherever the venue is asked about"
        );
        assert_eq!(
            longbridge_row(Some(LongbridgeCredentials {
                app_key: SmolStr::new_static("app-key"),
                app_secret: SmolStr::new_static("app-secret"),
                access_token: SmolStr::new_static("access-token"),
            }))
            .validate_credentials(longbridge, LONGBRIDGE_US),
            Ok(true)
        );

        assert_eq!(
            VenueSettings::Okx(OkxSettings {
                credentials: Some(OkxCredentials {
                    api_key: SmolStr::new_static("api-key"),
                    secret_key: SmolStr::new_static(""),
                    passphrase: SmolStr::new_static("passphrase"),
                }),
                ..OkxSettings::default()
            })
            .validate_credentials(okx, OKX_SWAP),
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue: okx,
                scope: None,
            }),
            "every scope of a venue that authenticates as a whole signs with the one set"
        );
        assert_eq!(
            VenueSettings::Hyperliquid(HyperliquidSettings {
                credentials: Some(HyperliquidCredentials {
                    main_account_address: SmolStr::new_static("0xmain"),
                    api_agent_private_key: SmolStr::new_static("agent-private"),
                }),
                ..HyperliquidSettings::default()
            })
            .validate_credentials(hyperliquid, HYPERLIQUID_CORE_PERP),
            Ok(true)
        );

        let spot_only = |spot: Option<BinanceMarketCredentials>| {
            VenueSettings::Binance(BinanceSettings {
                spot_credentials: spot,
                ..BinanceSettings::default()
            })
        };
        assert_eq!(
            spot_only(None).validate_credentials(binance, BINANCE_SPOT),
            Ok(false),
            "a market the operator never started is unconfigured"
        );
        assert_eq!(
            spot_only(Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("spot-key"),
                private_key: SmolStr::new_static(""),
                passphrase: SmolStr::new_static(""),
            }))
            .validate_credentials(binance, BINANCE_SPOT),
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue: binance,
                scope: Some(SmolStr::new_static(BINANCE_SPOT)),
            }),
            "a half-filled market names the market the operator has to finish"
        );
        assert_eq!(
            spot_only(Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("spot-key"),
                private_key: SmolStr::new_static("spot-private"),
                passphrase: SmolStr::new_static(""),
            }))
            .validate_credentials(binance, BINANCE_SPOT),
            Ok(true),
            "the passphrase is optional, so a set without one is complete"
        );
        assert_eq!(
            spot_only(Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("spot-key"),
                private_key: SmolStr::new_static("spot-private"),
                passphrase: SmolStr::new_static(""),
            }))
            .validate_credentials(binance, BINANCE_USD_M_PERP),
            Ok(false),
            "the sibling market was never started, so it is unconfigured"
        );
        assert_eq!(
            spot_only(Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("spot-key"),
                private_key: SmolStr::new_static("spot-private"),
                passphrase: SmolStr::new_static(""),
            }))
            .validate_credentials(binance, "perp-dex/xyz"),
            Ok(false),
            "a scope Binance does not publish holds no credential set at all"
        );
    }

    #[test]
    fn a_venue_credentials_key_is_unconfigured_or_stored_but_incomplete() {
        let rows = [
            (
                "okx",
                json!({
                    "venue": "okx",
                    "proxy": null,
                    "region": "global",
                }),
                venue("okx"),
                OKX_SPOT,
            ),
            (
                "hyperliquid",
                json!({
                    "venue": "hyperliquid",
                    "proxy": null,
                    "http": "https://api.hyperliquid.xyz/",
                    "websocket": "wss://api.hyperliquid.xyz/ws",
                }),
                venue("hyperliquid"),
                HYPERLIQUID_CORE_PERP,
            ),
            (
                "longbridge",
                json!({
                    "venue": "longbridge",
                    "proxy": null,
                    "region": "global",
                }),
                venue("longbridge"),
                LONGBRIDGE_US,
            ),
        ];

        for (name, row, venue, scope) in rows {
            let decoded =
                serde_json::from_value::<VenueSettings>(row.clone()).unwrap_or_else(|error| {
                    panic!("{name}'s row without a credentials key should decode: {error}")
                });
            assert_eq!(
                serde_json::to_value(&decoded).expect("the row should serialize")["credentials"],
                json!(null),
                "{name}'s row names no credential key, so it stores no set"
            );
            assert_eq!(
                decoded.validate_credentials(venue, scope),
                Ok(false),
                "{name} without a credential key is unconfigured, not invalid"
            );

            let mut explicit_null = row;
            explicit_null
                .as_object_mut()
                .expect("a venue row serializes as an object")
                .insert("credentials".to_owned(), json!(null));
            let decoded =
                serde_json::from_value::<VenueSettings>(explicit_null).unwrap_or_else(|error| {
                    panic!("{name}'s row with a null credentials key should decode: {error}")
                });
            assert_eq!(
                serde_json::to_value(&decoded).expect("the row should serialize")["credentials"],
                json!(null),
                "{name}'s explicit null stores no set either"
            );
            assert_eq!(
                decoded.validate_credentials(venue, scope),
                Ok(false),
                "{name}'s null credential set is unconfigured, not invalid"
            );
        }

        let decoded = serde_json::from_value::<VenueSettings>(json!({
            "venue": "okx",
            "proxy": null,
            "region": "global",
            "credentials": {
                "api_key": "",
                "secret_key": "",
                "passphrase": "",
            },
        }))
        .expect("an all-empty object is still this shape");
        let VenueSettings::Okx(settings) = &decoded else {
            panic!("the row is OKX's");
        };
        let stored = settings
            .credentials
            .as_ref()
            .expect("an object that is present is stored, empty values and all");
        assert_eq!(
            (
                stored.api_key.as_str(),
                stored.secret_key.as_str(),
                stored.passphrase.as_str(),
            ),
            ("", "", ""),
            "the stored set is the all-empty object the file names"
        );
        assert_eq!(
            decoded.validate_credentials(venue("okx"), OKX_SPOT),
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue: venue("okx"),
                scope: None,
            }),
            "a stored set missing every value is refused as incomplete"
        );
    }

    #[test]
    fn venue_settings_debug_should_redact_every_stored_secret() {
        let binance = VenueSettings::Binance(BinanceSettings {
            spot_credentials: Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("binance-spot-api-key"),
                private_key: SmolStr::new_static("binance-spot-private-key"),
                passphrase: SmolStr::new_static("binance-spot-passphrase"),
            }),
            usd_m_perp_credentials: Some(BinanceMarketCredentials {
                api_key: SmolStr::new_static("binance-perp-api-key"),
                private_key: SmolStr::new_static("binance-perp-private-key"),
                passphrase: SmolStr::new_static("binance-perp-passphrase"),
            }),
            ..BinanceSettings::default()
        });
        let okx = VenueSettings::Okx(OkxSettings {
            credentials: Some(OkxCredentials {
                api_key: SmolStr::new_static("okx-api-key"),
                secret_key: SmolStr::new_static("okx-secret-key"),
                passphrase: SmolStr::new_static("okx-passphrase"),
            }),
            ..OkxSettings::default()
        });
        let hyperliquid = VenueSettings::Hyperliquid(HyperliquidSettings {
            credentials: Some(HyperliquidCredentials {
                main_account_address: SmolStr::new_static("hypercore-main-account"),
                api_agent_private_key: SmolStr::new_static("hypercore-agent-key"),
            }),
            ..HyperliquidSettings::default()
        });
        let longbridge = VenueSettings::Longbridge(LongbridgeSettings {
            credentials: Some(LongbridgeCredentials {
                app_key: SmolStr::new_static("longbridge-app-key"),
                app_secret: SmolStr::new_static("longbridge-app-secret"),
                access_token: SmolStr::new_static("longbridge-access-token"),
            }),
            ..LongbridgeSettings::default()
        });

        let output = format!("{binance:?} {okx:?} {hyperliquid:?} {longbridge:?}");

        assert!(
            [
                "binance-spot-api-key",
                "binance-spot-private-key",
                "binance-spot-passphrase",
                "binance-perp-api-key",
                "binance-perp-private-key",
                "binance-perp-passphrase",
                "okx-api-key",
                "okx-secret-key",
                "okx-passphrase",
                "hypercore-main-account",
                "hypercore-agent-key",
                "longbridge-app-key",
                "longbridge-app-secret",
                "longbridge-access-token",
            ]
            .iter()
            .all(|secret| !output.contains(secret)),
            "a stored secret never reaches the log: {output}"
        );
    }

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

    #[test]
    fn a_venue_row_under_another_venues_key_is_refused() {
        let mut config = serde_json::to_value(crate::settings::Config::default())
            .expect("the default configuration serializes");
        let venues = config["venues"]
            .as_object_mut()
            .expect("the venues are a map");
        let okx = venues["okx"].clone();
        venues.insert("binance".to_owned(), okx);

        let message = serde_json::from_value::<crate::settings::Config>(config)
            .expect_err("a row under another venue's key is not this shape")
            .to_string();

        assert!(
            message.contains("binance") && message.contains("okx"),
            "the refusal names the key and the row's own tag: {message}"
        );
        assert!(
            message.contains("tagged"),
            "the refusal says what disagrees with the key: {message}"
        );
    }

    #[test]
    fn every_venue_row_is_stored_under_the_key_its_tag_names() {
        let written = serde_json::to_value(crate::settings::Config::default())
            .expect("the default configuration serializes");
        let decoded = serde_json::from_value::<crate::settings::Config>(written)
            .expect("a configuration this build wrote is this shape");

        for (venue, settings) in &decoded.venues {
            assert_eq!(
                settings.venue_id(),
                *venue,
                "the row is stored under the key its own `venue` tag names"
            );
        }
        assert_eq!(decoded.venues, default_venues());
    }

    #[test]
    fn a_venue_names_its_own_line() {
        let mut settings = VenueSettings::Okx(OkxSettings::default());
        assert_eq!(settings.proxy(), None);

        settings.set_proxy(Some(ProxyRoute::named("hk")));
        assert_eq!(settings.proxy(), Some(ProxyRoute::named("hk")));
    }
}
