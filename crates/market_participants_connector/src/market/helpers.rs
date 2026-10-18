use market_participants_model::listing::route::VenueRoute;

use super::{error::MarketError, provider::MarketCapability};
use crate::{
    adapter::{ProtocolRequest, ProtocolResponse, SubscriptionHandle},
    protocol::{RequestIdentity, classify},
};

pub(crate) async fn execute(
    provider: &MarketCapability,
    route: VenueRoute,
    request: ProtocolRequest,
) -> Result<ProtocolResponse, MarketError> {
    let context = request_context(&route, &request);
    provider
        .gateway()
        .execute(provider.venues(), request, context, provider.catalog())
        .await
        .map_err(MarketError::from)
}

pub(crate) async fn subscribe(
    provider: &MarketCapability,
    route: VenueRoute,
    request: ProtocolRequest,
) -> Result<SubscriptionHandle, MarketError> {
    let context = request_context(&route, &request);
    provider
        .gateway()
        .subscribe(provider.venues(), request, context, provider.catalog())
        .await
        .map_err(MarketError::from)
}

/// Builds the request context of one capability call.
///
/// The capability a request needs is the classifier's answer, so a call site
/// never repeats it.
fn request_context(route: &VenueRoute, request: &ProtocolRequest) -> RequestIdentity {
    RequestIdentity::new(route.clone(), classify(request).capability())
}

pub(crate) const fn protocol_response(operation: &'static str) -> MarketError {
    MarketError::Protocol {
        operation: smol_str::SmolStr::new_static(operation),
    }
}
