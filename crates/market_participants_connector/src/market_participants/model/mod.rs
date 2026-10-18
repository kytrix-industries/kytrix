//! Protocol-facing values of the market-participants layer.
//!
//! These types are the vocabulary one protocol conversation is written in. They
//! are deliberately not the public model: a query names the exact listing key it
//! is about and preserves the protocol's own value shapes, and a native payload
//! keeps the venue's identity and units until a capability boundary maps it into
//! a unit-carrying, listing-keyed public value.
//!
//! Nothing here carries a route *and* a listing key: a listing-scoped request
//! reads its route from the key, and only a genuinely route-scoped request names
//! a `VenueRoute` of its own.

pub(crate) mod account;
pub(crate) mod hyperliquid;
pub(crate) mod query;

pub(crate) use account::*;
pub(crate) use hyperliquid::*;
pub(crate) use query::*;

#[cfg(test)]
mod tests {
    use market_participants_model::routing::CapabilityId;

    use super::{StatisticsTimeFrame, TimeFrame};
    use crate::catalog::test_support;

    /// A listing-scoped query names its key and reads its route from that key:
    /// there is no second field a route could disagree with.
    #[test]
    fn a_listing_scoped_query_reads_its_route_from_its_listing_key() {
        let route = test_support::spot_route();
        let listing = test_support::listing_key(&route, "BTCUSDT");
        let query = super::TickerQuery {
            listing: listing.clone(),
        };

        assert_eq!(query.route(), &route);
        assert_eq!(query.listing, listing);
    }

    /// A route-scoped query names no listing at all.
    #[test]
    fn a_route_scoped_query_names_no_listing() {
        let query = super::TickersQuery {
            route: test_support::perp_route(),
        };

        assert_eq!(query.route().scope.as_str(), "perpetual");
        assert_eq!(query.route.scope.as_str(), "perpetual");
    }

    /// The enums are wire schema: a spelling change would silently reinterpret
    /// a stored payload, so each one is pinned here.
    #[test]
    fn the_request_enums_use_their_documented_spellings() {
        assert_eq!(
            serde_json::to_string(&TimeFrame::Hour4).expect("the interval should serialize"),
            "\"4h\""
        );
        assert_eq!(
            serde_json::to_string(&StatisticsTimeFrame::Day1)
                .expect("the statistics interval should serialize"),
            "\"1d\""
        );
        assert_eq!(
            serde_json::to_string(&CapabilityId::AuthenticatedTrading)
                .expect("the capability should serialize"),
            "\"authenticated_trading\""
        );
    }
}
