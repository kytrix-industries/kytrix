use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use smol_str::SmolStr;
use url::Url;

use crate::market_participants::credentials::BinanceCredentialMarket;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginPair {
    #[serde(rename = "http")]
    pub http: Url,
    #[serde(rename = "websocket")]
    pub websocket: Url,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceOrigin {
    pub http: Url,
    pub websocket: Option<Url>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueEndpoints {
    pub primary: OriginPair,
    pub metadata: Option<ServiceOrigin>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OriginRef(&'static str);

impl OriginRef {
    pub(crate) const fn published(published: &'static str) -> Self {
        Self(published)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

pub(crate) fn published_url(origin: &OriginRef) -> Url {
    Url::parse(origin.as_str()).expect("a published origin is a URL")
}

fn deserialize_origin<'de, D>(
    deserializer: D,
    published: &'static [&'static str],
) -> Result<OriginRef, D::Error>
where
    D: Deserializer<'de>,
{
    let value = SmolStr::deserialize(deserializer)?;
    published
        .iter()
        .find(|spelling| **spelling == value.as_str())
        .map(|spelling| OriginRef::published(spelling))
        .ok_or_else(|| {
            D::Error::custom(format_args!(
                "{value:?} is not an address this build publishes; its addresses are {published:?}"
            ))
        })
}

macro_rules! published_origins {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $origin:literal),+ $(,)? }) => {
        published_origins! {
            @table $(#[$meta])* $name { $($variant => $origin),+ }
        }
    };
    ($(#[$meta:meta])* $name:ident : $default:ident { $($variant:ident => $origin:literal),+ $(,)? }) => {
        published_origins! {
            @table $(#[$meta])* $name { $($variant => $origin),+ }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }
    };
    (@table $(#[$meta:meta])* $name:ident { $($variant:ident => $origin:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $origin),+
                }
            }

            pub fn from_url(address: &str) -> Option<Self> {
                Self::ALL
                    .iter()
                    .find(|choice| choice.as_str() == address)
                    .copied()
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let origin = deserialize_origin(deserializer, &[$($origin),+])?;
                Ok(match origin.as_str() {
                    $($origin => Self::$variant,)+
                    _ => unreachable!("the parser answered a spelling of this table"),
                })
            }
        }
    };
}

published_origins! {
    BinanceHttpOrigin {
        Api => "https://api.binance.com",
        Api1 => "https://api1.binance.com",
        Api2 => "https://api2.binance.com",
        Api3 => "https://api3.binance.com",
        Api4 => "https://api4.binance.com",
        FuturesApi => "https://fapi.binance.com",
    }
}

impl BinanceHttpOrigin {
    pub const fn for_market(market: BinanceMarket) -> &'static [Self] {
        match market {
            BinanceMarket::Spot => &[Self::Api, Self::Api1, Self::Api2, Self::Api3, Self::Api4],
            BinanceMarket::UsdMPerp => &[Self::FuturesApi],
        }
    }
}

published_origins! {
    BinanceWebsocketOrigin {
        Stream => "wss://stream.binance.com:9443",
        StreamAlt => "wss://stream.binance.com:443",
        FuturesStream => "wss://fstream.binance.com",
    }
}

impl BinanceWebsocketOrigin {
    pub const fn for_market(market: BinanceMarket) -> &'static [Self] {
        match market {
            BinanceMarket::Spot => &[Self::Stream, Self::StreamAlt],
            BinanceMarket::UsdMPerp => &[Self::FuturesStream],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OkxRegion {
    #[default]
    Global,
    UnitedStatesAustralia,
    Europe,
}

impl OkxRegion {
    pub const ALL: &'static [Self] = &[Self::Global, Self::UnitedStatesAustralia, Self::Europe];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::UnitedStatesAustralia => "us-au",
            Self::Europe => "eu",
        }
    }

    pub const fn http_str(&self) -> &'static str {
        match self {
            Self::Global => "https://openapi.okx.com",
            Self::UnitedStatesAustralia => "https://us.okx.com",
            Self::Europe => "https://eea.okx.com",
        }
    }

    pub const fn websocket_str(&self) -> &'static str {
        match self {
            Self::Global => "wss://ws.okx.com",
            Self::UnitedStatesAustralia => "wss://wsus.okx.com",
            Self::Europe => "wss://wseea.okx.com",
        }
    }

    pub fn origins(&self) -> OriginPair {
        OriginPair {
            http: published_url(&OriginRef::published(self.http_str())),
            websocket: published_url(&OriginRef::published(self.websocket_str())),
        }
    }
}

