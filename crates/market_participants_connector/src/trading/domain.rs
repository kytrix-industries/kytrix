//! Public trading commands.
//!
//! A command names the exact listing it trades and the authenticated target it
//! acts for; the two must agree on the route before anything is encoded. The
//! quantity is `market_participants_model`'s order quantity, the one definition of what an
//! order is denominated in, and it is validated against the resolved listing's
//! own rules rather than against a second unit vocabulary defined here.

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
    /// Validates this order against the listing it trades.
    ///
    /// The listing's own rules decide the traded unit and the notional asset, and
    /// a route the target does not act on is rejected before the venue's wire is
    /// built.
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
    /// Validates that the listing belongs to the target's route.
    pub fn validate_against(
        &self,
        resolved: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        require_target_route(self.target.route(), &resolved.key)
    }
}

/// Changes the position mode of one venue account.
///
/// Position mode is account-wide, so the command names no listing and there is no
/// listing route to disagree with the target's.
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
    /// Validates that the listing belongs to the target's route.
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
    /// Validates that the listing belongs to the target's route.
    pub fn validate_against(
        &self,
        resolved: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        require_target_route(self.target.route(), &resolved.key)
    }
}

/// Rejects a listing that the authenticated target does not act on.
///
/// A symbol, an asset index or a rules set resolved on one route is never reused
/// on another, so the check happens before anything is encoded.
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

/// Validates one order quantity against the listing's own quantity rule.
///
/// The listed unit is the rule's own unit and the only asset a notional order may
/// name is the listing's notional asset; an outcome quantity must name the exact
/// group, member and side of the listed outcome instrument.
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

/// Builds one order rejection.
pub(crate) const fn order_error(kind: OrderErrorKind) -> MarketParticipantError {
    MarketParticipantError::OrderError {
        kind,
        raw_data: None,
    }
}
