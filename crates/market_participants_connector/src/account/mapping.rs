use std::sync::Arc;

use market_participants_model::{
    asset::AssetId,
    decimal::{Quantity, SignedQuantity},
    instrument::{Instrument, InstrumentTerms},
    listing::route::{VenueListingKey, VenueRoute},
    market::account::{AssetAmount, AssetBalance},
};

use super::{
    domain::{
        AccountUnits, HyperliquidAssetContext, HyperliquidHistoricalOrder, HyperliquidLedgerEvent,
        HyperliquidLiquidatedPosition, HyperliquidOpenOrder, HyperliquidPerpetualPosition,
        HyperliquidSpotBalance, HyperliquidTransferDirection, HyperliquidUserFill,
    },
    error::{AccountCatalogError, AccountError, account_error_from_catalog},
    shared::{OrderRoute, PositionDirection},
};
use crate::{
    catalog::resolved::{EnsuredRoute, ResolvedListing},
    market_participants::model,
};

pub(crate) const PRIMARY_ACCOUNT: &str = "primary";

pub(super) struct RouteAssets {
    route: VenueRoute,
    published: ahash::AHashSet<AssetId>,
}

impl RouteAssets {
    pub(super) fn of(ensured: &EnsuredRoute) -> Self {
        let mut published = ahash::AHashSet::with_capacity(16);
        for entry in ensured.listings().iter() {
            published.extend(assets_of(&entry.instrument));
        }
        Self {
            route: ensured.route().clone(),
            published,
        }
    }

    pub(super) fn has(&self, asset: &AssetId) -> bool {
        self.published.contains(asset)
    }

    pub(super) fn published(&self, asset: &AssetId) -> Result<(), AccountError> {
        if self.has(asset) {
            return Ok(());
        }
        Err(AccountError::UnknownAsset {
            route: self.route.clone(),
            asset: Box::new(asset.clone()),
        })
    }
}

pub(super) struct WalletCatalogs {
    target_route: VenueRoute,
    routes: Vec<WalletRoute>,
}

struct WalletRoute {
    catalog: EnsuredRoute,
    assets: Arc<RouteAssets>,
}

impl WalletCatalogs {
    pub(super) fn new(target_route: VenueRoute, catalogs: Vec<EnsuredRoute>) -> Self {
        Self {
            target_route,
            routes: catalogs
                .into_iter()
                .map(|catalog| WalletRoute {
                    assets: Arc::new(RouteAssets::of(&catalog)),
                    catalog,
                })
                .collect(),
        }
    }

    pub(super) fn resolve(
        &self,
        listing: &VenueListingKey,
    ) -> Result<(&EnsuredRoute, ResolvedListing, &Arc<RouteAssets>), AccountError> {
        let route = self.route_of(listing)?;
        let resolved = route
            .catalog
            .resolve_listing(listing)
            .map_err(account_error_from_catalog)?;
        Ok((&route.catalog, resolved, &route.assets))
    }

    pub(super) fn published(&self, asset: &AssetId) -> Result<(), AccountError> {
        if self.routes.iter().any(|route| route.assets.has(asset)) {
            return Ok(());
        }
        Err(AccountError::UnknownAsset {
            route: self.target_route.clone(),
            asset: Box::new(asset.clone()),
        })
    }

    fn route_of(&self, listing: &VenueListingKey) -> Result<&WalletRoute, AccountError> {
        self.routes
            .iter()
            .find(|route| route.catalog.route() == &listing.route)
            .ok_or_else(|| {
                AccountError::Catalog(AccountCatalogError::ListingNotFound {
                    route: listing.route.clone(),
                    listing: Box::new(listing.clone()),
                })
            })
    }
}

pub(super) fn assets_of(instrument: &Instrument) -> Vec<AssetId> {
    match instrument.terms() {
        InstrumentTerms::Spot(terms) => vec![terms.base().clone(), terms.quote().clone()],
        InstrumentTerms::Perpetual(terms) => {
            let mut assets = vec![
                terms.price_quote().clone(),
                terms.settlement_asset().clone(),
            ];
            if let Some(base) = underlying_asset(terms.underlying()) {
                assets.push(base);
            }
            assets
        }
        InstrumentTerms::Future(terms) => {
            let mut assets = vec![terms.price_quote().clone()];
            assets.extend(delivery_assets(terms.delivery()));
            assets
        }
        InstrumentTerms::Option(terms) => {
            let mut assets = vec![terms.price_quote().clone()];
            assets.extend(delivery_assets(terms.delivery()));
            assets
        }
        InstrumentTerms::Outcome(_) => Vec::new(),
    }
}

