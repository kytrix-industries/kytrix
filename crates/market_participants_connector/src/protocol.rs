//! The protocol boundary: request identity, targets and the request classifier.
//!
//! Every market-participants request names exactly one target. A target is either an
//! authenticated account, a public wallet address, a route-level public stream,
//! or the route itself; there is no request without one, because a request
//! without a target could not be validated against a registered scope.

use market_participants_model::{
    identifier::{MarketParticipantAccountId, WalletAddress},
    listing::{listing_impl::ListingStatus, route::VenueRoute},
    routing::CapabilityId,
};

pub(crate) use crate::adapter::{ProtocolRequest, ProtocolResponse, SubscriptionHandle};

/// Routing information supplied by a capability implementation for one protocol call.
///
/// The target is the only route/capability source: `route` and `capability` are
/// The identity of one protocol call: the route it addresses and the capability
/// it is classified as.
///
/// The two are stated rather than derived from a network target: a route and a
/// capability are what a request is, and the target a persisted configuration is
/// keyed by is not part of a request any more. The gateway still derives
/// the target the connection layer needs from these two, once, inside itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestIdentity {
    route: VenueRoute,
    capability: CapabilityId,
}

impl RequestIdentity {
    /// Builds the identity of one request.
    pub fn new(route: VenueRoute, capability: CapabilityId) -> Self {
        Self { route, capability }
    }

    /// Returns the route this request addresses.
    pub const fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the capability this request is classified as.
    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }
}

/// The account one authenticated request acts for.
///
/// Only the account and its route are stored: the capability comes from the
/// request context, so an authenticated target can never disagree with the
/// descriptor that resolved it.
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
    /// Builds an authenticated target.
    pub fn new(account: MarketParticipantAccountId, route: VenueRoute) -> Self {
        Self { account, route }
    }

    /// Returns the account this target acts for.
    pub const fn account(&self) -> &MarketParticipantAccountId {
        &self.account
    }

    /// Returns the route this target acts on.
    pub const fn route(&self) -> &VenueRoute {
        &self.route
    }
}

/// How one wallet-monitor request reaches the wallet it reads.
///
/// The two variants are not interchangeable: a public address names the wallet it
/// reads, while a route-level stream carries no address at all. Neither looks up a
/// trading credential — a wallet monitored by address is read as public state — and
/// the type carries no serde derive: nothing outside this crate builds one, so there
/// is no wire form a target could be decoded from either.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WalletMonitorTarget {
    /// A public address read, which never requires a trading credential.
    Public {
        route: VenueRoute,
        address: WalletAddress,
    },
    /// A public stream of one route that carries no wallet address.
    Route { route: VenueRoute },
}

impl WalletMonitorTarget {
    /// Returns the route this target reads, whichever access mode it uses.
    pub fn route(&self) -> &VenueRoute {
        match self {
            Self::Public { route, .. } | Self::Route { route } => route,
        }
    }
}

/// What a request does to a listing, which decides whether its status allows it.
///
/// The distinction is the model's: opening or changing exposure is a
/// trading command, and the operations that complete an expiration are
/// settlement commands that an expired listing still allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandKind {
    /// Opens or changes exposure: placing an order, setting leverage or margin mode.
    NewTrading,
    /// Cancels an existing order.
    CancelOrder,
    /// Completes an expiration: future delivery, option exercise, outcome settlement.
    Settlement,
    /// Reads state and changes nothing.
    Read,
}

/// The identity one protocol request addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestTarget {
    /// A route-level request: catalog, all tickers or a force-order subscription.
    Route(VenueRoute),
    /// A request about one listing.
    Listing(market_participants_model::listing::route::VenueListingKey),
    /// An authenticated request about the account of one route.
    Account(AuthenticatedTarget),
    /// A wallet-monitor request.
    Wallet(WalletMonitorTarget),
}

impl RequestTarget {
    /// Returns the route this request addresses.
    pub fn route(&self) -> &VenueRoute {
        match self {
            Self::Route(route) => route,
            Self::Listing(listing) => &listing.route,
            Self::Account(target) => target.route(),
            Self::Wallet(target) => target.route(),
        }
    }

    /// Returns the authenticated target, when this request authenticates.
    pub fn authenticated(&self) -> Option<&AuthenticatedTarget> {
        match self {
            Self::Account(target) => Some(target),
            // A wallet-monitor request names no account: it is a public address read or a
            // route-level stream, and both are read without a trading credential.
            Self::Wallet(_) | Self::Route(_) | Self::Listing(_) => None,
        }
    }
}

