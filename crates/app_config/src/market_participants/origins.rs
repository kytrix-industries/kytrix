//! The origins every venue this build speaks is reached on.
//!
//! An origin is a fact of the market participant rather than something a user types: the
//! operator *chooses* one of the addresses the venue publishes, and this module is
//! the table those choices come from. Each choice is a variant of a closed enum, so
//! the stored value and the address it stands for are one list — a settings page
//! draws exactly the variants its venue declares, and a value outside them is a
//! configuration file this build refuses rather than a host nobody vetted.
//!
//! The addresses are transcribed from the venues' own API documentation:
//!
//! * Binance spot: `https://api.binance.com` beside the `api1`–`api4` mirrors, and
//!   the streams `wss://stream.binance.com:9443` and `wss://stream.binance.com:443`.
//! * Binance USDⓈ-M futures: `https://fapi.binance.com` and
//!   `wss://fstream.binance.com`.
//! * OKX: one host per service region — global, US & Australia, and the EEA.
//! * Longbridge: one site per service region — the international site beside the
//!   mainland-China one — for requests and for the quote stream.
//! * Hyperliquid: one host for requests and one for streams.
//!
//! OKX and Longbridge state their origins per region rather than per API product, so their
//! tables are keyed by [`OkxRegion`] and [`LongbridgeRegion`], and every scope of either venue
//! is reached through the region the operator selected. Hyperliquid states exactly one address
//! of each kind, so its table holds one variant each: the choice exists because the setting
//! does, and it is a choice with one answer.
//!
//! # Where an address resolves
//!
//! A choice *is* its published address, and a value can only be built from one of the
//! tables here, so a host this build does not publish is not a value it can hold. A
//! test is the one caller that has to reach a *fixture* instead, and no published
//! address names a loopback socket. `crate::market_participants::fixture` is that seam: it
//! replaces where a published address resolves, never what it is called, so a
//! settings file written from a fixture is still a file this build decodes. The seam
//! exists only when the `test-support` feature is on, which in this workspace no crate but a
//! test's dev-dependency edge enables.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use smol_str::SmolStr;
use url::Url;

use crate::market_participants::credentials::BinanceCredentialMarket;

/// The origin pair of one scope of one venue.
///
/// Both origins are required: a venue whose transport is one host for both requests
/// and subscriptions names it twice, so no request path has to decide what an absent
/// origin means.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginPair {
    #[serde(rename = "http")]
    pub http: Url,
    #[serde(rename = "websocket")]
    pub websocket: Url,
}

/// The address one choice of an origin table resolves at.
///
/// The value is the address as the venue publishes it, and it is the only shape a choice
/// has: the tables below are the sole constructors, so a host this build does not publish is
/// not a value that exists. What a settings file holds is a *choice*, which serializes as
/// that same published address — never a variant name, and never a fixture's socket.
///
/// A test that has to reach a *fixture* instead does not change this value: it binds the
/// address to a socket of its own on the settings it builds — see
/// `crate::market_participants::fixture` — so the choice keeps the spelling this build stores and the
/// configuration carries where a request dials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OriginRef(&'static str);

impl OriginRef {
    /// Declares one published address.
    pub(crate) const fn published(published: &'static str) -> Self {
        Self(published)
    }

    /// Returns the address as the venue publishes it.
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

/// Parses one published address.
///
/// Every address the tables below state is a URL this build dials, so a value that is not one
/// is a table this build cannot read rather than anything a caller can cause.
pub(crate) fn published_url(origin: &OriginRef) -> Url {
    Url::parse(origin.as_str()).expect("a published origin is a URL")
}

/// Decodes one choice from the address a file stores.
///
/// An address no table publishes is refused rather than decoded into the nearest
/// match: the choices are the venue's own, and a file that names a host nobody
/// documented is not a file this build knows how to reach.
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

/// Declares one closed set of published origins.
///
/// The variants, the spellings a file stores and the parser are one statement: an address
/// added to the set is a variant, and a file naming an address the set does not hold is
/// refused.
///
/// A table's variants are not all one scope's: Binance's two markets are reached on hosts of
/// their own, so those tables hold both and each market narrows them with `for_market`. What
/// that means for a default is that a table spanning two markets has none — the default of a
/// market's own enum is the market's own host, and stating it once for the whole table would
/// answer the *other* market's address to whichever one asked. A table that is one scope's, or
/// that has one variant, declares its `default` beside the variants.
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
            /// Every address this table offers, in the order it is drawn.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Returns the address as the venue publishes it.
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $origin),+
                }
            }

            /// Returns the choice that publishes `address`, or nothing when none does.
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
    /// One HTTP address the Binance spot API answers on.
    ///
    /// The venue publishes `api.binance.com` first and four mirrors beside it. They
    /// serve the same API — the mirrors exist so a client can spread its request
    /// weight and survive one host being slow — so the choice is which host this
    /// installation addresses, and the venue's USDⓈ-M futures API has one host of
    /// its own.
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
    /// The addresses one market's requests may be sent to.
    pub const fn for_market(market: BinanceMarket) -> &'static [Self] {
        match market {
            BinanceMarket::Spot => &[Self::Api, Self::Api1, Self::Api2, Self::Api3, Self::Api4],
            BinanceMarket::UsdMPerp => &[Self::FuturesApi],
        }
    }
}

