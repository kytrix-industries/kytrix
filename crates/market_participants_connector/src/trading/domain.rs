use market_participants_model::{
    decimal::Ratio,
    listing::route::{VenueListingKey, VenueRoute},
    market::{
        selection::OrderQuantity,
        units::{PositionSide, Side},
    },
};

use crate::{
    account::shared::{MarginMode, OrderId, OrderKind, PositionMode},
    catalog::resolved::ResolvedListing,
    error::{MarketParticipantError, OrderErrorKind},
    protocol::AuthenticatedTarget,
};

#[derive(Clone, Debug)]
pub struct PlaceOrderCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub side: Side,
    pub position_side: Option<PositionSide>,
    pub kind: OrderKind,
    pub quantity: OrderQuantity,
    pub reduce_only: bool,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<std::num::NonZeroU8>,
    pub market_max_slippage: Option<Ratio>,
}

impl PlaceOrderCommand {
    pub fn validate_against(
        &self,
        resolved: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        require_target_route(self.target.route(), &resolved.key)?;
        if self.quantity.value().is_zero() {
            return Err(order_error(OrderErrorKind::InvalidQuantity {
                reason: smol_str::SmolStr::new_static("an order quantity must be positive"),
            }));
        }
        match (&self.kind, self.market_max_slippage) {
            (OrderKind::Market, Some(slippage))
                if slippage.is_zero() || slippage >= Ratio::from(1_u8) =>
            {
                return Err(order_error(OrderErrorKind::InvalidPrice {
                    reason: smol_str::SmolStr::new_static(
                        "an immediate market order slippage must be inside (0, 1)",
                    ),
                }));
            }
            (OrderKind::Market, _) | (_, None) => {}
            (_, Some(_)) => {
                return Err(order_error(OrderErrorKind::InvalidPrice {
                    reason: smol_str::SmolStr::new_static(
                        "a market slippage bound requires an immediate market order",
                    ),
                }));
            }
        }
        require_order_quantity(&self.quantity, resolved)
    }
}

#[derive(Clone, Debug)]
pub struct CancelOrderCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub order_id: OrderId,
}

impl CancelOrderCommand {
    pub fn validate_against(
        &self,
        resolved: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        require_target_route(self.target.route(), &resolved.key)
    }
}

#[derive(Clone, Debug)]
pub struct SetPositionModeCommand {
    pub target: AuthenticatedTarget,
    pub mode: PositionMode,
}

#[derive(Clone, Debug)]
pub struct SetMarginModeCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub mode: MarginMode,
}

impl SetMarginModeCommand {
    pub fn validate_against(
        &self,
        resolved: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        require_target_route(self.target.route(), &resolved.key)
    }
}

#[derive(Clone, Debug)]
pub struct SetLeverageCommand {
    pub target: AuthenticatedTarget,
    pub listing: VenueListingKey,
    pub margin_mode: MarginMode,
    pub position_side: Option<PositionSide>,
    pub leverage: std::num::NonZeroU8,
}

impl SetLeverageCommand {
    pub fn validate_against(
        &self,
        resolved: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        require_target_route(self.target.route(), &resolved.key)
    }
}

pub(crate) fn require_target_route(
    target: &VenueRoute,
    listing: &VenueListingKey,
) -> Result<(), MarketParticipantError> {
    if &listing.route == target {
        return Ok(());
    }
    Err(MarketParticipantError::ListingRouteMismatch {
        listing: listing.clone(),
        route: target.clone(),
    })
}

fn require_order_quantity(
    quantity: &OrderQuantity,
    resolved: &ResolvedListing,
) -> Result<(), MarketParticipantError> {
    let rules = &resolved.rules;
    quantity
        .validate_against(
            &rules.common.quantity.unit,
            rules.common.notional.as_ref().map(|rule| &rule.asset),
            &resolved.instrument,
        )
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))
}

pub(crate) const fn order_error(kind: OrderErrorKind) -> MarketParticipantError {
    MarketParticipantError::OrderError {
        kind,
        raw_data: None,
    }
}