/// What one protocol request declares about itself.
///
/// The listing a request is about is not its target: a command and an account
/// read address an authenticated account *and* name the listing they act on, so
/// the listing travels with the classification instead of being read out of one
/// target variant. A request that names no listing — an account-wide balance
/// read, an account-wide position mode change — declares none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedRequest {
    capability: CapabilityId,
    target: RequestTarget,
    command: Option<CommandKind>,
    listing: Option<market_participants_model::listing::route::VenueListingKey>,
}

impl ClassifiedRequest {
    /// Builds a classification.
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

    /// Returns the capability the request needs.
    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    /// Returns the identity the request addresses.
    pub const fn target(&self) -> &RequestTarget {
        &self.target
    }

    /// Returns the listing key the request is about, when it is about one.
    pub const fn listing(
        &self,
    ) -> Option<&market_participants_model::listing::route::VenueListingKey> {
        self.listing.as_ref()
    }

    /// Returns the account the request authenticates as, when it does.
    pub fn authenticated_account(
        &self,
    ) -> Option<&market_participants_model::identifier::MarketParticipantAccountId> {
        self.target
            .authenticated()
            .map(AuthenticatedTarget::account)
    }

    /// Returns the command the request is, when it is one.
    pub const fn command(&self) -> Option<CommandKind> {
        self.command
    }
}

/// Classifies one protocol request into the capability and target it needs.
///
/// This is the single table of the protocol: every market-participants request appears here with an
/// explicit target, and no market-participant variant is absent. The per-variant payload
/// accessors are the adapter boundary's, so the table never guesses a route from
/// a venue.
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
        Request::FetchAllTickers(query) => route_request(Capability::Tickers, query.route()),
        Request::FetchOrderBook(query) | Request::SubscribeOrderBook(query) => {
            listing_request(Capability::OrderBook, &query.listing, None)
        }
        Request::FetchTrades(query) => {
            listing_request(Capability::MarketTrades, &query.listing, None)
        }
        Request::SubscribeTrades(query) => {
            listing_request(Capability::MarketTrades, &query.listing, None)
        }
        Request::SubscribeForceOrders(query) => {
            route_request(Capability::ForceOrders, query.route())
        }
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
            wallet_route_request(Capability::WalletMonitor, query.route())
        }
    }
}

/// Classifies a request that addresses a route and changes nothing.
fn route_request(capability: CapabilityId, route: &VenueRoute) -> ClassifiedRequest {
    ClassifiedRequest::new(capability, RequestTarget::Route(route.clone()), None, None)
}

/// Classifies a request about one listing of a route.
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

/// Classifies an authenticated request.
///
/// The gateway checks the listing route a command or a narrowed account read
/// names against the context route separately; what authentication resolves is
/// the account, so that is what the classified target carries, and the listing a
/// request acts on travels next to it.
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

/// Classifies a wallet-monitor request.
fn wallet_request(target: &WalletMonitorTarget) -> ClassifiedRequest {
    ClassifiedRequest::new(
        CapabilityId::WalletMonitor,
        RequestTarget::Wallet(target.clone()),
        None,
        None,
    )
}

/// Classifies a route-level wallet-monitor request.
fn wallet_route_request(capability: CapabilityId, route: &VenueRoute) -> ClassifiedRequest {
    ClassifiedRequest::new(
        capability,
        RequestTarget::Wallet(WalletMonitorTarget::Route {
            route: route.clone(),
        }),
        None,
        None,
    )
}

/// Validates the listing status against the command a request performs.
///
/// The matrix is the model's: `Scheduled`, `Halted`, `Settled` and
/// `Delisted` never allow new trading, an `Expired` listing still allows the
/// settlement commands that complete its expiration, and `Delisted` allows
/// nothing at all. Cancelling an order stays possible while the venue still
/// holds it, so it is only refused once the listing is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListingCommandMatrix;

impl ListingCommandMatrix {
    /// Returns whether one listing status permits one command kind.
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

    /// Returns whether one status and window permit one command at one instant.
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

    /// The documented matrix, written out row by row so a widened permission
    /// shows up as a failing expectation rather than passing silently.
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

    /// The account a fixture request authenticates as.
    fn account(route: &VenueRoute) -> AuthenticatedTarget {
        AuthenticatedTarget::new(
            MarketParticipantAccountId::new("primary").expect("the fixture account is valid"),
            route.clone(),
        )
    }

    /// A command and a narrowed account read declare the listing they act on,
    /// even though both are classified into the authenticated target.
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

    /// An account-wide read names no listing, and neither does a route-scoped
    /// request or a wallet read.
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
            market_participants_model::routing::CapabilityId::WalletMonitor
        );
    }
}
