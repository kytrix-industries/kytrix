//! Hyperliquid wallet-monitor requests and their native payloads.
//!
//! The position a wallet request reads is the one its `WalletMonitorTarget`
//! name: a public address read never resolves a trading credential, and a
//! route-level stream carries no address at all.

use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, Quantity, SignedQuantity, UnixMillis},
    error::ModelError,
    identifier::WalletAddress,
    listing::route::{VenueListingKey, VenueRoute},
    market::units::Side,
};

use crate::{
    account::shared::{OrderKind, OrderStatus},
    protocol::WalletMonitorTarget,
};

/// Hyperliquid wallet query identity.
///
/// This is a runtime request, not a wire payload: the adapter encodes its own
/// subscription body from the query, so the type carries no serde derive at all. That is
/// also what keeps `decoding_routes` unrepresentable as empty — deserialization would be
/// the one way past the constructors that refuse an empty set, and there is none. If a
/// query ever does need to travel as data, the boundary has to refuse an empty set too.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HyperliquidWalletQuery {
    pub target: WalletMonitorTarget,
    /// The routes whose published catalogs may decode this query's answer.
    ///
    /// `target` decides what is validated and where the request goes; this set decides
    /// which catalog an answered row is resolved against. They are not the same set:
    /// `userFills`, `userNonFundingLedgerUpdates`, `historicalOrders` and
    /// `fastAssetCtxs` are venue-wide reads — their wire requests name no scope — and
    /// their rows are the coins of every scope the venue serves. A reader that monitors
    /// several routes therefore hands over all of them, and a row is decoded with the
    /// catalog of the route its own listing belongs to.
    ///
    /// Never empty, and always names `target`'s own route: a query is built through the
    /// constructors below, which cannot express anything else. The field is private for
    /// exactly that reason — a query that named no route would read a venue-wide answer
    /// and resolve every row of it to nothing.
    decoding_routes: Vec<VenueRoute>,
}

impl HyperliquidWalletQuery {
    /// Builds a query about one public wallet address on one route.
    pub fn public(route: VenueRoute, address: WalletAddress) -> Self {
        Self {
            target: WalletMonitorTarget::Public {
                route: route.clone(),
                address,
            },
            decoding_routes: vec![route],
        }
    }

    /// Builds a query about one public wallet address decoded by several routes.
    ///
    /// The request is addressed to the first route — its validation and transport are
    /// what the query uses — while the whole set is what the answer is decoded with.
    /// The set is the caller's monitored routes rather than the addressed route alone:
    /// one address is read once per data kind, and an answered row belongs to whichever
    /// route published its listing.
    ///
    /// # Errors
    ///
    /// An empty set is refused: a query with nothing to decode against would read an
    /// answer and publish no row of it.
    pub fn public_across(
        decoding_routes: Vec<VenueRoute>,
        address: WalletAddress,
    ) -> Result<Self, ModelError> {
        let Some(route) = decoding_routes.first().cloned() else {
            return Err(ModelError::EmptyCollection {
                field: "decoding routes",
            });
        };
        Ok(Self {
            target: WalletMonitorTarget::Public { route, address },
            decoding_routes,
        })
    }

    /// Builds a route-level query decoded by several routes.
    ///
    /// A route-level read is the venue-wide one — the fast asset contexts — so the set
    /// is what its payloads are decoded with, exactly as for a public address read.
    ///
    /// # Errors
    ///
    /// An empty set is refused, for the same reason as [`Self::public_across`].
    pub fn route_level_across(decoding_routes: Vec<VenueRoute>) -> Result<Self, ModelError> {
        let Some(route) = decoding_routes.first().cloned() else {
            return Err(ModelError::EmptyCollection {
                field: "decoding routes",
            });
        };
        Ok(Self {
            target: WalletMonitorTarget::Route { route },
            decoding_routes,
        })
    }

    /// Returns the routes this query's answer is decoded with.
    pub fn decoding_routes(&self) -> &[VenueRoute] {
        &self.decoding_routes
    }

    /// Returns the route this query reads.
    pub fn route(&self) -> &VenueRoute {
        self.target.route()
    }

    /// Returns the wallet-monitor target this query reads.
    pub const fn wallet_target(&self) -> &WalletMonitorTarget {
        &self.target
    }
}

/// A bounded Hyperliquid user-fills window.
///
/// Like [`HyperliquidWalletQuery`], this is a runtime request rather than a wire payload
/// and carries no serde derive: that is what keeps its route set unrepresentable as empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HyperliquidUserFillsByTimeQuery {
    pub target: WalletMonitorTarget,
    /// The routes whose published catalogs may decode this window's answer, on the same
    /// terms as [`HyperliquidWalletQuery::decoding_routes`]: a fills window is a
    /// venue-wide read, so every monitored route's listings may appear in it. Private for
    /// the same reason — only the constructor below builds one.
    decoding_routes: Vec<VenueRoute>,
    pub start_timestamp: UnixMillis,
    pub end_timestamp: UnixMillis,
}

