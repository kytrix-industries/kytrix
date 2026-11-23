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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterLimits {
    pub order_book_depth_limit: u16,
    pub timeframes: Vec<TimeFrame>,
    pub adjustments: Vec<Adjustment>,
}

pub trait MarketParticipantAdapter: Send + Sync {
    fn venue(&self) -> &VenueId;

    fn limits(&self, route: &VenueRoute) -> AdapterLimits;

    fn read_venue(
        &self,
        context: VenueExecutionContext,
    ) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        let venue = *context.venue();
        Box::pin(async move { Err(MarketParticipantError::VenuePublishesNoScopeTable { venue }) })
    }

    fn execute(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<ProtocolResponse, MarketParticipantError>>;

    fn subscribe(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<SubscriptionHandle, MarketParticipantError>>;
}