fn delivery_assets(delivery: &market_participants_model::settlement::DeliveryMode) -> Vec<AssetId> {
    match delivery {
        market_participants_model::settlement::DeliveryMode::Cash { asset, .. } => {
            vec![asset.clone()]
        }
        market_participants_model::settlement::DeliveryMode::Physical { deliveries } => {
            deliveries.iter().map(|leg| leg.asset().clone()).collect()
        }
    }
}

pub(super) fn map_order_id(
    value: &crate::account::shared::OrderId,
) -> crate::account::shared::OrderId {
    crate::account::shared::OrderId::new(
        value.as_str(),
        match value.route() {
            OrderRoute::Normal => OrderRoute::Normal,
            OrderRoute::Algo => OrderRoute::Algo,
        },
    )
    .expect("network order IDs are validated before entering the account capability")
}

pub(super) fn map_position_id(
    value: &crate::account::shared::PositionId,
) -> crate::account::shared::PositionId {
    crate::account::shared::PositionId::new(value.as_str())
        .expect("network position IDs are validated")
}

pub(super) fn map_account_trade_id(
    value: &crate::account::shared::AccountTradeId,
) -> crate::account::shared::AccountTradeId {
    crate::account::shared::AccountTradeId::new(value.as_str())
        .expect("network trade IDs are validated")
}

pub(super) fn map_balance(
    assets: &RouteAssets,
    native: &model::NativeBalance,
) -> Result<AssetBalance, AccountError> {
    assets.published(&native.asset)?;
    Ok(AssetBalance {
        asset: native.asset.clone(),
        total: native.total,
        available: native.available,
    })
}

pub(super) fn map_order(
    native: &model::NativeOrder,
    resolved: &ResolvedListing,
) -> Result<super::domain::Order, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    Ok(super::domain::Order {
        listing: native.listing.clone(),
        id: map_order_id(&native.id),
        side: native.side,
        position_side: native.position_side,
        status: native.status,
        kind: native.kind.clone(),
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        filled_quantity: units
            .quantity(native.filled_quantity)
            .map_err(AccountError::Model)?,
        average_filled_price: units.optional_price(native.average_filled_price),
        reduce_only: native.reduce_only,
        margin_mode: native.margin_mode,
        leverage: native.leverage,
        create_timestamp: native.create_timestamp,
        update_timestamp: native.update_timestamp,
    })
}

pub(super) fn map_position(
    native: &model::NativePosition,
    resolved: &ResolvedListing,
) -> Result<super::domain::Position, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    Ok(super::domain::Position {
        listing: native.listing.clone(),
        id: native.id.as_ref().map(map_position_id),
        position_side: native.position_side,
        direction: native.direction,
        margin_mode: native.margin_mode,
        leverage: native.leverage,
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        entry_price: units.price(native.entry_price),
        break_even_price: units.optional_price(native.break_even_price),
        mark_price: units.price(native.mark_price),
        unrealized_profit_or_loss: units.amount(native.unrealized_profit_or_loss.into()),
        liquidation_price: units.optional_price(native.liquidation_price),
        notional: units
            .notional(native.notional)
            .map_err(AccountError::Model)?,
        initial_margin: units.amount(native.initial_margin.into()),
        maintenance_margin: native
            .maintenance_margin
            .map(|value| units.amount(value.into())),
        update_timestamp: native.update_timestamp,
    })
}

pub(super) fn map_account_trade(
    native: &model::NativeAccountTrade,
    resolved: &ResolvedListing,
    assets: &RouteAssets,
) -> Result<super::domain::AccountTrade, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    assets.published(&native.fee_asset)?;
    Ok(super::domain::AccountTrade {
        listing: native.listing.clone(),
        id: map_account_trade_id(&native.id),
        order_id: map_order_id(&native.order_id),
        side: native.side,
        price: units.price(native.price),
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        fee: AssetAmount {
            asset: native.fee_asset.clone(),
            value: native.fee.into(),
        },
        liquidity: native.liquidity,
        timestamp: native.timestamp,
    })
}