impl Serialize for OkxRegion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OkxRegion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        static PUBLISHED: [&str; 3] = ["global", "us-au", "eu"];
        let value = SmolStr::deserialize(deserializer)?;
        PUBLISHED
            .iter()
            .position(|spelling| *spelling == value.as_str())
            .map(|index| Self::ALL[index])
            .ok_or_else(|| {
                D::Error::custom(format_args!(
                    "{value:?} is not a service region this build speaks; it speaks {PUBLISHED:?}"
                ))
            })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LongbridgeRegion {
    #[default]
    Global,
    China,
}

impl LongbridgeRegion {
    pub const ALL: &'static [Self] = &[Self::Global, Self::China];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::China => "china",
        }
    }

    pub const fn http_str(&self) -> &'static str {
        match self {
            Self::Global => "https://openapi.longbridge.com",
            Self::China => "https://openapi.longbridge.cn",
        }
    }

    pub const fn websocket_str(&self) -> &'static str {
        match self {
            Self::Global => "wss://openapi-quote.longbridge.com/v2",
            Self::China => "wss://openapi-quote.longbridge.cn/v2",
        }
    }

    pub fn origins(&self) -> OriginPair {
        OriginPair {
            http: published_url(&OriginRef::published(self.http_str())),
            websocket: published_url(&OriginRef::published(self.websocket_str())),
        }
    }
}

impl Serialize for LongbridgeRegion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LongbridgeRegion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        static PUBLISHED: [&str; 2] = ["global", "china"];
        let value = SmolStr::deserialize(deserializer)?;
        PUBLISHED
            .iter()
            .position(|spelling| *spelling == value.as_str())
            .map(|index| Self::ALL[index])
            .ok_or_else(|| {
                D::Error::custom(format_args!(
                    "{value:?} is not a service region this build speaks; it speaks {PUBLISHED:?}"
                ))
            })
    }
}

published_origins! {
    PolymarketGammaOrigin: Api {
        Api => "https://gamma-api.polymarket.com",
    }
}

published_origins! {
    PolymarketClobOrigin: Api {
        Api => "https://clob.polymarket.com",
    }
}

published_origins! {
    PolymarketWebsocketOrigin: Api {
        Api => "wss://ws-subscriptions-clob.polymarket.com",
    }
}

published_origins! {
    HyperliquidHttpOrigin: Api {
        Api => "https://api.hyperliquid.xyz/",
    }
}