impl HyperliquidUserFillsByTimeQuery {
    pub const MAX_WINDOW_MILLIS: UnixMillis = 6 * 60 * 60 * 1_000;

    /// Builds a window about one public wallet address decoded by several routes.
    ///
    /// As in [`HyperliquidWalletQuery::public_across`], the request is addressed to the
    /// first route and the whole set decodes the answer.
    ///
    /// # Errors
    ///
    /// An empty set is refused: a window with nothing to decode against would read an
    /// answer and publish no row of it.
    pub fn for_routes(
        decoding_routes: Vec<VenueRoute>,
        address: WalletAddress,
        start_timestamp: UnixMillis,
        end_timestamp: UnixMillis,
    ) -> Result<Self, ModelError> {
        let Some(route) = decoding_routes.first().cloned() else {
            return Err(ModelError::EmptyCollection {
                field: "decoding routes",
            });
        };
        Ok(Self {
            target: WalletMonitorTarget::Public { route, address },
            decoding_routes,
            start_timestamp,
            end_timestamp,
        })
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.start_timestamp < 0 {
            return Err(ModelError::InvalidTimestamp { field: "start" });
        }
        if self.end_timestamp < 0 {
            return Err(ModelError::InvalidTimestamp { field: "end" });
        }
        if self.start_timestamp > self.end_timestamp {
            return Err(ModelError::InvalidTimeRange);
        }
        let window = self
            .end_timestamp
            .checked_sub(self.start_timestamp)
            .ok_or(ModelError::InvalidTimeRange)?;
        if window > Self::MAX_WINDOW_MILLIS {
            return Err(ModelError::TimeWindowTooLarge {
                maximum: Self::MAX_WINDOW_MILLIS,
            });
        }
        Ok(())
    }

    /// Returns the routes this window's answer is decoded with.
    pub fn decoding_routes(&self) -> &[VenueRoute] {
        &self.decoding_routes
    }

    /// Returns the route this window reads.
    pub fn route(&self) -> &VenueRoute {
        self.target.route()
    }

    /// Returns the wallet-monitor target this query reads.
    pub const fn wallet_target(&self) -> &WalletMonitorTarget {
        &self.target
    }
}

/// One spot balance of a monitored wallet, in its own token.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidSpotBalance {
    pub coin: AssetId,
    pub total: SignedQuantity,
    pub hold: SignedQuantity,
}

/// The spot state of one monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidSpotState {
    pub balances: Vec<NativeHyperliquidSpotBalance>,
}

/// One spot pair a Hyperliquid route publishes.
///
/// The pair is metadata about a route's token namespace, not about one tradable
/// listing: `asset_id` is the venue's asset index and the listing key is what
/// carries the tradable identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidSpotPair {
    pub coin: market_participants_model::identifier::IdentifierValue,
    pub base_token: AssetId,
    pub quote_token: AssetId,
    pub asset_id: market_participants_model::listing::venue::HyperliquidAssetId,
}

/// One fast asset context of a route-level stream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidAssetContext {
    pub listing: VenueListingKey,
    pub mark_price: Option<Price>,
    pub mid_price: Option<Price>,
}

/// One perpetual position of a monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidPerpetualPosition {
    pub listing: VenueListingKey,
    pub size: SignedQuantity,
    pub entry_price: Option<Price>,
    pub mark_price: Option<Price>,
    pub liquidation_price: Option<Price>,
    pub position_value: Option<Quantity>,
    pub unrealized_pnl: ProfitOrLoss,
    pub margin: Option<Quantity>,
    pub funding_since_open: ProfitOrLoss,
}

/// The perpetual state of one monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidPerpetualState {
    pub positions: Vec<NativeHyperliquidPerpetualPosition>,
}

/// One open order of a monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidOpenOrder {
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: Quantity,
    pub price: Price,
    pub status: OrderStatus,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub timestamp: UnixMillis,
    pub order_id: market_participants_model::identifier::IdentifierValue,
}

/// One historical order of a monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidHistoricalOrder {
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: Quantity,
    pub price: Price,
    pub kind: OrderKind,
    pub reduce_only: bool,
    /// The venue's own order-state label, which is not one of [`OrderStatus`]'s.
    pub status: market_participants_model::identifier::IdentifierValue,
    pub timestamp: UnixMillis,
    pub order_id: market_participants_model::identifier::IdentifierValue,
}

