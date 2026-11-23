use market_participants_model::{
    identifier::{MarketParticipantAccountId, WalletAddress},
    listing::{listing_impl::ListingStatus, route::VenueRoute},
    routing::CapabilityId,
};

pub(crate) use crate::adapter::{ProtocolRequest, ProtocolResponse, SubscriptionHandle};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestIdentity {
    route: VenueRoute,
    capability: CapabilityId,
}

impl RequestIdentity {
    pub fn new(route: VenueRoute, capability: CapabilityId) -> Self {
        Self { route, capability }
    }

    pub const fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct AuthenticatedTarget {
    #[serde(rename = "account")]
    account: MarketParticipantAccountId,
    #[serde(rename = "route")]
    route: VenueRoute,
}

impl AuthenticatedTarget {
    pub fn new(account: MarketParticipantAccountId, route: VenueRoute) -> Self {
        Self { account, route }
    }

    pub const fn account(&self) -> &MarketParticipantAccountId {
        &self.account
    }

    pub const fn route(&self) -> &VenueRoute {
        &self.route
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WalletTarget {
    Public {
        route: VenueRoute,
        address: WalletAddress,
    },
    Route {
        route: VenueRoute,
    },
}

impl WalletTarget {
    pub fn route(&self) -> &VenueRoute {
        match self {
            Self::Public { route, .. } | Self::Route { route } => route,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandKind {
    NewTrading,
    CancelOrder,
    Settlement,
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestTarget {
    Route(VenueRoute),
    Listing(market_participants_model::listing::route::VenueListingKey),
    Account(AuthenticatedTarget),
    Wallet(WalletTarget),
}

impl RequestTarget {
    pub fn route(&self) -> &VenueRoute {
        match self {
            Self::Route(route) => route,
            Self::Listing(listing) => &listing.route,
            Self::Account(target) => target.route(),
            Self::Wallet(target) => target.route(),
        }
    }

    pub fn authenticated(&self) -> Option<&AuthenticatedTarget> {
        match self {
            Self::Account(target) => Some(target),
            Self::Wallet(_) | Self::Route(_) | Self::Listing(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedRequest {
    capability: CapabilityId,
    target: RequestTarget,
    command: Option<CommandKind>,
    listing: Option<market_participants_model::listing::route::VenueListingKey>,
}

impl ClassifiedRequest {
    pub fn new(
        capability: CapabilityId,
        target: RequestTarget,
        command: Option<CommandKind>,
        listing: Option<market_participants_model::listing::route::VenueListingKey>,
    ) -> Self {
        Self {
            capability,
            target,
            command,
            listing,
        }
    }

    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    pub const fn target(&self) -> &RequestTarget {
        &self.target
    }

    pub const fn listing(
        &self,
    ) -> Option<&market_participants_model::listing::route::VenueListingKey> {
        self.listing.as_ref()
    }

    pub fn authenticated_account(
        &self,
    ) -> Option<&market_participants_model::identifier::MarketParticipantAccountId> {
        self.target
            .authenticated()
            .map(AuthenticatedTarget::account)
    }

    pub const fn command(&self) -> Option<CommandKind> {
        self.command
    }
}

pub fn classify(request: &ProtocolRequest) -> ClassifiedRequest {
    use CapabilityId as Capability;
    use ProtocolRequest as Request;
    match request {
        Request::FetchMarkets(query) => route_request(Capability::MarketCatalog, query.route()),
        Request::FetchHyperliquidSpotPairs(query) => {
            route_request(Capability::MarketCatalog, query.route())
        }
        Request::FetchCandlesticks(query) => {
            listing_request(Capability::Candlesticks, &query.listing, None)
        }
        Request::SubscribeCandlesticks(query) => {
            listing_request(Capability::Candlesticks, &query.listing, None)
        }
        Request::FetchTicker(query) | Request::SubscribeTicker(query) => {
            listing_request(Capability::Tickers, &query.listing, None)
        }
        Request::FetchAllTickers(query) => route_request(Capability::AllTickers, query.route()),
        Request::FetchPredictionEvents(query) => {
            route_request(Capability::PredictionEvents, query.route())
        }
        Request::FetchPredictionEvent(query) => {
            route_request(Capability::PredictionEvents, query.route())
        }
        Request::FetchPredictionSeries(query) => {
            route_request(Capability::PredictionSeries, query.route())
        }
        Request::SubscribePredictionSeries(query) => {
            route_request(Capability::PredictionSeries, query.route())
        }
        Request::FetchPredictionOrderBook(query) | Request::SubscribePredictionOrderBook(query) => {
            route_request(Capability::PredictionOrderBook, query.route())
        }
        Request::FetchOrderBook(query) | Request::SubscribeOrderBook(query) => {
            listing_request(Capability::OrderBook, &query.listing, None)
        }
        Request::FetchTrades(query) => {
            listing_request(Capability::MarketTrade, &query.listing, None)
        }
        Request::SubscribeTrades(query) => {
            listing_request(Capability::MarketTrade, &query.listing, None)
        }
        Request::SubscribeForceOrder(query) => route_request(Capability::ForceOrder, query.route()),
        Request::FetchOpenInterest(query) => {
            listing_request(Capability::OpenInterest, &query.listing, None)
        }
        Request::FetchLongShortRatio(query) => {
            listing_request(Capability::LongShortAccountRatio, &query.listing, None)
        }
        Request::FetchTopTraderAccountRatio(query) => {
            listing_request(Capability::TopTraderAccountRatio, &query.listing, None)
        }
        Request::FetchTopTraderPositionRatio(query) => {
            listing_request(Capability::TopTraderPositionRatio, &query.listing, None)
        }
        Request::FetchTakerVolume(query) => {
            listing_request(Capability::TakerVolume, &query.listing, None)
        }
        Request::PlaceOrder(command) => account_request(
            Capability::AuthenticatedTrading,
            command.target(),
            Some(CommandKind::NewTrading),
            Some(command.listing.clone()),
        ),
        Request::CancelOrder(command) => account_request(
            Capability::AuthenticatedTrading,
            command.target(),
            Some(CommandKind::CancelOrder),
            Some(command.listing.clone()),
        ),
        Request::SetPositionMode(command) => account_request(
            Capability::AuthenticatedTrading,
            command.target(),
            Some(CommandKind::NewTrading),
            None,
        ),
        Request::SetMarginMode(command) => account_request(
            Capability::AuthenticatedTrading,
            command.target(),
            Some(CommandKind::NewTrading),
            Some(command.listing.clone()),
        ),
        Request::SetLeverage(command) => account_request(
            Capability::AuthenticatedTrading,
            command.target(),
            Some(CommandKind::NewTrading),
            Some(command.listing.clone()),
        ),
        Request::FetchTradingSettings(query) => account_request(
            Capability::AuthenticatedTrading,
            query.target(),
            None,
            Some(query.listing.clone()),
        ),
        Request::SubscribeOrders(query) | Request::SubscribePositions(query) => account_request(
            Capability::AccountOrders,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchBalances(query) => account_request(
            Capability::AccountBalances,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchActiveOrders(query) => account_request(
            Capability::AccountOrders,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchOrderHistory(query) => account_request(
            Capability::AccountHistory,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchPositions(query) => account_request(
            Capability::AccountPositions,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchAccountTrades(query) => account_request(
            Capability::AccountTrades,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchPositionHistory(query) => account_request(
            Capability::AccountPositions,
            query.target(),
            None,
            query.listing.clone(),
        ),
        Request::FetchHyperliquidSpotState(query) => wallet_request(query.wallet_target()),
        Request::FetchHyperliquidPerpetualState(query) => wallet_request(query.wallet_target()),
        Request::FetchHyperliquidOpenOrders(query) => wallet_request(query.wallet_target()),
        Request::FetchHyperliquidHistoricalOrders(query) => wallet_request(query.wallet_target()),
        Request::FetchHyperliquidUserFills { query, .. } => wallet_request(query.wallet_target()),
        Request::FetchHyperliquidUserFillsByTime(query) => wallet_request(query.wallet_target()),
        Request::FetchHyperliquidUserLedger { query, .. } => wallet_request(query.wallet_target()),
        Request::SubscribeHyperliquidUserFills(query) => wallet_request(query.wallet_target()),
        Request::SubscribeHyperliquidUserLedger(query) => wallet_request(query.wallet_target()),
        Request::SubscribeHyperliquidFastContexts(query) => {
            wallet_route_request(Capability::Wallet, query.route())
        }
    }
}

fn route_request(capability: CapabilityId, route: &VenueRoute) -> ClassifiedRequest {
    ClassifiedRequest::new(capability, RequestTarget::Route(route.clone()), None, None)
}

fn listing_request(
    capability: CapabilityId,
    listing: &market_participants_model::listing::route::VenueListingKey,
    command: Option<CommandKind>,
) -> ClassifiedRequest {
    ClassifiedRequest::new(
        capability,
        RequestTarget::Listing(listing.clone()),
        command,
        Some(listing.clone()),
    )
}

fn account_request(
    capability: CapabilityId,
    target: &AuthenticatedTarget,
    command: Option<CommandKind>,
    listing: Option<market_participants_model::listing::route::VenueListingKey>,
) -> ClassifiedRequest {
    ClassifiedRequest::new(
        capability,
        RequestTarget::Account(target.clone()),
        command,
        listing,
    )
}

fn wallet_request(target: &WalletTarget) -> ClassifiedRequest {
    ClassifiedRequest::new(
        CapabilityId::Wallet,
        RequestTarget::Wallet(target.clone()),
        None,
        None,
    )
}

fn wallet_route_request(capability: CapabilityId, route: &VenueRoute) -> ClassifiedRequest {
    ClassifiedRequest::new(
        capability,
        RequestTarget::Wallet(WalletTarget::Route {
            route: route.clone(),
        }),
        None,
        None,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListingCommandMatrix;

impl ListingCommandMatrix {
    pub const fn allows(status: ListingStatus, command: CommandKind) -> bool {
        match (status, command) {
            (ListingStatus::Trading, _) => true,
            (ListingStatus::Scheduled, CommandKind::Read) => true,
            (ListingStatus::Scheduled, _) => false,
            (ListingStatus::Halted, CommandKind::Read) => true,
            (ListingStatus::Halted, _) => false,
            (ListingStatus::Expired, CommandKind::NewTrading) => false,
            (ListingStatus::Expired, _) => true,
            (ListingStatus::Settled | ListingStatus::Delisted, CommandKind::Read) => true,
            (ListingStatus::Settled | ListingStatus::Delisted, _) => false,
        }
    }

    pub fn permits(
        status: ListingStatus,
        command: CommandKind,
        available_from: Option<market_participants_model::decimal::UnixMillis>,
        available_until: Option<market_participants_model::decimal::UnixMillis>,
        as_of: market_participants_model::decimal::UnixMillis,
    ) -> bool {
        let within_window = available_from.is_none_or(|from| as_of >= from)
            && available_until.is_none_or(|until| as_of < until);
        Self::allows(status, command) && (within_window || command == CommandKind::Read)
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{MarketParticipantAccountId, WalletAddress},
        listing::{listing_impl::ListingStatus, route::VenueRoute},
        market::{selection::OrderQuantity, units::Side},
    };

    use super::{AuthenticatedTarget, CommandKind, ListingCommandMatrix, classify};
    use crate::{
        account::shared::{MarginMode, OrderKind},
        adapter::ProtocolRequest,
        catalog::test_support,
        market_participants::model::{AuthenticatedQuery, MarketCatalogQuery, PlaceOrderCommand},
    };

    const STATUSES: [ListingStatus; 6] = [
        ListingStatus::Scheduled,
        ListingStatus::Trading,
        ListingStatus::Halted,
        ListingStatus::Expired,
        ListingStatus::Settled,
        ListingStatus::Delisted,
    ];

    const COMMANDS: [CommandKind; 4] = [
        CommandKind::NewTrading,
        CommandKind::CancelOrder,
        CommandKind::Settlement,
        CommandKind::Read,
    ];

    const EXPECTED: [[bool; 4]; 6] = [
        [false, false, false, true],
        [true, true, true, true],
        [false, false, false, true],
        [false, true, true, true],
        [false, false, false, true],
        [false, false, false, true],
    ];

    fn address() -> WalletAddress {
        "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("the wallet address should be valid")
    }

    #[test]
    fn the_listing_status_command_matrix_matches_the_documented_one() {
        for (status_index, status) in STATUSES.iter().enumerate() {
            for (command_index, command) in COMMANDS.iter().enumerate() {
                assert_eq!(
                    ListingCommandMatrix::allows(*status, *command),
                    EXPECTED[status_index][command_index],
                    "{status:?} x {command:?}"
                );
            }
        }
    }

    #[test]
    fn an_expired_listing_still_allows_settlement_but_never_new_trading() {
        assert!(ListingCommandMatrix::allows(
            ListingStatus::Expired,
            CommandKind::Settlement
        ));
        assert!(!ListingCommandMatrix::allows(
            ListingStatus::Expired,
            CommandKind::NewTrading
        ));
        assert!(
            !ListingCommandMatrix::allows(ListingStatus::Settled, CommandKind::Settlement),
            "a settled listing never changes settlement state again"
        );
        assert!(!ListingCommandMatrix::allows(
            ListingStatus::Delisted,
            CommandKind::Settlement
        ));
    }

    #[test]
    fn the_availability_window_gates_every_command_that_changes_state() {
        assert!(ListingCommandMatrix::permits(
            ListingStatus::Trading,
            CommandKind::NewTrading,
            Some(10),
            Some(20),
            19
        ));
        assert!(
            !ListingCommandMatrix::permits(
                ListingStatus::Trading,
                CommandKind::NewTrading,
                Some(10),
                Some(20),
                20
            ),
            "the window is half-open"
        );
        assert!(!ListingCommandMatrix::permits(
            ListingStatus::Expired,
            CommandKind::Settlement,
            Some(10),
            Some(20),
            20
        ));
        assert!(
            ListingCommandMatrix::permits(
                ListingStatus::Expired,
                CommandKind::Read,
                Some(10),
                Some(20),
                20
            ),
            "reading state is never gated by the window"
        );
        assert!(
            ListingCommandMatrix::permits(
                ListingStatus::Trading,
                CommandKind::NewTrading,
                None,
                None,
                0
            ),
            "a missing bound is no bound, not a closed window"
        );
    }

    fn account(route: &VenueRoute) -> AuthenticatedTarget {
        AuthenticatedTarget::new(
            MarketParticipantAccountId::new("primary").expect("the fixture account is valid"),
            route.clone(),
        )
    }

    #[test]
    fn a_command_declares_the_listing_it_acts_on() {
        let route = test_support::spot_route();
        let listing = test_support::listing_key(&route, "BTCUSDT");
        let command = classify(&ProtocolRequest::PlaceOrder(PlaceOrderCommand {
            target: account(&route),
            listing: listing.clone(),
            side: Side::Buy,
            position_side: None,
            kind: OrderKind::Market,
            quantity: OrderQuantity::AssetUnits {
                asset: test_support::token("binance", "BTC"),
                value: market_participants_model::decimal::Quantity::from(1_u8),
            },
            reduce_only: false,
            margin_mode: Some(MarginMode::Cross),
            leverage: None,
            market_max_slippage: None,
        }));
        let narrowed = classify(&ProtocolRequest::FetchActiveOrders(
            AuthenticatedQuery::for_listing(account(&route), listing.clone()),
        ));

        assert_eq!(command.listing(), Some(&listing));
        assert_eq!(command.command(), Some(CommandKind::NewTrading));
        assert!(
            command.target().authenticated().is_some(),
            "a command still authenticates as its account"
        );
        assert_eq!(narrowed.listing(), Some(&listing));
        assert_eq!(narrowed.command(), None);
    }

    #[test]
    fn a_request_that_names_no_listing_declares_none() {
        let route = test_support::spot_route();
        let wide = classify(&ProtocolRequest::FetchBalances(
            AuthenticatedQuery::account_wide(account(&route)),
        ));
        let markets = classify(&ProtocolRequest::FetchMarkets(MarketCatalogQuery {
            route: route.clone(),
        }));
        let wallet = classify(&ProtocolRequest::FetchHyperliquidOpenOrders(
            crate::market_participants::model::HyperliquidWalletQuery::public(route, address()),
        ));

        assert_eq!(wide.listing(), None);
        assert_eq!(markets.listing(), None);
        assert_eq!(wallet.listing(), None);
        assert_eq!(
            wallet.capability(),
            market_participants_model::routing::CapabilityId::Wallet
        );
    }

    #[test]
    fn a_prediction_order_book_request_is_classified_on_the_events_route() {
        use market_participants_model::{
            identifier::{IdentifierValue, VenueId, VenueScopeId},
            outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId},
            prediction::PredictionEventRef,
        };

        use crate::market_participants::model::PredictionOrderBookQuery;

        let identity = |digits: &str| format!("v1/{}", digits.repeat(64));
        let route = VenueRoute::new(
            VenueId::Polymarket,
            VenueScopeId::new("predict").expect("the prediction scope is canonical"),
        );
        let query = PredictionOrderBookQuery {
            event: PredictionEventRef {
                route: route.clone(),
                native_event: IdentifierValue::new("1262243")
                    .expect("the fixture event id is valid"),
            },
            legs: Vec::new(),
            leg: OutcomeLegRef {
                group: OutcomeGroupId::from_identity_value(&identity("1"))
                    .expect("the fixture group identity is valid"),
                member: OutcomeMemberId::from_identity_value(&identity("2"))
                    .expect("the fixture member identity is valid"),
                side: OutcomeSideId::from_identity_value(&identity("3"))
                    .expect("the fixture side identity is valid"),
            },
        };

        for request in [
            ProtocolRequest::FetchPredictionOrderBook(query.clone()),
            ProtocolRequest::SubscribePredictionOrderBook(query),
        ] {
            let classified = classify(&request);
            assert_eq!(
                classified.capability(),
                market_participants_model::routing::CapabilityId::PredictionOrderBook
            );
            assert_eq!(classified.target().route(), &route);
            assert_eq!(classified.listing(), None);
            assert_eq!(classified.command(), None);
        }
    }
}