published_origins! {
    /// One WebSocket address the Binance spot API streams on.
    ///
    /// The venue publishes the stream on two ports of one host — `9443` and the
    /// `443` a network that blocks the first still carries — and its USDⓈ-M futures
    /// streams on a host of their own.
    BinanceWebsocketOrigin {
        Stream => "wss://stream.binance.com:9443",
        StreamAlt => "wss://stream.binance.com:443",
        FuturesStream => "wss://fstream.binance.com",
    }
}

impl BinanceWebsocketOrigin {
    /// The addresses one market's streams may be opened on.
    pub const fn for_market(market: BinanceMarket) -> &'static [Self] {
        match market {
            BinanceMarket::Spot => &[Self::Stream, Self::StreamAlt],
            BinanceMarket::UsdMPerp => &[Self::FuturesStream],
        }
    }
}

/// The service region an OKX account belongs to.
///
/// OKX publishes one API per region and refuses the others, so the region is what decides
/// where the venue is reached: a global account answers on `openapi.okx.com` and streams on
/// `ws.okx.com`, a US or Australian account — the one registered on the venue's US
/// application — answers on `us.okx.com` and streams on `wsus.okx.com`, and an EEA account
/// answers on `eea.okx.com` and streams on `wseea.okx.com`. Every scope of the venue moves
/// with the region: the venue serves spot and swap from the one host it publishes there.
///
/// The stream addresses are written the way the venue's own reference writes them, with no
/// port: OKX answers its v5 streams on the default TLS port, and the `:8443` a client
/// integration often carries is a second port it also accepts rather than the one its
/// documentation publishes.
///
/// A region is one choice with two addresses, because the venue publishes them together and
/// the operator picks the region they registered in rather than a host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OkxRegion {
    /// The international API.
    #[default]
    Global,
    /// The United States and Australia API.
    UnitedStatesAustralia,
    /// The European Economic Area API.
    Europe,
}

impl OkxRegion {
    /// Every region this table offers, in the order it is drawn.
    pub const ALL: &'static [Self] = &[Self::Global, Self::UnitedStatesAustralia, Self::Europe];

    /// Returns the stored spelling of this region.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::UnitedStatesAustralia => "us-au",
            Self::Europe => "eu",
        }
    }

    /// Returns the request address the venue publishes for this region.
    pub const fn http_str(&self) -> &'static str {
        match self {
            Self::Global => "https://openapi.okx.com",
            Self::UnitedStatesAustralia => "https://us.okx.com",
            Self::Europe => "https://eea.okx.com",
        }
    }

    /// Returns the stream address the venue publishes for this region.
    pub const fn websocket_str(&self) -> &'static str {
        match self {
            Self::Global => "wss://ws.okx.com",
            Self::UnitedStatesAustralia => "wss://wsus.okx.com",
            Self::Europe => "wss://wseea.okx.com",
        }
    }

    /// Returns the origin pair this region is reached on.
    ///
    /// Every scope of the venue is reached through it: OKX publishes its regions rather than
    /// its API products, so the venue's spot and swap scopes differ in nothing but the request
    /// path.
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

/// The service region a Longbridge account belongs to.
///
/// Longbridge publishes one site per region and refuses the other, so the region is what
/// decides where the venue is reached: an account on the international site answers on
/// `openapi.longbridge.com` and streams on `openapi-quote.longbridge.com/v2`, and an account on
/// the mainland-China site answers on `openapi.longbridge.cn` and streams on
/// `openapi-quote.longbridge.cn/v2`. Every scope of the venue moves with the region: the venue
/// serves its one scope from the site it publishes there.
///
/// The stream address is written with the whole path the venue serves its quote stream under,
/// `…/v2`, rather than as a host a request path appends to: the path is part of the published
/// address, and the venue names no stream outside it.
///
/// A region is one choice with two addresses, because the venue publishes them together and
/// the operator picks the site their account was opened on rather than a host. Choosing the
/// two roles apart would let an installation state a pair the venue never publishes — the
/// request role of one region beside the stream role of another — which is why there is one
/// choice here rather than two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LongbridgeRegion {
    /// The international site.
    #[default]
    Global,
    /// The mainland-China site.
    China,
}