pub(super) fn map_historical_position(
    native: &model::NativeHistoricalPosition,
    resolved: &ResolvedListing,
) -> Result<super::domain::HistoricalPosition, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    Ok(super::domain::HistoricalPosition {
        listing: native.listing.clone(),
        id: map_position_id(&native.id),
        direction: native.direction,
        status: native.status,
        margin_mode: native.margin_mode,
        leverage: native.leverage,
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        maximum_quantity: units
            .quantity(native.maximum_quantity)
            .map_err(AccountError::Model)?,
        closed_quantity: units
            .quantity(native.closed_quantity)
            .map_err(AccountError::Model)?,
        entry_price: units.price(native.entry_price),
        exit_price: units.price(native.exit_price),
        realized_profit_or_loss: units.amount(native.realized_profit_or_loss.into()),
        realized_return: native.realized_return,
        opened_timestamp: native.opened_timestamp,
        closed_timestamp: native.closed_timestamp,
    })
}

pub(super) fn map_hyperliquid_spot_balance(
    native: &model::NativeHyperliquidSpotBalance,
) -> HyperliquidSpotBalance {
    HyperliquidSpotBalance {
        asset: native.coin.clone(),
        total: native.total,
        hold: native.hold,
    }
}

fn underlying_asset(
    underlying: &market_participants_model::asset::DerivativeUnderlying,
) -> Option<AssetId> {
    underlying.reference().asset().cloned()
}

fn signed_inner(value: Quantity) -> i128 {
    i128::try_from(value.inner()).unwrap_or(i128::MAX)
}

pub(super) fn map_hyperliquid_asset_context(
    native: &model::NativeHyperliquidAssetContext,
    resolved: &ResolvedListing,
) -> HyperliquidAssetContext {
    let units = AccountUnits::of(&resolved.rules);
    HyperliquidAssetContext {
        listing: native.listing.clone(),
        mark_price: native.mark_price.map(|price| units.price(price)),
        mid_price: native.mid_price.map(|price| units.price(price)),
    }
}

pub(super) fn map_hyperliquid_perpetual_position(
    native: &model::NativeHyperliquidPerpetualPosition,
    resolved: &ResolvedListing,
) -> Result<HyperliquidPerpetualPosition, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    Ok(HyperliquidPerpetualPosition {
        listing: native.listing.clone(),
        size: units
            .quantity(native.size.magnitude())
            .map_err(AccountError::Model)?,
        direction: if native.size.inner().is_negative() {
            PositionDirection::Short
        } else {
            PositionDirection::Long
        },
        entry_price: native.entry_price.map(|price| units.price(price)),
        mark_price: native.mark_price.map(|price| units.price(price)),
        liquidation_price: native.liquidation_price.map(|price| units.price(price)),
        position_value: native
            .position_value
            .map(|value| units.amount(SignedQuantity::from_inner(signed_inner(value)))),
        unrealized_pnl: units.amount(native.unrealized_pnl.into()),
        margin: native
            .margin
            .map(|value| units.amount(SignedQuantity::from_inner(signed_inner(value)))),
        funding_since_open: units.amount(native.funding_since_open.into()),
    })
}

pub(super) fn map_hyperliquid_open_order(
    native: &model::NativeHyperliquidOpenOrder,
    resolved: &ResolvedListing,
) -> Result<HyperliquidOpenOrder, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    Ok(HyperliquidOpenOrder {
        listing: native.listing.clone(),
        side: native.side,
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        price: units.price(native.price),
        status: native.status,
        kind: native.kind.clone(),
        reduce_only: native.reduce_only,
        timestamp: native.timestamp,
        order_id: smol_str::SmolStr::new(native.order_id.as_str()),
    })
}

pub(super) fn map_hyperliquid_historical_order(
    native: &model::NativeHyperliquidHistoricalOrder,
    resolved: &ResolvedListing,
) -> Result<HyperliquidHistoricalOrder, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    Ok(HyperliquidHistoricalOrder {
        listing: native.listing.clone(),
        side: native.side,
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        price: units.price(native.price),
        kind: native.kind.clone(),
        reduce_only: native.reduce_only,
        status: smol_str::SmolStr::new(native.status.as_str()),
        timestamp: native.timestamp,
        order_id: smol_str::SmolStr::new(native.order_id.as_str()),
    })
}

