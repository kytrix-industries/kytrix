//! One venue's protocol implementation.
//!
//! `venue()` is the static attribution this adapter is registered under and never
//! decides what a scope serves: which scopes exist and what each of them supports is
//! the venue's loader's answer (the scope entries a commit publishes), so an adapter
//! implements the wire for a scope rather than judging whether the scope is served.

use futures::future::BoxFuture;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};

use super::{
    context::{ExecutionContext, VenueExecutionContext},
    request::PreparedRequest,
    response::{ProtocolResponse, SubscriptionHandle},
};
use crate::{
    error::MarketParticipantError,
    market::timeframe::{Adjustment, TimeFrame},
    venue::metadata::VenueReading,
};

/// The venue-specific bounds of one route.
///
/// These are values the venue accepts rather than a cap this crate chose: a
/// depth request reaches the venue unchanged, and the intervals are the ones its
/// candle endpoint publishes. They belong to the adapter because only the
/// adapter knows its venue's wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterLimits {
    /// The deepest order book this route's venue hands over in one request.
    pub order_book_depth_limit: u16,
    /// The candlestick intervals this route's venue publishes.
    pub timeframes: Vec<TimeFrame>,
    /// The price-adjustment conventions this route's venue publishes for its
    /// candlesticks. A route that serves no candlesticks answers an empty vector;
    /// a route that does answers at least one value.
    ///
    /// It carries [`Adjustment::None`] whenever it carries anything, and that is the
    /// invariant a caller's fallback rests on: a stored convention a route no longer
    /// publishes goes back to the neutral one — the chart's own default, not the
    /// route's first entry — which is only the route's answer while every route that
    /// serves candlesticks publishes the neutral convention. A route that published
    /// only [`Adjustment::Forward`] would make the fallback a value the route does not
    /// serve, and the fallback would then have to become "the route's first published
    /// value" instead. `every_candlestick_route_publishes_the_neutral_convention`
    /// keeps this honest across every route this build registers.
    pub adjustments: Vec<Adjustment>,
}

pub trait MarketParticipantAdapter: Send + Sync {
    /// Returns the venue this adapter speaks to.
    fn venue(&self) -> &VenueId;

    /// Returns the venue-specific bounds of one route.
    fn limits(&self, route: &VenueRoute) -> AdapterLimits;

    /// Reads what one venue publishes, in one reading of its own tables.
    ///
    /// A venue whose scopes are all statically known publishes no table of its own:
    /// every scope it serves is in [`crate::venue::specs`], so its loader never opens
    /// this conversation and the refusal below is the honest answer to a caller that
    /// asks anyway. The refusal names the venue as a typed identity rather than inside
    /// a diagnostic sentence, because it reaches a reader through a localized template.
    fn read_venue(
        &self,
        context: VenueExecutionContext,
    ) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        let venue = *context.venue();
        Box::pin(async move { Err(MarketParticipantError::VenuePublishesNoScopeTable { venue }) })
    }

    /// Executes one request against the venue.
    fn execute(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<ProtocolResponse, MarketParticipantError>>;

    /// Opens one subscription against the venue.
    fn subscribe(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<SubscriptionHandle, MarketParticipantError>>;
}