impl LongbridgeRegion {
    /// Every region this table offers, in the order it is drawn.
    pub const ALL: &'static [Self] = &[Self::Global, Self::China];

    /// Returns the stored spelling of this region.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::China => "china",
        }
    }

    /// Returns the request address the venue publishes for this region.
    pub const fn http_str(&self) -> &'static str {
        match self {
            Self::Global => "https://openapi.longbridge.com",
            Self::China => "https://openapi.longbridge.cn",
        }
    }

    /// Returns the stream address the venue publishes for this region.
    pub const fn websocket_str(&self) -> &'static str {
        match self {
            Self::Global => "wss://openapi-quote.longbridge.com/v2",
            Self::China => "wss://openapi-quote.longbridge.cn/v2",
        }
    }

    /// Returns the origin pair this region is reached on.
    ///
    /// Every scope of the venue is reached through it: the venue publishes its regions rather
    /// than its API products, so its one scope differs in nothing but the request path.
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
    /// One HTTP address Hyperliquid answers on.
    ///
    /// Hyperliquid publishes exactly one production host for requests, so the page offers the
    /// one address `https://api.hyperliquid.xyz/` and this build reaches nothing else.
    HyperliquidHttpOrigin: Api {
        Api => "https://api.hyperliquid.xyz/",
    }
}

published_origins! {
    /// One WebSocket address Hyperliquid streams on.
    ///
    /// Hyperliquid publishes exactly one production host for streams, so the page offers the one
    /// address `wss://api.hyperliquid.xyz/ws` and this build opens streams on nothing else.
    ///
    /// The stream address is the whole address rather than the host it belongs to: the venue
    /// serves its streams under a path of their own, so `wss://api.hyperliquid.xyz/` — the host
    /// without that path — is not an address it answers on. Every other venue in this table
    /// publishes a stream *host* whose path the request path appends, which is why this is the
    /// one entry that carries a path.
    HyperliquidWebsocketOrigin: Api {
        Api => "wss://api.hyperliquid.xyz/ws",
    }
}

/// The API product a Binance market serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinanceMarket {
    /// The spot API.
    Spot,
    /// The USDⓈ-margined futures API.
    UsdMPerp,
}

impl BinanceMarket {
    /// Every Binance market this build speaks, in the order its page draws them.
    pub const ALL: [Self; 2] = [Self::Spot, Self::UsdMPerp];

    /// Returns the canonical name of the scope this market is routed by.
    pub const fn scope(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::UsdMPerp => "usd-m-perp",
        }
    }

    /// Returns the credential set this market signs with.
    pub const fn credential_market(self) -> BinanceCredentialMarket {
        match self {
            Self::Spot => BinanceCredentialMarket::Spot,
            Self::UsdMPerp => BinanceCredentialMarket::UsdMPerp,
        }
    }

    /// Returns the HTTP address this market's requests are sent to while the operator
    /// chooses none.
    ///
    /// The table spans both markets, so a market's own address is stated here rather than
    /// as a default on the table: one `Default` for both would answer the sibling's host.
    pub const fn http(self) -> BinanceHttpOrigin {
        match self {
            Self::Spot => BinanceHttpOrigin::Api,
            Self::UsdMPerp => BinanceHttpOrigin::FuturesApi,
        }
    }

    /// Returns the WebSocket address this market's streams are opened on while the operator
    /// chooses none.
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

    /// Every market reaches the addresses its own API publishes, and no market borrows its
    /// sibling's: a Binance spot request sent to the futures API is a request to the wrong
    /// product, and that is what the per-market narrowing in the decoder and the page exists to
    /// prevent.
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

    /// The addresses one market is reached on, as the settings resolve them for a market that
    /// is configured exactly as a fresh installation configures it.
    fn market_address(market: BinanceMarket) -> OriginPair {
        BinanceOrigins {
            http: market.http(),
            websocket: market.websocket(),
        }
        .addresses()
    }

    /// The two Binance markets reach disjoint hosts, and the mirrors of the spot API are all
    /// offered: an address a market may be reached on is one of that market's own choices.
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

    /// Every region of OKX answers a host of its own, one per role, and no two regions share
    /// one.
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

    /// Every region of Longbridge answers a site of its own, one per role, and no two regions
    /// share one.
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

    /// Hyperliquid offers exactly one address of each kind, and it is the one the venue
    /// publishes.
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

    /// A stored choice decodes back to the variant it was written from, and an address this
    /// build does not publish is refused.
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

    /// A choice is looked up by the address it publishes, and an address no table states names
    /// no choice.
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

    /// The origin pair is one scope's two addresses, under the names the request path reads them
    /// by.
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
