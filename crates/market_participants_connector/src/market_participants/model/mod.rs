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

    #[test]
    fn a_route_scoped_query_names_no_listing() {
        let query = super::TickersQuery {
            route: test_support::perp_route(),
        };

        assert_eq!(query.route().scope.as_str(), "perpetual");
        assert_eq!(query.route.scope.as_str(), "perpetual");
    }

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
