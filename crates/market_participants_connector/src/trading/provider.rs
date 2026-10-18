//! The trading capability: the only place a command becomes an order.

use std::sync::Arc;

use market_participants_model::{
    listing::route::VenueListingKey, market::selection::ListingTicker,
};

use super::{
    domain::{
        CancelOrderCommand, PlaceOrderCommand, SetLeverageCommand, SetMarginModeCommand,
        SetPositionModeCommand,
    },
    error::{TradingError, trading_error_from_catalog, trading_error_from_market_participant},
};
use crate::{
    account::shared::OrderId,
    adapter::{ProtocolRequest, ProtocolResponse},
    catalog::resolved::ResolvedListing,
    connector::MarketParticipantsConnectorInner,
    market_participants::model,
    protocol::{RequestIdentity, classify},
};

/// Trading capability handle backed by the shared market-participants runtime.
pub struct TradingCapability {
    inner: Arc<MarketParticipantsConnectorInner>,
}

impl Clone for TradingCapability {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl TradingCapability {
    pub(crate) fn new(inner: Arc<MarketParticipantsConnectorInner>) -> Self {
        Self { inner }
    }

    /// Returns the current ticker of one listing.
    pub async fn ticker(&self, listing: &VenueListingKey) -> Result<ListingTicker, TradingError> {
        let resolved = self.resolve_listing(listing)?;
        let response = self
            .execute(
                resolved.key.route.clone(),
                ProtocolRequest::FetchTicker(model::TickerQuery {
                    listing: resolved.key.clone(),
                }),
            )
            .await?;
        match response {
            ProtocolResponse::Ticker(value) => {
                let units = crate::market::units::ListingUnits::of(&resolved.rules);
                crate::market::units::ticker(&value.listing, &value.data, &units)
                    .map_err(trading_error_from_market_participant)
            }
            _ => Err(Self::protocol_error("fetch ticker")),
        }
    }

    /// Places one order against the listing it names.
    pub async fn place_order(&self, command: PlaceOrderCommand) -> Result<OrderId, TradingError> {
        let resolved = self.resolve_listing(&command.listing)?;
        command
            .validate_against(&resolved)
            .map_err(trading_error_from_market_participant)?;
        let response = self
            .execute(
                resolved.key.route.clone(),
                ProtocolRequest::PlaceOrder(model::PlaceOrderCommand {
                    target: command.target,
                    listing: resolved.key.clone(),
                    side: command.side,
                    position_side: command.position_side,
                    kind: command.kind,
                    quantity: command.quantity,
                    reduce_only: command.reduce_only,
                    margin_mode: command.margin_mode,
                    leverage: command.leverage,
                    market_max_slippage: command.market_max_slippage,
                }),
            )
            .await?;
        match response {
            ProtocolResponse::OrderPlaced(value) => Ok(value),
            _ => Err(Self::protocol_error("place order")),
        }
    }

    /// Cancels one order of the listing it names.
    pub async fn cancel_order(&self, command: CancelOrderCommand) -> Result<OrderId, TradingError> {
        let resolved = self.resolve_listing(&command.listing)?;
        command
            .validate_against(&resolved)
            .map_err(trading_error_from_market_participant)?;
        let response = self
            .execute(
                resolved.key.route.clone(),
                ProtocolRequest::CancelOrder(model::CancelOrderCommand {
                    target: command.target,
                    listing: resolved.key.clone(),
                    order_id: command.order_id,
                }),
            )
            .await?;
        match response {
            ProtocolResponse::OrderCancelled(value) => Ok(value),
            _ => Err(Self::protocol_error("cancel order")),
        }
    }

    /// Changes the position mode of one venue account.
    ///
    /// No adapter implements this operation yet, so the request reaches the
    /// adapter dispatcher and fails there. A successful dispatch therefore means
    /// the request was routed to a handler that cannot honour it, which is
    /// reported as a protocol error.
    pub async fn set_position_mode(
        &self,
        command: SetPositionModeCommand,
    ) -> Result<(), TradingError> {
        self.execute(
            command.target.route().clone(),
            ProtocolRequest::SetPositionMode(model::SetPositionModeCommand {
                target: command.target,
                mode: command.mode,
            }),
        )
        .await?;
        Err(Self::protocol_error("set position mode"))
    }

    /// Changes the margin mode of one listing. See [`Self::set_position_mode`].
    pub async fn set_margin_mode(&self, command: SetMarginModeCommand) -> Result<(), TradingError> {
        let resolved = self.resolve_listing(&command.listing)?;
        command
            .validate_against(&resolved)
            .map_err(trading_error_from_market_participant)?;
        self.execute(
            resolved.key.route.clone(),
            ProtocolRequest::SetMarginMode(model::SetMarginModeCommand {
                target: command.target,
                listing: resolved.key.clone(),
                mode: command.mode,
            }),
        )
        .await?;
        Err(Self::protocol_error("set margin mode"))
    }

    /// Changes the leverage of one listing. See [`Self::set_position_mode`].
    pub async fn set_leverage(&self, command: SetLeverageCommand) -> Result<(), TradingError> {
        let resolved = self.resolve_listing(&command.listing)?;
        command
            .validate_against(&resolved)
            .map_err(trading_error_from_market_participant)?;
        self.execute(
            resolved.key.route.clone(),
            ProtocolRequest::SetLeverage(model::SetLeverageCommand {
                target: command.target,
                listing: resolved.key.clone(),
                margin_mode: command.margin_mode,
                position_side: command.position_side,
                leverage: command.leverage,
            }),
        )
        .await?;
        Err(Self::protocol_error("set leverage"))
    }

    /// Resolves one listing into the value a command validates against.
    fn resolve_listing(&self, listing: &VenueListingKey) -> Result<ResolvedListing, TradingError> {
        self.inner
            .catalog()
            .resolve_listing(listing)
            .map_err(trading_error_from_catalog)
    }

    async fn execute(
        &self,
        route: market_participants_model::listing::route::VenueRoute,
        request: ProtocolRequest,
    ) -> Result<ProtocolResponse, TradingError> {
        let context = RequestIdentity::new(route.clone(), classify(&request).capability());
        self.inner
            .gateway
            .execute(&self.inner.venues, request, context, self.inner.catalog())
            .await
            .map_err(trading_error_from_market_participant)
    }

    fn protocol_error(operation: &'static str) -> TradingError {
        TradingError::Protocol {
            operation: operation.into(),
            reason: "adapter returned an unexpected response".into(),
        }
    }
}