/// One fill of a monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidUserFill {
    pub address: WalletAddress,
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: Quantity,
    pub price: Price,
    pub timestamp: UnixMillis,
    pub start_position: SignedQuantity,
    pub closed_pnl: ProfitOrLoss,
    pub fee: ProfitOrLoss,
    /// The asset the fee was charged in.
    pub fee_asset: AssetId,
    pub hash: Option<market_participants_model::identifier::IdentifierValue>,
    pub tid: u64,
    pub liquidation: bool,
    pub direction: Option<crate::account::shared::HyperliquidFillDirection>,
}

/// One ledger event of a monitored wallet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidLedgerEvent {
    pub address: WalletAddress,
    pub timestamp: UnixMillis,
    pub hash: market_participants_model::identifier::IdentifierValue,
    /// The asset the entry is denominated in, when it names one.
    pub coin: Option<AssetId>,
    pub kind: market_participants_model::identifier::IdentifierValue,
    pub amount: Option<SignedQuantity>,
    pub value: Option<SignedQuantity>,
    pub transfer_direction: Option<HyperliquidTransferDirection>,
    pub related_wallet: Option<WalletAddress>,
    pub liquidated_notional: Option<SignedQuantity>,
    pub liquidated_positions: Vec<NativeHyperliquidLiquidatedPosition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HyperliquidTransferDirection {
    #[serde(rename = "in")]
    Incoming,
    #[serde(rename = "out")]
    Outgoing,
}

/// One liquidated position of a ledger event.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidLiquidatedPosition {
    pub listing: VenueListingKey,
    pub size: SignedQuantity,
}

/// One update of a Hyperliquid wallet stream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NativeHyperliquidWalletUpdate {
    #[serde(rename = "user_fills")]
    UserFills {
        #[serde(rename = "is_snapshot")]
        is_snapshot: bool,
        #[serde(rename = "fills")]
        fills: Vec<NativeHyperliquidUserFill>,
    },
    #[serde(rename = "user_ledger")]
    UserLedger {
        #[serde(rename = "is_snapshot")]
        is_snapshot: bool,
        #[serde(rename = "events")]
        events: Vec<NativeHyperliquidLedgerEvent>,
    },
    #[serde(rename = "fast_asset_contexts")]
    FastAssetContexts {
        #[serde(rename = "contexts")]
        contexts: Vec<NativeHyperliquidAssetContext>,
    },
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        error::ModelError,
        identifier::{VenueId, VenueScopeId, WalletAddress},
        listing::route::VenueRoute,
    };

    use super::{HyperliquidUserFillsByTimeQuery, HyperliquidWalletQuery};

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new(scope).expect("the fixture scope is canonical"),
        )
    }

    fn address() -> WalletAddress {
        "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("the fixture address is valid")
    }

    /// A query with nothing to decode against is refused instead of reading an answer and
    /// publishing no row of it.
    #[test]
    fn a_wallet_query_without_a_decoding_route_is_refused() {
        assert_eq!(
            HyperliquidWalletQuery::public_across(Vec::new(), address()),
            Err(ModelError::EmptyCollection {
                field: "decoding routes"
            })
        );
        assert_eq!(
            HyperliquidWalletQuery::route_level_across(Vec::new()),
            Err(ModelError::EmptyCollection {
                field: "decoding routes"
            })
        );
        assert_eq!(
            HyperliquidUserFillsByTimeQuery::for_routes(Vec::new(), address(), 1, 2),
            Err(ModelError::EmptyCollection {
                field: "decoding routes"
            })
        );
    }

    /// A multi-route query is addressed to its first route — the one whose validation and
    /// transport it uses — and decodes with every route it was given.
    #[test]
    fn a_multi_route_query_is_addressed_to_its_first_route() {
        let core = route("core-perp");
        let deployed = route("perp-dex/xyz");
        let routes = vec![core.clone(), deployed];

        let query = HyperliquidWalletQuery::public_across(routes.clone(), address())
            .expect("a monitored route set decodes the answer");
        assert_eq!(query.route(), &core);
        assert_eq!(query.decoding_routes(), routes.as_slice());

        let window = HyperliquidUserFillsByTimeQuery::for_routes(routes.clone(), address(), 5, 10)
            .expect("a monitored route set decodes the window");
        assert_eq!(window.route(), &core);
        assert_eq!(window.decoding_routes(), routes.as_slice());

        let contexts = HyperliquidWalletQuery::route_level_across(routes.clone())
            .expect("a monitored route set decodes the contexts");
        assert_eq!(contexts.route(), &core);
        assert_eq!(contexts.decoding_routes(), routes.as_slice());
    }

    /// A single-route query decodes with that route alone.
    #[test]
    fn a_single_route_query_decodes_with_its_own_route() {
        let core = route("core-perp");

        assert_eq!(
            HyperliquidWalletQuery::public(core.clone(), address()).decoding_routes(),
            [core].as_slice()
        );
    }
}