published_origins! {
    HyperliquidWebsocketOrigin: Api {
        Api => "wss://api.hyperliquid.xyz/ws",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinanceMarket {
    Spot,
    UsdMPerp,
}

impl BinanceMarket {
    pub const ALL: [Self; 2] = [Self::Spot, Self::UsdMPerp];

    pub const fn scope(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::UsdMPerp => "usd-m-perp",
        }
    }

    pub const fn credential_market(self) -> BinanceCredentialMarket {
        match self {
            Self::Spot => BinanceCredentialMarket::Spot,
            Self::UsdMPerp => BinanceCredentialMarket::UsdMPerp,
        }
    }

    pub const fn http(self) -> BinanceHttpOrigin {
        match self {
            Self::Spot => BinanceHttpOrigin::Api,
            Self::UsdMPerp => BinanceHttpOrigin::FuturesApi,
        }
    }

    pub const fn websocket(self) -> BinanceWebsocketOrigin {
        match self {
            Self::Spot => BinanceWebsocketOrigin::Stream,
            Self::UsdMPerp => BinanceWebsocketOrigin::FuturesStream,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_participants::settings::BinanceOrigins;

    #[test]
    fn every_binance_market_is_reached_on_its_own_hosts() {
        for market in BinanceMarket::ALL {
            assert!(
                BinanceHttpOrigin::for_market(market).contains(&market.http()),
                "{} offers the address it is reached on among its choices",
                market.scope()
            );
            assert!(
                BinanceWebsocketOrigin::for_market(market).contains(&market.websocket()),
                "{} offers the address it streams on among its choices",
                market.scope()
            );
            assert_eq!(
                market_address(market).http.as_str(),
                format!("{}/", market.http().as_str()),
                "{} dials the HTTP address it declares",
                market.scope()
            );
        }

        assert_eq!(
            (
                market_address(BinanceMarket::Spot).http.as_str(),
                market_address(BinanceMarket::Spot).websocket.as_str(),
            ),
            ("https://api.binance.com/", "wss://stream.binance.com:9443/"),
            "the spot market is reached on the venue's primary hosts"
        );
        assert_eq!(
            (
                market_address(BinanceMarket::UsdMPerp).http.as_str(),
                market_address(BinanceMarket::UsdMPerp).websocket.as_str(),
            ),
            ("https://fapi.binance.com/", "wss://fstream.binance.com/"),
            "the USD-M market is reached on the futures hosts"
        );
    }

    fn market_address(market: BinanceMarket) -> OriginPair {
        BinanceOrigins {
            http: market.http(),
            websocket: market.websocket(),
        }
        .addresses()
    }

    #[test]
    fn binance_markets_offer_disjoint_hosts() {
        let spot: Vec<&str> = BinanceHttpOrigin::for_market(BinanceMarket::Spot)
            .iter()
            .map(BinanceHttpOrigin::as_str)
            .collect();
        let perp: Vec<&str> = BinanceHttpOrigin::for_market(BinanceMarket::UsdMPerp)
            .iter()
            .map(BinanceHttpOrigin::as_str)
            .collect();

        assert_eq!(
            spot,
            [
                "https://api.binance.com",
                "https://api1.binance.com",
                "https://api2.binance.com",
                "https://api3.binance.com",
                "https://api4.binance.com",
            ],
            "the spot API publishes its primary host beside four mirrors"
        );
        assert_eq!(
            perp,
            ["https://fapi.binance.com"],
            "the USD-M API publishes one host"
        );
        assert!(
            spot.iter().all(|host| !perp.contains(host)),
            "no host is offered to both markets"
        );
        assert_eq!(
            BinanceWebsocketOrigin::ALL.len(),
            3,
            "the stream table is the two spot ports and the futures host"
        );
    }

    #[test]
    fn every_okx_region_answers_its_own_hosts() {
        let mut hosts: Vec<&str> = Vec::with_capacity(OkxRegion::ALL.len());
        for region in OkxRegion::ALL {
            let origins = region.origins();
            assert_eq!(origins.http.as_str(), format!("{}/", region.http_str()));
            assert_eq!(
                origins.websocket.as_str(),
                format!("{}/", region.websocket_str())
            );
            assert!(
                region.websocket_str().starts_with("wss://"),
                "a stream address of one region is a stream address"
            );
            hosts.push(region.http_str());
        }
        hosts.sort_unstable();
        hosts.dedup();
        assert_eq!(
            hosts.len(),
            OkxRegion::ALL.len(),
            "no two regions share a host"
        );
        assert_eq!(
            OkxRegion::default(),
            OkxRegion::Global,
            "an installation that made no choice speaks to the international API"
        );
    }

    #[test]
    fn every_longbridge_region_answers_its_own_hosts() {
        let mut hosts: Vec<&str> = Vec::with_capacity(LongbridgeRegion::ALL.len());
        for region in LongbridgeRegion::ALL {
            let origins = region.origins();
            assert_eq!(origins.http.as_str(), format!("{}/", region.http_str()));
            assert_eq!(
                origins.websocket.as_str(),
                region.websocket_str(),
                "a region's stream address is the venue's own, path included"
            );
            assert!(
                region.websocket_str().starts_with("wss://"),
                "a stream address of one region is a stream address"
            );
            hosts.push(region.http_str());
        }
        hosts.sort_unstable();
        hosts.dedup();
        assert_eq!(
            hosts.len(),
            LongbridgeRegion::ALL.len(),
            "no two regions share a host"
        );
        assert_eq!(
            LongbridgeRegion::default(),
            LongbridgeRegion::Global,
            "an installation that made no choice speaks to the international site"
        );
        assert_eq!(
            (
                LongbridgeRegion::Global.http_str(),
                LongbridgeRegion::Global.websocket_str(),
            ),
            (
                "https://openapi.longbridge.com",
                "wss://openapi-quote.longbridge.com/v2"
            ),
            "the international site is the one the venue documents first"
        );
    }

    #[test]
    fn hyperliquid_offers_one_address_of_each_kind() {
        assert_eq!(HyperliquidHttpOrigin::ALL.len(), 1);
        assert_eq!(HyperliquidWebsocketOrigin::ALL.len(), 1);
        assert_eq!(
            HyperliquidHttpOrigin::default().as_str(),
            "https://api.hyperliquid.xyz/"
        );
        assert_eq!(
            HyperliquidWebsocketOrigin::default().as_str(),
            "wss://api.hyperliquid.xyz/ws"
        );
    }

    #[test]
    fn a_published_origin_round_trips_and_an_unpublished_one_is_refused() {
        for origin in BinanceHttpOrigin::ALL {
            let wire = serde_json::to_value(origin).expect("a choice serializes");
            assert_eq!(wire, serde_json::json!(origin.as_str()));
            assert_eq!(
                serde_json::from_value::<BinanceHttpOrigin>(wire).expect("a choice decodes"),
                *origin
            );
        }
        for origin in BinanceWebsocketOrigin::ALL {
            let wire = serde_json::to_value(origin).expect("a choice serializes");
            assert_eq!(
                serde_json::from_value::<BinanceWebsocketOrigin>(wire).expect("a choice decodes"),
                *origin
            );
        }
        for region in OkxRegion::ALL {
            let wire = serde_json::to_value(region).expect("a region serializes");
            assert_eq!(wire, serde_json::json!(region.as_str()));
            assert_eq!(
                serde_json::from_value::<OkxRegion>(wire).expect("a region decodes"),
                *region
            );
        }
        for region in LongbridgeRegion::ALL {
            let wire = serde_json::to_value(region).expect("a region serializes");
            assert_eq!(wire, serde_json::json!(region.as_str()));
            assert_eq!(
                serde_json::from_value::<LongbridgeRegion>(wire).expect("a region decodes"),
                *region
            );
        }

        for refused in [
            "https://api.binance.us",
            "https://data-api.binance.vision",
            "https://api.binance.com/",
            "wss://fstream.binance.com",
        ] {
            assert!(
                serde_json::from_value::<BinanceHttpOrigin>(serde_json::json!(refused)).is_err(),
                "{refused} is not an address the Binance page offers"
            );
        }
        assert!(
            serde_json::from_value::<OkxRegion>(serde_json::json!("us")).is_err(),
            "a region this build does not publish is refused"
        );
        assert!(
            serde_json::from_value::<LongbridgeRegion>(serde_json::json!("us")).is_err(),
            "a region this build does not publish is refused"
        );
    }

    #[test]
    fn a_choice_is_found_by_the_address_it_publishes() {
        for origin in BinanceHttpOrigin::ALL {
            assert_eq!(BinanceHttpOrigin::from_url(origin.as_str()), Some(*origin));
        }
        for origin in BinanceWebsocketOrigin::ALL {
            assert_eq!(
                BinanceWebsocketOrigin::from_url(origin.as_str()),
                Some(*origin)
            );
        }
        assert_eq!(
            HyperliquidHttpOrigin::from_url("https://spot.example"),
            None,
            "an address no table publishes names no choice"
        );
    }

    #[test]
    fn an_origin_pair_stores_both_members() {
        let pair = market_address(BinanceMarket::Spot);

        assert_eq!(
            serde_json::to_value(&pair).expect("a pair serializes"),
            serde_json::json!({
                "http": "https://api.binance.com/",
                "websocket": "wss://stream.binance.com:9443/",
            })
        );
    }
}
