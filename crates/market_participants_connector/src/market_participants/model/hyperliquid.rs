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
    protocol::WalletTarget,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HyperliquidWalletQuery {
    pub target: WalletTarget,
    decoding_routes: Vec<VenueRoute>,
}

impl HyperliquidWalletQuery {
    pub fn public(route: VenueRoute, address: WalletAddress) -> Self {
        Self {
            target: WalletTarget::Public {
                route: route.clone(),
                address,
            },
            decoding_routes: vec![route],
        }
    }

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
            target: WalletTarget::Public { route, address },
            decoding_routes,
        })
    }

    pub fn route_level_across(decoding_routes: Vec<VenueRoute>) -> Result<Self, ModelError> {
        let Some(route) = decoding_routes.first().cloned() else {
            return Err(ModelError::EmptyCollection {
                field: "decoding routes",
            });
        };
        Ok(Self {
            target: WalletTarget::Route { route },
            decoding_routes,
        })
    }

    pub fn decoding_routes(&self) -> &[VenueRoute] {
        &self.decoding_routes
    }

    pub fn route(&self) -> &VenueRoute {
        self.target.route()
    }

    pub const fn wallet_target(&self) -> &WalletTarget {
        &self.target
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HyperliquidUserFillsByTimeQuery {
    pub target: WalletTarget,
    decoding_routes: Vec<VenueRoute>,
    pub start_timestamp: UnixMillis,
    pub end_timestamp: UnixMillis,
}

impl HyperliquidUserFillsByTimeQuery {
    pub const MAX_WINDOW_MILLIS: UnixMillis = 6 * 60 * 60 * 1_000;

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
            target: WalletTarget::Public { route, address },
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

    pub fn decoding_routes(&self) -> &[VenueRoute] {
        &self.decoding_routes
    }

    pub fn route(&self) -> &VenueRoute {
        self.target.route()
    }

    pub const fn wallet_target(&self) -> &WalletTarget {
        &self.target
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidSpotBalance {
    pub coin: AssetId,
    pub total: SignedQuantity,
    pub hold: SignedQuantity,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidSpotState {
    pub balances: Vec<NativeHyperliquidSpotBalance>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidSpotPair {
    pub coin: market_participants_model::identifier::IdentifierValue,
    pub base_token: AssetId,
    pub quote_token: AssetId,
    pub asset_id: market_participants_model::listing::venue::HyperliquidAssetId,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidAssetContext {
    pub listing: VenueListingKey,
    pub mark_price: Option<Price>,
    pub mid_price: Option<Price>,
}

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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidPerpetualState {
    pub positions: Vec<NativeHyperliquidPerpetualPosition>,
}

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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidHistoricalOrder {
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: Quantity,
    pub price: Price,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub status: market_participants_model::identifier::IdentifierValue,
    pub timestamp: UnixMillis,
    pub order_id: market_participants_model::identifier::IdentifierValue,
}

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
    pub fee_asset: AssetId,
    pub hash: Option<market_participants_model::identifier::IdentifierValue>,
    pub tid: u64,
    pub liquidation: bool,
    pub direction: Option<crate::account::shared::HyperliquidFillDirection>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidLedgerEvent {
    pub address: WalletAddress,
    pub timestamp: UnixMillis,
    pub hash: market_participants_model::identifier::IdentifierValue,
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NativeHyperliquidLiquidatedPosition {
    pub listing: VenueListingKey,
    pub size: SignedQuantity,
}

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

    #[test]
    fn a_single_route_query_decodes_with_its_own_route() {
        let core = route("core-perp");

        assert_eq!(
            HyperliquidWalletQuery::public(core.clone(), address()).decoding_routes(),
            [core].as_slice()
        );
    }
}