pub(super) fn map_hyperliquid_user_fill(
    native: &model::NativeHyperliquidUserFill,
    resolved: &ResolvedListing,
    assets: &RouteAssets,
) -> Result<HyperliquidUserFill, AccountError> {
    let units = AccountUnits::of(&resolved.rules);
    assets.published(&native.fee_asset)?;
    let fee_asset = native.fee_asset.clone();
    Ok(HyperliquidUserFill {
        address: native.address.clone(),
        listing: native.listing.clone(),
        side: native.side,
        quantity: units
            .quantity(native.quantity)
            .map_err(AccountError::Model)?,
        price: units.price(native.price),
        timestamp: native.timestamp,
        start_position: native.start_position,
        closed_pnl: AssetAmount {
            asset: fee_asset.clone(),
            value: native.closed_pnl.into(),
        },
        fee: AssetAmount {
            asset: fee_asset,
            value: native.fee.into(),
        },
        hash: native
            .hash
            .as_ref()
            .map(|hash| smol_str::SmolStr::new(hash.as_str())),
        tid: native.tid,
        liquidation: native.liquidation,
        direction: native.direction,
    })
}

pub(super) fn map_ledger_event(
    native: &model::NativeHyperliquidLedgerEvent,
    catalogs: &WalletCatalogs,
) -> Result<HyperliquidLedgerEvent, AccountError> {
    let asset = native.coin.clone();
    if let Some(asset) = asset.as_ref() {
        catalogs.published(asset)?;
    }
    let amount = match (asset.as_ref(), native.amount) {
        (Some(asset), Some(value)) => Some(AssetAmount {
            asset: asset.clone(),
            value,
        }),
        _ => None,
    };
    let value = match (asset.as_ref(), native.value) {
        (Some(asset), Some(value)) => Some(AssetAmount {
            asset: asset.clone(),
            value,
        }),
        _ => None,
    };
    let liquidated_notional = match (asset.as_ref(), native.liquidated_notional) {
        (Some(asset), Some(notional)) => Some(AssetAmount {
            asset: asset.clone(),
            value: notional,
        }),
        _ => None,
    };
    Ok(HyperliquidLedgerEvent {
        address: native.address.clone(),
        timestamp: native.timestamp,
        hash: smol_str::SmolStr::new(native.hash.as_str()),
        asset,
        amount,
        kind: smol_str::SmolStr::new(native.kind.as_str()),
        value,
        transfer_direction: native.transfer_direction.map(|direction| match direction {
            model::HyperliquidTransferDirection::Incoming => HyperliquidTransferDirection::Incoming,
            model::HyperliquidTransferDirection::Outgoing => HyperliquidTransferDirection::Outgoing,
        }),
        related_wallet: native.related_wallet.clone(),
        liquidated_notional,
        liquidated_positions: native
            .liquidated_positions
            .iter()
            .map(|position| {
                let (_, resolved, _) = catalogs.resolve(&position.listing)?;
                Ok(HyperliquidLiquidatedPosition {
                    listing: position.listing.clone(),
                    size: AccountUnits::of(&resolved.rules)
                        .quantity(position.size.magnitude())
                        .map_err(AccountError::Model)?,
                })
            })
            .collect::<Result<Vec<_>, AccountError>>()?,
    })
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        asset::AssetId, decimal::Quantity, listing::route::VenueListingKey,
    };

    use super::{RouteAssets, WalletCatalogs, map_balance, map_hyperliquid_user_fill};
    use crate::{
        account::{
            error::AccountError,
            shared::{OrderStatus, PositionDirection},
        },
        catalog::{resolved::ResolvedListing, test_support},
        market_participants::model,
    };

    async fn assets() -> (RouteAssets, AssetId, AssetId) {
        let route = test_support::spot_route();
        let instrument = test_support::spot_token("binance", "BTC", "USDT");
        let entry = test_support::entry(&route, instrument.clone(), "BTCUSDT");
        let fixture = test_support::Fixture::new(vec![entry]);
        let ensured = fixture
            .ensure()
            .await
            .expect("the fixture route publishes its catalog");
        let (quote, base) = test_support::spot_assets(&instrument);

        (RouteAssets::of(&ensured), base, quote)
    }

    #[tokio::test]
    async fn a_balance_of_a_published_asset_is_carried_as_that_asset() {
        let (assets, base, _) = assets().await;
        let native = model::NativeBalance {
            asset: base.clone(),
            total: Quantity::from(2_u64),
            available: Quantity::from(1_u64),
        };

        let balance = map_balance(&assets, &native).expect("the route publishes that asset");

        assert_eq!(balance.asset, base);
        assert_eq!(balance.total, Quantity::from(2_u64));
        assert_eq!(balance.available, Quantity::from(1_u64));
    }

    #[tokio::test]
    async fn a_balance_of_an_unpublished_asset_is_reported() {
        let (assets, _, _) = assets().await;
        let native = model::NativeBalance {
            asset: test_support::token("okx", "BTC"),
            total: Quantity::from(2_u64),
            available: Quantity::from(1_u64),
        };

        let error = map_balance(&assets, &native)
            .expect_err("an asset the route never published must not resolve");

        assert!(matches!(error, AccountError::UnknownAsset { .. }));
    }

    async fn published() -> (
        ResolvedListing,
        VenueListingKey,
        RouteAssets,
        AssetId,
        AssetId,
    ) {
        let route = test_support::spot_route();
        let instrument = test_support::spot_token("binance", "BTC", "USDT");
        let key = test_support::listing_key(&route, "BTCUSDT");
        let entry = test_support::entry(&route, instrument.clone(), "BTCUSDT");
        let fixture = test_support::Fixture::new(vec![entry]);
        let ensured = fixture
            .ensure()
            .await
            .expect("the fixture route publishes its catalog");
        let (quote, base) = test_support::spot_assets(&instrument);
        let resolved = ensured
            .resolve_listing(&key)
            .expect("the published listing resolves");

        (resolved, key, RouteAssets::of(&ensured), base, quote)
    }

    #[tokio::test]
    async fn an_order_carries_the_listing_units() {
        let (resolved, key, _, base, quote) = published().await;
        let native = model::NativeOrder {
            listing: key.clone(),
            id: crate::account::shared::OrderId::normal("7").expect("the fixture id is valid"),
            side: market_participants_model::market::units::Side::Buy,
            position_side: None,
            status: OrderStatus::Open,
            kind: crate::account::shared::OrderKind::Market,
            quantity: Quantity::from(3_u64),
            filled_quantity: Quantity::from(1_u64),
            average_filled_price: Some("100.5".parse().expect("the fixture price parses")),
            reduce_only: true,
            margin_mode: None,
            leverage: None,
            create_timestamp: 10,
            update_timestamp: 11,
        };

        let order = super::map_order(&native, &resolved).expect("the order maps");

        assert_eq!(order.listing, key);
        assert_eq!(
            order.quantity.unit,
            market_participants_model::market::units::QuantityUnit::AssetUnits {
                asset: base.clone()
            }
        );
        assert_eq!(order.quantity.value, Quantity::from(3_u64));
        assert_eq!(
            order.filled_quantity.unit,
            market_participants_model::market::units::QuantityUnit::AssetUnits { asset: base }
        );
        let price = order
            .average_filled_price
            .expect("the average fill price is carried");
        assert_eq!(price.quote_asset, quote);
        assert_eq!(price.value.to_string(), "100.5");
        assert!(order.reduce_only);
        assert_eq!(order.update_timestamp, 11);
    }

    #[tokio::test]
    async fn a_position_carries_the_listing_units() {
        let (resolved, key, _, base, quote) = published().await;
        let native = model::NativePosition {
            listing: key,
            id: None,
            position_side: market_participants_model::market::units::PositionSide::Long,
            direction: crate::account::shared::PositionDirection::Long,
            margin_mode: None,
            leverage: None,
            quantity: Quantity::from(2_u64),
            entry_price: "100".parse().expect("the fixture price parses"),
            break_even_price: None,
            mark_price: "101".parse().expect("the fixture price parses"),
            unrealized_profit_or_loss: 5_i64.into(),
            liquidation_price: None,
            notional: Quantity::from(200_u64),
            initial_margin: 20_i64.into(),
            maintenance_margin: None,
            update_timestamp: 12,
        };

        let position = super::map_position(&native, &resolved).expect("the position maps");

        assert_eq!(
            position.quantity.unit,
            market_participants_model::market::units::QuantityUnit::AssetUnits { asset: base }
        );
        assert_eq!(
            position.notional.unit,
            market_participants_model::market::units::QuantityUnit::Notional {
                asset: quote.clone()
            }
        );
        assert_eq!(position.unrealized_profit_or_loss.asset, quote);
        assert_eq!(position.entry_price.value.to_string(), "100");
        assert_eq!(position.update_timestamp, 12);
    }

    #[tokio::test]
    async fn an_account_trade_carries_its_fee_asset() {
        let (resolved, key, assets, _, quote) = published().await;
        let native = model::NativeAccountTrade {
            listing: key,
            id: crate::account::shared::AccountTradeId::new("41").expect("the fixture id is valid"),
            order_id: crate::account::shared::OrderId::normal("7")
                .expect("the fixture id is valid"),
            side: market_participants_model::market::units::Side::Sell,
            price: "101.25".parse().expect("the fixture price parses"),
            quantity: Quantity::from(2_u64),
            fee_asset: quote.clone(),
            fee: 1_i64.into(),
            liquidity: None,
            timestamp: 13,
        };

        let trade = super::map_account_trade(&native, &resolved, &assets).expect("the trade maps");

        assert_eq!(trade.price.value.to_string(), "101.25");
        assert_eq!(trade.fee.asset, quote);
        assert_eq!(trade.timestamp, 13);

        let unknown = model::NativeAccountTrade {
            fee_asset: test_support::token("okx", "USDT"),
            ..native
        };
        let error = super::map_account_trade(&unknown, &resolved, &assets)
            .expect_err("an unpublished fee asset must not be carried");

        assert!(matches!(error, AccountError::UnknownAsset { .. }));
    }

    #[tokio::test]
    async fn a_position_keeps_the_direction_of_its_signed_size() {
        let (resolved, key, _, _, _) = published().await;
        let long = model::NativeHyperliquidPerpetualPosition {
            listing: key,
            size: 2_i64.into(),
            entry_price: Some("100".parse().expect("the fixture price parses")),
            mark_price: None,
            liquidation_price: None,
            position_value: None,
            unrealized_pnl: 0_i64.into(),
            margin: None,
            funding_since_open: 0_i64.into(),
        };
        let short = model::NativeHyperliquidPerpetualPosition {
            size: (-3_i64).into(),
            ..long.clone()
        };

        let long = super::map_hyperliquid_perpetual_position(&long, &resolved)
            .expect("the long position maps");
        let short = super::map_hyperliquid_perpetual_position(&short, &resolved)
            .expect("the short position maps");

        assert_eq!(long.direction, PositionDirection::Long);
        assert_eq!(short.direction, PositionDirection::Short);
        assert_eq!(
            long.size.value,
            Quantity::from(2_u64),
            "the size is the magnitude the signed size names"
        );
        assert_eq!(short.size.value, Quantity::from(3_u64));
        assert_eq!(short.size.unit, long.size.unit);
    }

    #[tokio::test]
    async fn a_liquidation_carries_its_notional_and_position_units() {
        let (resolved, key, _assets, base, quote) = published().await;
        let route = test_support::spot_route();
        let instrument = test_support::spot_token("binance", "BTC", "USDT");
        let entry = test_support::entry(&route, instrument, "BTCUSDT");
        let fixture = test_support::Fixture::new(vec![entry]);
        let ensured = fixture
            .ensure()
            .await
            .expect("the fixture route publishes its catalog");
        let catalogs = super::WalletCatalogs::new(route, vec![ensured]);
        let native = model::NativeHyperliquidLedgerEvent {
            address: "0x1111111111111111111111111111111111111111"
                .parse()
                .expect("the fixture address is valid"),
            timestamp: 14,
            hash: "0xabc".parse().expect("the fixture hash is valid"),
            coin: Some(quote.clone()),
            kind: "liquidation".parse().expect("the fixture kind is valid"),
            amount: None,
            value: None,
            transfer_direction: None,
            related_wallet: None,
            liquidated_notional: Some(1234_i64.into()),
            liquidated_positions: vec![model::NativeHyperliquidLiquidatedPosition {
                listing: key.clone(),
                size: (-5_i64).into(),
            }],
        };

        let event = super::map_ledger_event(&native, &catalogs).expect("the liquidation maps");

        let notional = event
            .liquidated_notional
            .expect("the closed notional is carried");
        assert_eq!(notional.value.to_string(), "1234");
        assert_eq!(
            notional.asset, quote,
            "the notional is denominated in the asset the entry names"
        );
        assert_eq!(
            event.amount, None,
            "a liquidation moves no token of its own"
        );
        assert_eq!(event.liquidated_positions.len(), 1);
        assert_eq!(event.liquidated_positions[0].listing, key);
        assert_eq!(
            event.liquidated_positions[0].size.unit,
            market_participants_model::market::units::QuantityUnit::AssetUnits { asset: base },
            "a closed position is sized in its listing's own unit"
        );
        assert_eq!(
            event.liquidated_positions[0].size.value,
            Quantity::from(5_u64)
        );
        assert_eq!(resolved.key, key);
    }

    struct TwoRouteCatalogs {
        catalogs: WalletCatalogs,
        first_listing: VenueListingKey,
        second_listing: VenueListingKey,
        second_base: AssetId,
    }

    async fn two_route_catalogs() -> TwoRouteCatalogs {
        let first = test_support::spot_route();
        let first_instrument = test_support::spot_token("binance", "BTC", "USDT");
        let first_entry = test_support::entry(&first, first_instrument, "BTCUSDT");
        let first_listing = first_entry.listing.key.clone();
        let first_ensured = test_support::Fixture::new(vec![first_entry])
            .ensure()
            .await
            .expect("the first route publishes its catalog");

        let second = test_support::perp_route();
        let second_instrument = test_support::spot_token(test_support::VENUE, "ETH", "USDC");
        let second_entry = test_support::entry(&second, second_instrument.clone(), "ETHUSDC");
        let second_listing = second_entry.listing.key.clone();
        let second_ensured = test_support::Fixture::with_entries(
            second.clone(),
            test_support::SPOT_SOURCE,
            test_support::listing(vec![second_entry.clone()]),
            &[second_entry],
            test_support::FIXTURE_CODES,
        )
        .ensure()
        .await
        .expect("the second route publishes its catalog");
        let (_, second_base) = test_support::spot_assets(&second_instrument);

        TwoRouteCatalogs {
            catalogs: WalletCatalogs::new(first, vec![first_ensured, second_ensured]),
            first_listing,
            second_listing,
            second_base,
        }
    }

    fn native_fill(
        listing: VenueListingKey,
        fee_asset: AssetId,
    ) -> model::NativeHyperliquidUserFill {
        model::NativeHyperliquidUserFill {
            address: "0x1111111111111111111111111111111111111111"
                .parse()
                .expect("the fixture address is valid"),
            listing,
            side: market_participants_model::market::units::Side::Buy,
            quantity: Quantity::from(1_u64),
            price: "10".parse().expect("the fixture price is valid"),
            timestamp: 1,
            start_position: 0_i64.into(),
            closed_pnl: 0_i64.into(),
            fee: 0_i64.into(),
            fee_asset,
            hash: None,
            tid: 1,
            liquidation: false,
            direction: None,
        }
    }

    #[tokio::test]
    async fn a_row_is_resolved_and_validated_by_the_route_that_published_it() {
        let TwoRouteCatalogs {
            catalogs,
            first_listing,
            second_listing,
            second_base,
        } = two_route_catalogs().await;

        let (first_catalog, first_resolved, _) = catalogs
            .resolve(&first_listing)
            .expect("the first route published its listing");
        assert_eq!(first_catalog.route(), &test_support::spot_route());
        assert_eq!(first_resolved.key, first_listing);
        let (second_catalog, second_resolved, second_assets) = catalogs
            .resolve(&second_listing)
            .expect("the second route published its listing");
        assert_eq!(second_catalog.route(), &test_support::perp_route());
        assert_eq!(second_resolved.key, second_listing);

        let fill = native_fill(second_listing.clone(), second_base);
        map_hyperliquid_user_fill(&fill, &second_resolved, second_assets)
            .expect("the second route published the fee asset");
        let unknown = map_hyperliquid_user_fill(
            &native_fill(second_listing.clone(), test_support::token("kraken", "SOL")),
            &second_resolved,
            second_assets,
        )
        .expect_err("an asset no monitored route published must not resolve");
        assert!(matches!(unknown, AccountError::UnknownAsset { .. }));

        let (_, _, first_assets) = catalogs
            .resolve(&first_listing)
            .expect("the first route published its listing");
        let error = map_hyperliquid_user_fill(&fill, &second_resolved, first_assets)
            .expect_err("another route's assets are not the ones that accept this row");
        assert!(matches!(error, AccountError::UnknownAsset { .. }));
    }
}
