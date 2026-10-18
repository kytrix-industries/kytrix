//! The market-trades tape widget.
//!
//! The widget owns one listing, and draws whatever the live trade stream
//! publishes for it. Everything the tape *does* around a frame — connecting,
//! loading the opening page, failing — is reported through the framework's status
//! cell; the body only ever draws the trades it has.

use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    empty_state::EmptyState,
    style::theme::{Theme, ThemeExt},
    table::{Table, TableDataSet},
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Empty, Entity, Hsla, InteractiveElement,
    IntoElement, Length, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, base::Align, div, relative,
};
use i18n::t;
use instrument_selector_ui::{
    instrument_button::InstrumentButton,
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope, offers_route,
    },
};
use market_participants_connector::{
    catalog::{display::catalog::DisplaySnapshot, error::CatalogError},
    connector::MarketParticipantsConnector,
    market::error::MarketError,
    venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    asset::AssetId,
    identifier::VenueId,
    listing::route::VenueListingKey,
    market::{
        selection::InstrumentSelection,
        units::{QuantityUnit, Side},
    },
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::market_is_catalog_not_loaded,
    identity::{WidgetId, WidgetInstanceId},
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    model::{MAX_TRADES, MarketTradesSnapshot},
    service::{
        MarketTradesProtocol, MarketTradesRequest, MarketTradesResponse, MarketTradesService,
        MarketTradesSource, MarketTradesStatus, MarketTradesStreamKey, MarketTradesStreamMessage,
    },
};

/// How many trades the opening page asks for.
const HISTORY_LIMIT: u16 = 200;
const COLUMN_COUNT: usize = 3;
/// Column widths in header order: price, amount, time.
///
/// The shared table draws every column at its declared width, so the columns are
/// sized by share of the pane instead of by a minimum that would need scrolling.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.4)),
    Length::Definite(relative(0.35)),
    Length::Definite(relative(0.25)),
];

/// The listing a tape follows, persisted with the widget.
///
/// The selection is optional because a tape can exist before the user has chosen
/// anything: it then asks for a listing instead of streaming a default nobody
/// asked for.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MarketTradesConfig {
    selection: Option<InstrumentSelection>,
}

impl WidgetConfig for MarketTradesConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The units the columns of the tape are named with.
///
/// A price is quoted in an asset and a traded quantity is counted in a unit, and
/// both are carried by every trade rather than by the pair the user picked. The
/// newest trade is what the headers name; a tape that has printed nothing has no
/// unit to name either, so its headers stay generic.
struct MarketTradeUnits {
    price: SharedString,
    quantity: SharedString,
}

impl MarketTradeUnits {
    fn of(snapshot: &MarketTradesSnapshot, cx: &App) -> Option<Self> {
        let trade = snapshot.rows().last()?.trade();
        // One capture names both headers, so they are read from the same
        // publication. It is taken when the table accepts a snapshot, not once per
        // frame: the table keeps the units it read, so a header can still name a
        // generation the catalog has already replaced until the next snapshot
        // reaches the table.
        let display = cx
            .global::<MarketParticipantsConnector>()
            .catalog()
            .cached_display();

        Some(Self {
            price: asset_display(&display, &trade.price.quote_asset, cx),
            quantity: quantity_unit_label(&display, &trade.quantity.unit, cx),
        })
    }
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalog knows what an asset is called: the canonical identity
/// of a token is a digest, and the code the catalog publishes is the whole name of
/// the asset. An asset the captured generation does not
/// hold is the localized unknown rather than an identity digest, because a reader
/// cannot use a hash and a name invented here would disagree with every other
/// pane.
fn asset_display(display: &DisplaySnapshot, asset: &AssetId, cx: &App) -> SharedString {
    display.resolver().asset_code(asset).map_or_else(
        |_| t!(cx, "error.display.unknown_asset"),
        |code| SharedString::new(code.as_str()),
    )
}

/// The label of one quantity unit, named the way that unit is measured.
fn quantity_unit_label(display: &DisplaySnapshot, unit: &QuantityUnit, cx: &App) -> SharedString {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(display, asset, cx)
        }
        QuantityUnit::Contracts => t!(cx, "widget.market_trades.contracts"),
        QuantityUnit::OutcomeTokens { .. } => t!(cx, "widget.market_trades.outcome_tokens"),
    }
}

/// The trade rows rendered by the body table.
///
/// The table is a separate entity because [`Table`] reads its rows from an
/// [`Entity`]; it keeps its own invalidation source, so a snapshot that changes
/// nothing the table draws does not rebuild it. It is the body's only expensive
/// region, so it is the region the body caches.
struct MarketTradesTable {
    /// The accepted snapshot, or nothing before the first one arrives.
    snapshot: Option<MarketTradesSnapshot>,
    /// The units the column headers name, taken from the snapshot they describe.
    units: Option<MarketTradeUnits>,
    /// Direction colors, resolved once because `TableDataSet::cell` renders
    /// without a window or application context.
    buy: Hsla,
    sell: Hsla,
}

impl MarketTradesTable {
    fn new(theme: &Theme) -> Self {
        Self {
            snapshot: None,
            units: None,
            buy: theme.roles.market.up.hsla,
            sell: theme.roles.market.down.hsla,
        }
    }
}

impl TableDataSet<COLUMN_COUNT> for MarketTradesTable {
    fn rows_count(&self) -> usize {
        self.snapshot.as_ref().map_or(0, MarketTradesSnapshot::len)
    }

    /// Each column is named with the unit it is measured in.
    ///
    /// A price without its quote currency is not readable, and the unit a traded
    /// quantity is counted in is not the same on every product, so the units come
    /// from the trades themselves rather than from a fixed label.
    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        self.units.as_ref().map_or_else(
            || {
                [
                    t!(cx, "common.price"),
                    t!(cx, "common.amount"),
                    t!(cx, "common.time"),
                ]
            },
            |units| {
                [
                    t!(
                        cx,
                        "widget.market_trades.price_with_unit",
                        unit = units.price.clone()
                    ),
                    t!(
                        cx,
                        "widget.market_trades.amount_with_unit",
                        unit = units.quantity.clone()
                    ),
                    t!(cx, "common.time"),
                ]
            },
        )
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        [Align::End, Align::End, Align::End]
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    fn cell(&self, row_index: usize, column_index: usize, _cx: &App) -> impl IntoElement {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Empty.into_any_element();
        };
        // The newest trade is displayed first, so display order is the reverse of
        // the chronological snapshot order.
        let row = row_index
            .checked_add(1)
            .and_then(|offset| snapshot.len().checked_sub(offset))
            .and_then(|index| snapshot.rows().get(index));
        let Some(row) = row else {
            return Empty.into_any_element();
        };
        let display = row.display();

        match column_index {
            // The price carries the direction colour: a tape is read by which side
            // crossed the spread, and the shared table cannot tint a whole row.
            0 => {
                let color = if row.trade().side == Side::Buy {
                    self.buy
                } else {
                    self.sell
                };
                div()
                    .text_color(color)
                    .child(display.price.clone())
                    .into_any_element()
            }
            1 => display.quantity.clone().into_any_element(),
            2 => display.time.clone().into_any_element(),
            _ => Empty.into_any_element(),
        }
    }

    /// A live tape that has printed nothing yet is an empty tape, which is a fact
    /// rather than a state: the table says so instead of leaving the body under its
    /// headers blank.
    fn empty(&self, cx: &App) -> Option<AnyElement> {
        Some(EmptyState::new("MarketTradesEmpty", t!(cx, "common.no_data")).into_any_element())
    }
}

pub struct MarketTradesWidget {
    config: MarketTradesConfig,
    binding: WidgetBinding<MarketTradesProtocol>,
    table: Entity<MarketTradesTable>,
    status: Entity<WidgetStatus>,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    snapshot: Option<MarketTradesSnapshot>,
    pending_snapshot: Option<MarketTradesSnapshot>,
    ready_received: bool,
    awaiting_snapshot: bool,
    /// Whether the tape is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the market
    /// again once the user loads it — so a trade that was in flight when the route
    /// went away can still arrive. Accepting it would put a snapshot back and make the
    /// pane report content over the notice the framework is drawing, so every message
    /// entry refuses while this is set. The stream key cannot do this job: it says
    /// which scope a trade belongs to, and this one belongs to the scope the notice is
    /// about.
    awaiting_market: bool,
    /// Whether the venue this tape reads was loaded or refreshed again.
    ///
    /// Such a commit replaces the catalog generation the live stream resolved its listing
    /// against, while the stream itself keeps the rules, status and identifiers it resolved
    /// when it was opened. So the tape has to start over, and the request it sends has to be
    /// the one that opens a new stream: [`Self::load`] reads this after its own gate — a tape a
    /// shrinking commit sent back to waiting keeps it, so the load that brings the market back
    /// reopens the stream too — and it is what makes the request a `Retry` rather than the
    /// idempotent `Load`, which the service answers without touching a stream for that listing
    /// that is already live.
    market_reloaded: bool,
    directory_generation: u64,
}

impl WidgetView for MarketTradesWidget {
    type Config = MarketTradesConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.market_trades");
    const DISPLAY_NAME: &'static str = "widget.names.market_trades";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "Recent native market executions";
    const ICON: Option<&'static str> = Some("widgets/market_trades.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<MarketParticipantsConnector>().market();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// The table is the content, and it is ready as soon as a snapshot has been
    /// accepted — including an empty one. An empty tape is what "nothing has
    /// printed yet" looks like; it is not a state the widget reports.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The market this tape follows has to be loaded before anything else
        // happens here, and the check is first on purpose: a route the user never
        // loaded, or one a shrinking commit withdrew, leaves the pane waiting for it
        // instead of reaching the branches below, which settle the selection the
        // user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // Read past the gate: a commit on the tape's own venue that took its market away leaves
        // this set while the stream that resolved against the retired generation is still alive,
        // and the load that brings the market back has to reopen that stream.
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx);
        if !retry {
            // An explicit load starts over, because the widget cannot vouch for what it
            // was showing. A retry keeps the trades the user is still reading.
            self.clear_trades(cx);
        }
        // The two decisions are separate here: the trades still go for a reload — nothing on
        // screen can be vouched for after the generation behind it was replaced — while the
        // request is the replacing one.
        self.start(retry || market_reloaded, cx);
    }

    /// The listing picker is the whole of what this widget puts in its header.
    ///
    /// It is the panes' own control, and it disables itself while the loaded venues offer it
    /// nothing to select from — the state in which the market participant's metadata this pane reads is not
    /// in memory yet.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().left(self.instrument_button(cx))
    }
}

impl MarketTradesWidget {
    fn new_with_source<S>(
        config: MarketTradesConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: MarketTradesSource,
    {
        let service = MarketTradesService::new(source, HISTORY_LIMIT, MAX_TRADES);
        let binding = WidgetBinding::spawn(
            global::handle(cx).handle(),
            service,
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let table = cx.new(|cx| MarketTradesTable::new(cx.theme()));
        // The pane recovers from the market it waits for without polling: a venue commit is
        // the one event that can change what may be read, and a change that arrives while the
        // pane has nothing to show is a reason to load again.
        let events = cx.global::<MarketParticipantsConnector>().venues().events();
        cx.subscribe(&events, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        })
        .detach();

        Self {
            config,
            binding,
            table,
            status,
            recovery,
            config_change,
            snapshot: None,
            pending_snapshot: None,
            ready_received: false,
            awaiting_snapshot: true,
            awaiting_market: false,
            market_reloaded: false,
            directory_generation: 0,
        }
    }

    /// Whether what this tape needs is in memory: at least one market that answers with trades,
    /// and the route of the stored selection among them.
    ///
    /// A tape that follows nothing still needs a market: the picker is drawn from the loaded
    /// venues, so with none of them loaded there is nothing to choose from — the pane waits for
    /// the market participant's metadata and says so, rather than asking for a choice that is not there.
    /// Once one is loaded, a tape that still follows nothing is the "select an instrument"
    /// notice's business and this answers `true` for it.
    ///
    /// Reading a market is only possible once the user has loaded it, and "not
    /// loaded" is a wait rather than a failure. One question answers it: a scope in
    /// the loaded venues is a scope whose catalog committed with it.
    fn market_ready(&self, cx: &App) -> bool {
        if !Self::any_market_loaded(cx) {
            return false;
        }

        let Some(route) = self
            .config
            .selection
            .as_ref()
            .map(|selection| &selection.listing.route)
        else {
            return true;
        };

        cx.global::<MarketParticipantsConnector>()
            .venues()
            .scope(route)
            .is_some()
    }

    /// Reports the neutral notice for the market this tape follows, and drops the
    /// trades it was showing.
    ///
    /// The route behind that tape is gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: the
    /// stream it was reading is still alive, and a trade that was already in flight
    /// would otherwise put content back over the notice. The stored selection is left
    /// alone: it is the user's, and loading the market again is what brings it back.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.clear_trades(cx);
        self.report_markets_not_loaded(cx);
        cx.notify();
    }

    /// Reports the notice naming the market this tape follows.
    ///
    /// The list the sentence ends with is every venue that answers with trades — the
    /// capability this pane is defined by, read off the definition table rather than off
    /// what happens to be loaded — while the market it waits for is the one behind the
    /// selection, or that whole list while the user follows nothing and no single venue
    /// can be named.
    ///
    /// This is the one place that reference is produced: [`Self::wait_for_market`] reports it
    /// when the route behind the tape goes away, or while no market at all is loaded and there
    /// is nothing to pick a listing from. The sentence is the framework's to resolve, in
    /// the language showing on the frame that draws it.
    fn report_markets_not_loaded(&self, cx: &mut Context<Self>) {
        let supported = Self::listed_venues();
        let venues = self
            .config
            .selection
            .as_ref()
            .map(|selection| vec![selection.listing.route.venue])
            .unwrap_or_else(|| supported.clone());

        self.status.report(
            WidgetState::waiting(Notice::MarketNotLoaded { venues, supported }),
            cx,
        );
    }

    /// Reports the prompt asking for a listing.
    ///
    /// This is the one place that reference is produced: [`Self::nothing_chosen`] reports it
    /// when the tape loses its listing.
    fn report_nothing_chosen(&self, cx: &mut Context<Self>) {
        self.status.report(
            WidgetState::waiting(Notice::message(
                "widget.market_trades.select_instrument_prompt",
            )),
            cx,
        );
    }

    /// Reports the neutral notice for a read the directory answered "not loaded"
    /// to, and drops the trades it was showing.
    ///
    /// The pane's own gate asks the directory before it starts a stream, so this is
    /// the answer to a question the gate could not have seen the answer to: the route
    /// was revoked between the two, which is what a venue re-discovering its scopes
    /// does. That is a wait rather than a failure — the connection action is
    /// what puts the catalog back. Returns whether it reported, so a caller keeps its
    /// own failure path for everything else.
    fn report_waiting_for_directory(
        &mut self,
        error: &MarketError,
        cx: &mut Context<Self>,
    ) -> bool {
        if !market_is_catalog_not_loaded(error) {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// A commit can matter to this tape for either of two reasons. The first is that it changes
    /// whether the market it needs is in memory, and the pane already holds that answer in the two
    /// things it maintains anyway: `awaiting_market` says it is not reading because a market it
    /// needs is missing, and `market_ready` says everything it needs is there now. A pane that is
    /// waiting for one and can now read starts over rather than re-checking; a pane that was
    /// reading one that is gone goes back to waiting.
    ///
    /// The second is that it landed on the venue behind the stored listing, which is the user
    /// loading or refreshing the market this tape reads: the catalog generation the live stream
    /// resolved its listing against is replaced, so the tape starts over — see `market_reloaded`,
    /// which is what makes the request the one that opens a new stream. A commit of any other venue
    /// is neither, and leaves the pane and its stream exactly as they are. A failure is terminal:
    /// the event does not revive a failed pane, only the user's own recovery does.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on the venue the stored listing is on is this tape's own market being
        // loaded again, so the tape cannot keep reading the stream it holds.
        if self
            .config
            .selection
            .as_ref()
            .is_some_and(|selection| selection.listing.route.venue == event.venue)
        {
            self.market_reloaded = true;
            self.load(cx);
            return;
        }
        if self.awaiting_market {
            if self.market_ready(cx) {
                self.load(cx);
            }
            return;
        }
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
        }
    }

    /// The venues the waiting notice lists: every venue whose loader can answer with trades.
    ///
    /// A tape reads one venue — the one behind the instrument the user followed — so the
    /// market it waits for follows the selection. What it can read at all does not: that is
    /// this capability's venue set off the definition table, which is the same answer whether
    /// or not anything is loaded and whichever instrument is followed.
    fn listed_venues() -> Vec<VenueId> {
        market_participants_connector::venue::definition::venues_supporting(
            CapabilityId::MarketTrades,
        )
    }

    /// Whether any market this tape could read is loaded, whatever the user selected.
    ///
    /// This is the first half of the pane's precondition, and it is asked before the selection is
    /// looked at on purpose: the picker a tape with nothing followed draws is built from the
    /// loaded venues, so with none of them loaded there is nothing to choose from, and "nothing is
    /// chosen" and "the market is not loaded" are one and the same thing to the user.
    fn any_market_loaded(cx: &App) -> bool {
        let loaded = cx.global::<MarketParticipantsConnector>().venues();
        Self::listed_venues()
            .iter()
            .any(|venue| loaded.loaded(venue).is_some())
    }

    /// The stream scope this tape would accept messages from.
    ///
    /// Absent until a listing is chosen: nothing is streaming, so nothing is in
    /// scope.
    /// The stream scope this tape would accept messages from.
    ///
    /// Absent until a listing is chosen: nothing is streaming, so nothing is in
    /// scope.
    fn current_key(&self) -> Option<MarketTradesStreamKey> {
        self.config
            .selection
            .as_ref()
            .map(|selection| MarketTradesStreamKey::new(selection.listing.clone()))
    }

    /// Puts the stored selection back to "nothing chosen" when the listing cannot be
    /// followed any more.
    ///
    /// The value is one this build can no longer act on — the route left the
    /// registry, or the venue delisted the listing — so it goes back to the config
    /// default and the pane is told, or the next start would only reset it again.
    fn withdraw(&mut self, listing: &VenueListingKey, cause: &str, cx: &mut Context<Self>) {
        log::warn!(
            "`{}` {cause}; the tape went back to having nothing chosen",
            listing.listing_id
        );
        self.config.selection = None;
        self.config_change.publish(cx);
        self.nothing_chosen(cx);
    }

    /// Applies what the directory answered about the stored listing.
    ///
    /// Returns whether this answer still describes the scope the tape follows, which is
    /// what decides whether the subscription for it starts. The lookup is advisory: only
    /// an answer that proves the listing cannot be followed resets the selection — a
    /// listing the route does not publish, and one the venue has settled or delisted. A
    /// lookup that failed proves nothing — the venue may simply not have answered — so it
    /// is logged and the selection is kept, and the subscription that follows reports its
    /// own error, which the user can recover from. That request is the authority on
    /// whether the listing can be followed, because the market participants connector resolves it —
    /// rules, status and native identifiers — before anything is sent: a catalog or
    /// network failure therefore reaches the user as the failure it is instead of being
    /// dressed up as a listing nobody lists. A choice made while the lookup was in flight
    /// is theirs, and it started a subscription of its own.
    fn settle_selection(
        &mut self,
        selection: InstrumentSelection,
        support: ListingSupport,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.config.selection.as_ref() != Some(&selection) {
            return false;
        }
        if support == ListingSupport::Withdrawn {
            self.withdraw(&selection.listing, "is no longer offered by its route", cx);
        }
        true
    }

    /// Publishes the accepted snapshot to the table entity.
    ///
    /// The table is the only part of the body that renders rows, so it is
    /// notified at the exact boundaries where what it draws changes — the
    /// accepted snapshot, and the units its headers are labelled with.
    /// Notifying it is what busts the body's cache; nothing else repaints a cached
    /// body.
    fn sync_table(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.snapshot.clone();
        self.table.update(cx, |table, cx| {
            table.units = snapshot
                .as_ref()
                .and_then(|snapshot| MarketTradeUnits::of(snapshot, cx));
            table.snapshot = snapshot;
            cx.notify();
        });
    }

    fn clear_trades(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.sync_table(cx);
    }

    fn apply_snapshot(&mut self, snapshot: MarketTradesSnapshot, cx: &mut Context<Self>) {
        self.snapshot = Some(snapshot);
        self.awaiting_snapshot = false;
        self.status.report(WidgetState::Ready, cx);
        self.sync_table(cx);
    }

    /// Settles the stored scope against the route, and starts the tape on it.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        // The pane reached here only past its own gate, so the market it was waiting
        // for is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let generation = self.directory_generation;
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        // A stored listing the route no longer offers is settled before the tape
        // subscribes to it: subscribing first would report a failure about a scope the user
        // cannot return to anyway. The lookup is work this tape is doing, so it reports
        // connecting while it is in flight — the request that follows is what connects.
        // That request is also what settles the scope for real: see `Self::settle_selection`.
        self.status.report(WidgetState::Connecting, cx);
        cx.notify();
        // What the loaded venues answer is read here, where the settlement is decided,
        // rather than remembered: a venue the user loads after this tape started reading
        // carries a scope it may follow, and one a shrinking commit withdrew carries none.
        let routes = LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues());
        // A route that is not loaded cannot be asked anything at all, so it is
        // settled without a lookup: the catalog would answer with the same refusal.
        if !offers_route(
            &routes,
            &selection.listing.route,
            CapabilityId::MarketTrades,
        ) {
            self.withdraw(&selection.listing, "is no longer a loaded route", cx);
            return;
        }
        let lookup = cx
            .global::<MarketParticipantsConnector>()
            .catalog()
            .contains(&selection.listing);
        cx.spawn(async move |this, cx| {
            let offered = lookup;
            let _ = this.update(cx, |this, cx| {
                // An answer about a scope the user has left is not this tape's to apply:
                // their own choice started a subscription of its own.
                if this.directory_generation != generation {
                    return;
                }
                let support = listing_support(&selection.listing, offered, cx);
                if !this.settle_selection(selection, support, cx) {
                    return;
                }
                let Some(selection) = this.config.selection.clone() else {
                    this.nothing_chosen(cx);
                    return;
                };
                this.load_trades(selection, retry, cx);
            });
        })
        .detach();
    }

    /// Asks the service for the settled scope.
    fn load_trades(&mut self, selection: InstrumentSelection, retry: bool, cx: &mut Context<Self>) {
        let listing = selection.listing;
        let request = if retry {
            MarketTradesRequest::Retry { listing }
        } else {
            MarketTradesRequest::Load { listing }
        };
        self.binding.call(request, cx, Self::apply_rpc_result);
        cx.notify();
    }

    /// Says what a tape with nothing to follow is doing: nothing, and it is not a failure.
    ///
    /// There is nothing to follow, so there is no market to read and nothing the tape
    /// can vouch for. It has no content of its own, so it says what it is waiting for
    /// instead of reporting work it is not doing.
    fn nothing_chosen(&mut self, cx: &mut Context<Self>) {
        self.clear_trades(cx);
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        self.report_nothing_chosen(cx);
        cx.notify();
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<MarketTradesProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(MarketTradesResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status
                    .report(WidgetState::failed(Arc::new(error), recovery), cx);
            }
        }
    }

    fn apply_reliable(
        &mut self,
        key: MarketTradesStreamKey,
        message: Arc<MarketTradesStreamMessage>,
        cx: &mut Context<Self>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return;
        }

        match message.as_ref() {
            MarketTradesStreamMessage::Status(status) => match status {
                MarketTradesStatus::Connecting => {
                    if self.snapshot.is_none() {
                        self.status.report(WidgetState::Connecting, cx);
                    }
                }
                MarketTradesStatus::LoadingHistory => {
                    if self.snapshot.is_none() {
                        self.status.report(WidgetState::Loading, cx);
                    }
                }
                MarketTradesStatus::Ready => {
                    self.ready_received = true;
                    if let Some(snapshot) = self.pending_snapshot.take() {
                        self.apply_snapshot(snapshot, cx);
                    } else if !self.awaiting_snapshot {
                        self.status.report(WidgetState::Ready, cx);
                    }
                }
                MarketTradesStatus::Error(error) => {
                    if self.report_waiting_for_directory(error, cx) {
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status
                        .report(WidgetState::failed(Arc::new(error.clone()), recovery), cx);
                }
            },
            MarketTradesStreamMessage::Snapshot(_) => {}
        }
    }

    fn apply_latest(
        &mut self,
        key: MarketTradesStreamKey,
        message: Arc<MarketTradesStreamMessage>,
        cx: &mut Context<Self>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return;
        }

        if let MarketTradesStreamMessage::Snapshot(snapshot) = message.as_ref() {
            if self.ready_received {
                self.apply_snapshot(snapshot.clone(), cx);
            } else {
                self.pending_snapshot = Some(snapshot.clone());
            }
        }
    }

    /// Applies a selector result as one configuration transition.
    ///
    /// Returns whether the selection was accepted. A listing key is the whole of a
    /// scope, so two listings of one instrument are two selections and neither is
    /// translated into the other.
    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut Context<Self>) -> bool {
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx);
        // The trades on screen belong to the scope the user just left.
        self.clear_trades(cx);
        self.start(false, cx);
        true
    }

    /// The filters the picker this control opens is built with.
    ///
    /// The loaded venues are read here, where the control is built, rather than kept: the
    /// header is rebuilt every frame, so a scope loaded after this tape started reading is a
    /// scope the user may select, and one a shrinking commit withdrew is not. A venue commit
    /// re-runs [`Self::load`] only while the pane is waiting for a market, so a reading kept
    /// from the last load would stop offering every venue the user loads later. This is the
    /// whole of that rule, and it is what the freshness test asserts.
    fn instrument_filters(&self, cx: &App) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::MarketTrades,
            entry_predicate: None,
            routes: LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues()),
        }
    }

    /// The control that opens the listing selector.
    ///
    /// Before anything is chosen it offers the choice rather than reporting a
    /// scope, because there is no scope yet. What it states once a listing is
    /// followed — the published name and the route it is on — is the control's own
    /// text, so the tape and the dialog cannot disagree about which listing is
    /// followed.
    fn instrument_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        InstrumentButton::new(
            ElementId::Name(SharedString::new_static("MarketTradesInstrument")),
            self.config.selection.clone(),
            self.instrument_filters(cx),
            "widget.market_trades.select_instrument",
            move |selection, _, app| {
                // The selector does not keep the pane alive: a pane gone by the
                // time a listing is confirmed has nothing left to select into.
                let _ = widget.update(app, |this, cx| {
                    this.select_selection(selection, cx);
                });
            },
        )
        .into_any_element()
    }

    /// The ready-state body: the table, always.
    ///
    /// The body is cached by the pane, so nothing here needs to place a cache
    /// boundary. What this must do instead is notify on the boundary where what it
    /// draws changes — the table entity — because a cached body is not repainted
    /// for free.
    fn ready_body(&self, cx: &App) -> AnyElement {
        div()
            .id(ElementId::Name(SharedString::new_static(
                "MarketTradesTableFrame",
            )))
            .aria_label(t!(cx, "widget.market_trades.table"))
            .size_full()
            .min_h_0()
            .child(Table::new(
                // The table keeps its own scroll position in state keyed by this
                // id, so it has to stay stable across frames.
                ElementId::Name(SharedString::new_static("MarketTradesTable")),
                &self.table,
            ))
            .into_any_element()
    }
}

impl Render for MarketTradesWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the widget has content, so this is the
        // ready state and nothing else. Everything the widget is *doing* is drawn
        // by the pane around this.
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.ready_body(cx))
    }
}

/// How the stored listing stands against what its route still publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListingSupport {
    /// The route publishes the listing, and it still accepts followers.
    Offered,
    /// The listing is provably not followable: it left the catalog, or its venue
    /// settled or delisted it.
    Withdrawn,
    /// The route could not be asked, which proves nothing about the listing.
    Unknown,
}

/// What the published catalog answered about one listing.
///
/// Only two answers withdraw a stored listing: the route answered that it does not
/// publish it — including by reporting that it is not a registered route any more — or
/// the entry it publishes is in a terminal lifecycle state, which is a listing no venue
/// serves any more. Everything else — a venue that did not answer, a catalog that could
/// not be read — leaves the stored value alone, and the subscription that follows
/// reports what actually went wrong.
fn listing_support(
    listing: &VenueListingKey,
    offered: Result<bool, CatalogError>,
    cx: &App,
) -> ListingSupport {
    match offered {
        Ok(true) => match cx
            .global::<MarketParticipantsConnector>()
            .catalog()
            .cached_listing(listing)
        {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingSupport::Withdrawn,
            _ => ListingSupport::Offered,
        },
        Ok(false) => ListingSupport::Withdrawn,
        // A route the registry has withdrawn since is a scope nothing can be followed
        // on, and the directory says so itself: the registry snapshot a widget holds
        // was taken when it was built, so relying on it alone would leave the stored
        // listing in place until the stream failed.
        Err(CatalogError::RouteNotRegistered { .. }) => ListingSupport::Withdrawn,
        // A route nothing has loaded has not answered whether it publishes the
        // listing, so the stored value stays: the subscription that follows reaches
        // the venue and reports what actually went wrong — I10.
        Err(error) => {
            log::warn!(
                "asking whether `{}` is still published failed; the tape asks for the stream \
                 anyway: {error}",
                listing.listing_id
            );
            ListingSupport::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        future::Future,
        rc::Rc,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config::widget::VersionedWidgetConfig;
    use gpui_kit::{App, Entity, TestAppContext, WeakEntity};
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        catalog::{
            batch::CatalogEntry,
            error::{CatalogError, SourceError},
            store::Catalog,
            test_support::{self, Fixture},
        },
        market::{domain::DisconnectReason, error::MarketError},
    };
    use market_participants_model::{
        asset::{AssetId, CurrencyId, CurrencyTerms, Iso4217Code},
        decimal::{Price, Quantity, UnixMillis},
        listing::{
            listing_impl::ListingStatus, route::VenueRoute, rules::ListingPrecision,
            venue::PriceBasis,
        },
        market::{
            book::ListingTrade,
            price::MarketPrice,
            units::{MarketQuantity, MarketTradeId},
        },
    };
    use smol_str::SmolStr;
    use widget::error::LocalizedError;

    use super::*;
    use crate::{
        model::MarketTradesState,
        service::{MarketTradeEventSubscription, MarketTradesSource},
    };

    /// The route and listings every fixture is built on.
    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    /// The scope another venue's load commits, for the commits this tape does not follow.
    fn okx_spot_route() -> VenueRoute {
        test_support::route("okx", "spot")
    }

    fn listing() -> VenueListingKey {
        test_support::listing_key(&route(), "BTCUSDT")
    }

    fn other_listing() -> VenueListingKey {
        test_support::listing_key(&route(), "ETHUSDT")
    }

    /// The tape's listing again, published at another price tick.
    ///
    /// It is the shape a production load commits: the load re-fetches the catalogue, and what it
    /// publishes is the listing restated at the venue's own increments.
    fn listing_entry(price_tick: &str) -> CatalogEntry {
        let mut restated = test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        );
        restated.snapshot.rules.common.price.tick = price_tick
            .parse()
            .expect("the restated price tick should parse");
        restated
    }

    fn quote() -> AssetId {
        test_support::token("binance", "USDT")
    }

    fn base() -> AssetId {
        test_support::token("binance", "BTC")
    }

    fn selection(listing: VenueListingKey) -> InstrumentSelection {
        InstrumentSelection { listing }
    }

    fn market_price(value: u64) -> MarketPrice {
        MarketPrice {
            value: Price::from(value),
            quote_asset: quote(),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn traded_quantity(value: u64) -> MarketQuantity {
        MarketQuantity {
            value: Quantity::from(value),
            unit: QuantityUnit::AssetUnits { asset: base() },
        }
    }

    fn trade(id: u64, timestamp: UnixMillis, value: u64) -> ListingTrade {
        ListingTrade {
            listing: listing(),
            id: MarketTradeId::Numeric(id),
            price: market_price(value),
            quantity: traded_quantity(2),
            side: Side::Buy,
            timestamp,
        }
    }

    /// A lookup that failed, which is what a route that did not answer reports.
    fn no_answer() -> CatalogError {
        CatalogError::Source(Box::new(SourceError::Network {
            reason: SmolStr::new_static("the test venue did not answer"),
        }))
    }

    /// A tape built the way the framework builds one, over a source a test drives, a
    /// directory it arranges and the market it reads.
    ///
    /// The directory is published before the widget is constructed: a pane reads the
    /// catalog of a market the user has already loaded, and loading it is
    /// registration *and* a load of every catalog route. The widget is not loaded
    /// yet — the framework loads once construction is done, and a test decides when
    /// that is.
    fn tape<S>(
        config: MarketTradesConfig,
        source: S,
        directory: Fixture,
        change: WidgetConfigChange,
        cx: &mut TestAppContext,
    ) -> Entity<MarketTradesWidget>
    where
        S: MarketTradesSource + Clone,
    {
        tape_market(cx, config, source, directory, change, true)
    }

    /// The same, with the fixture venue loaded or left out of the loaded venues.
    fn tape_market<S>(
        cx: &mut TestAppContext,
        config: MarketTradesConfig,
        source: S,
        directory: Fixture,
        change: WidgetConfigChange,
        loaded: bool,
    ) -> Entity<MarketTradesWidget>
    where
        S: MarketTradesSource + Clone,
    {
        cx.executor().allow_parking();
        let entries = directory.entries().to_vec();
        let route = directory.route().clone();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let catalog = Arc::clone(directory.catalog_arc());
            install_connector(cx, &catalog, loaded);
            // The fixture route is published as the market's catalog, empty or not: a
            // route the venue answers nothing for is still a market the user has
            // loaded, and an empty catalog is what that market lists. The
            // publication follows the registration because a snapshot records the route
            // revision it was fetched under, and registering the route moves it on.
            if catalog.is_registered(&route) {
                test_support::publish_fixture_route(&catalog, &route, entries)
                    .expect("the fixture route should publish its catalog");
            }
            cx.new(|cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<MarketTradesWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                MarketTradesWidget::new_with_source(config, source, status, change, recovery, cx)
            })
        })
    }

    /// Installs the market participants root the pane reads its routing from.
    ///
    /// The registry is what says which routes a market has, so a test that drives a
    /// pane connects the fixture venue the way the connection action does: the pane
    /// reads the loaded venues again on every load, and a venue nobody loaded is one it
    /// waits for.
    ///
    /// The root is built over `catalog`, which is the directory's own catalogue:
    /// the pane's gate asks the directory and the service resolves listings
    /// against whatever the root owns, so two different catalogues would let a
    /// gate/read mismatch pass unnoticed.
    fn install_connector(cx: &mut App, catalog: &Arc<Catalog>, loaded: bool) {
        let connector =
            market_participants_connector::test_support::install_root(cx, Arc::clone(catalog));
        if loaded {
            connector
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::from_name(
                            test_support::VENUE,
                        )
                        .expect("the fixture venue is canonical"),
                    ]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
        }
    }

    /// A tape built the way the framework builds one, over a config a test reads back
    /// from the widget itself and a report it can count.
    fn widgets(
        cx: &mut TestAppContext,
        config: MarketTradesConfig,
        published: Rc<Cell<usize>>,
    ) -> Entity<MarketTradesWidget> {
        let change = WidgetConfigChange::new(move |_| {
            published.set(published.get() + 1);
        });
        tape(
            config,
            PendingMarketTradesSource,
            Fixture::unanswering(),
            change,
            cx,
        )
    }

    fn disconnect() -> MarketError {
        MarketError::Disconnected(DisconnectReason::Normal)
    }

    /// Lets a tape and its service run until what a test is waiting for has happened.
    ///
    /// Two executors are involved: the tape sends its commands from GPUI's own, and the
    /// service answers on the application runtime. So both are given a turn — the tape
    /// first, because a command that has not been sent is a service with nothing to
    /// answer.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(runtime_handle);
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The installed application runtime's handle.
    fn runtime_handle(cx: &mut App) -> tokio::runtime::Handle {
        global::handle(cx).handle().clone()
    }

    /// Counts the venue commits it is told about.
    ///
    /// A test that asserts a pane did *not* act on a commit still has to know the commit reached
    /// it: this probe subscribes the same way and after the pane did, so the delivery it counts is
    /// one the pane's own handler has already seen.
    struct CommitProbe {
        deliveries: Arc<AtomicUsize>,
    }

    impl CommitProbe {
        fn new(
            cx: &mut Context<Self>,
            events: &Entity<market_participants_connector::venue::loaded::VenueEventSource>,
            deliveries: Arc<AtomicUsize>,
        ) -> Self {
            cx.subscribe(events, |probe, _, _: &VenuesUpdated, _| {
                probe.deliveries.fetch_add(1, Ordering::AcqRel);
            })
            .detach();
            Self { deliveries }
        }
    }

    #[derive(Clone, Copy)]
    struct PendingMarketTradesSource;

    struct PendingMarketTradeSubscription;

    impl MarketTradesSource for PendingMarketTradesSource {
        type Subscription = PendingMarketTradeSubscription;

        fn fetch_market_trades(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
            std::future::pending()
        }

        fn subscribe_market_trades(
            &self,
            _listing: VenueListingKey,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            std::future::pending()
        }
    }

    /// A route that answers, and a record of what the tape subscribed to.
    #[derive(Clone, Default)]
    struct AnsweringMarketTradesSource {
        /// The listings the tape subscribed to, in order.
        subscribed: Arc<Mutex<Vec<VenueListingKey>>>,
    }

    impl AnsweringMarketTradesSource {
        /// The listings the tape subscribed to, in order.
        fn subscribed(&self) -> Vec<VenueListingKey> {
            self.subscribed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    /// A source that resolves a subscription through the catalogue the fixture publishes, at the
    /// moment it is asked for, and records the increments that listing was published at then.
    ///
    /// A stream's resolution is what a venue reload replaces, so a test about a reload has to see
    /// both that the tape asked for a new stream and which generation's increments that stream was
    /// opened at: this is the one seam where the two are a single record.
    #[derive(Clone)]
    struct ResolvingMarketTradesSource {
        catalog: Arc<Catalog>,
        resolutions: Arc<Mutex<Vec<ListingPrecision>>>,
    }

    impl MarketTradesSource for ResolvingMarketTradesSource {
        type Subscription = PendingMarketTradeSubscription;

        fn fetch_market_trades(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
            std::future::pending()
        }

        fn subscribe_market_trades(
            &self,
            listing: VenueListingKey,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            let published = self
                .catalog
                .cached_listing(&listing)
                .expect("the fixture publishes the listing the tape follows");
            self.resolutions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ListingPrecision::of(&published.snapshot.rules));
            std::future::pending()
        }
    }

    /// The increments every subscription one resolving source was asked for was published at.
    fn resolutions_of(resolutions: &Arc<Mutex<Vec<ListingPrecision>>>) -> Vec<ListingPrecision> {
        resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    impl MarketTradesSource for AnsweringMarketTradesSource {
        type Subscription = PendingMarketTradeSubscription;

        fn fetch_market_trades(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
            std::future::pending()
        }

        fn subscribe_market_trades(
            &self,
            listing: VenueListingKey,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            // Recorded before the stream is held open, so a test can tell which listing
            // the tape went and followed without the stream ever answering.
            self.subscribed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(listing);
            std::future::pending()
        }
    }

    impl MarketTradeEventSubscription for PendingMarketTradeSubscription {
        fn precision(&self) -> ListingPrecision {
            precision()
        }

        fn recv(
            &mut self,
        ) -> impl Future<Output = Option<Result<Arc<ListingTrade>, MarketError>>> + Send {
            std::future::pending()
        }
    }

    /// The increments the fixture listing publishes its numbers at.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity tick should parse"),
        }
    }

    /// A snapshot holding `(id, timestamp, price)` trades of the given listing.
    fn snapshot_of_listing(
        listing: VenueListingKey,
        trades: &[(u64, UnixMillis, u64)],
    ) -> MarketTradesSnapshot {
        let mut state = MarketTradesState::new(trades.len(), precision());
        state.merge(trades.iter().map(|&(id, timestamp, price)| ListingTrade {
            listing: listing.clone(),
            id: MarketTradeId::Numeric(id),
            price: market_price(price),
            quantity: traded_quantity(2),
            side: Side::Buy,
            timestamp,
        }));
        state.snapshot()
    }

    fn snapshot(timestamp: UnixMillis) -> MarketTradesSnapshot {
        snapshot_of_listing(listing(), &[(1, timestamp, 100)])
    }

    fn empty_snapshot() -> MarketTradesSnapshot {
        MarketTradesState::new(1, precision()).snapshot()
    }

    /// Creates a widget the way the framework does: state first, then one load.
    fn widget_with(
        config: MarketTradesConfig,
        cx: &mut TestAppContext,
    ) -> Entity<MarketTradesWidget> {
        let widget = tape(
            config,
            PendingMarketTradesSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            cx,
        );
        widget.update(cx, |widget, cx| widget.load(cx));
        widget
    }

    /// A widget the framework created before anything was chosen.
    fn widget(cx: &mut TestAppContext) -> Entity<MarketTradesWidget> {
        widget_with(MarketTradesConfig::default(), cx)
    }

    /// A widget that already follows one listing, so stream messages belong to
    /// its scope.
    fn scoped_widget(cx: &mut TestAppContext) -> Entity<MarketTradesWidget> {
        widget_with(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            cx,
        )
    }

    fn status(widget: &Entity<MarketTradesWidget>, cx: &App) -> WidgetState {
        let status = widget.read(cx).status.clone();
        status.state(cx)
    }

    /// The same read, for the places where the widget is already borrowed.
    fn reported(widget: &MarketTradesWidget, cx: &App) -> WidgetState {
        widget.status.state(cx)
    }

    /// Reports a ready snapshot as the stream would, then returns the widget.
    fn accept_snapshot(
        widget: &Entity<MarketTradesWidget>,
        snapshot: MarketTradesSnapshot,
        cx: &mut App,
    ) {
        widget.update(cx, |widget, context| {
            widget.binding.cancel();
            widget.ready_received = true;
            widget.apply_snapshot(snapshot, context);
        });
    }

    #[gpui_kit::test]
    fn a_widget_without_a_listing_should_ask_for_one(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            let status = status(&widget, cx);
            assert!(
                !status.is_busy(),
                "a pane asking for a listing is not working"
            );
            assert!(status.failure().is_none());
            assert_eq!(
                status.notice(),
                Some(&Notice::message(
                    "widget.market_trades.select_instrument_prompt"
                ))
            );
        });
    }

    /// No market that could answer with trades is loaded and nothing is chosen: there is nothing
    /// to read *and* nothing to choose from, so the pane waits for the market participant's metadata rather
    /// than asking for a listing its picker could not offer (AGENTS.md §8).
    #[gpui_kit::test]
    fn a_tape_with_nothing_chosen_and_no_market_loaded_should_wait_for_one(
        cx: &mut TestAppContext,
    ) {
        let widget = tape_market(
            cx,
            MarketTradesConfig::default(),
            PendingMarketTradesSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        widget.update(cx, |widget, cx| widget.load(cx));

        cx.update(|cx| {
            assert!(
                !widget.read(cx).has_content(),
                "there are no trades to read"
            );
            assert!(
                widget.read(cx).awaiting_market,
                "and no market to read them from either"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: MarketTradesWidget::listed_venues(),
                    supported: MarketTradesWidget::listed_venues(),
                }),
                "a tape nobody could choose from waits for the market participant's metadata"
            );
        });
    }

    /// The commit of a market the tape was waiting for is what takes it off that notice: a market
    /// with nothing chosen is the prompt's own business, and the pane says so without being
    /// reopened (a tape waiting with nothing chosen reads nothing, so only the wait itself can
    /// tell that it is over).
    #[gpui_kit::test]
    fn loading_a_market_should_turn_the_wait_back_into_the_prompt(cx: &mut TestAppContext) {
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let widget = tape_market(
            cx,
            MarketTradesConfig::default(),
            PendingMarketTradesSource,
            Fixture::new(entries.clone()),
            WidgetConfigChange::default(),
            false,
        );
        widget.update(cx, |widget, cx| widget.load(cx));

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the tape starts out waiting"
            );
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            test_support::publish_fixture_catalogs(cx, &[(route(), entries)]);
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the tape left the wait on the commit alone"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.market_trades.select_instrument_prompt"
                )),
                "a market the user can choose from asks for a listing"
            );
        });
    }

    /// A tape opened with nothing chosen reads nothing, and the listing the user then picks is
    /// what moves what it reads — not a venue commit. A commit of another venue therefore cannot
    /// move the tape's scope, so the pane leaves the stream it started and the trades it accepted
    /// exactly as they are rather than starting over what the user is watching.
    ///
    /// The commit is proved to have reached the pane by a probe subscribed after it did, and what
    /// proves the pane did not start over is its own directory generation beside the trades it
    /// was drawing and the status it reported for them.
    #[gpui_kit::test]
    fn choosing_a_listing_should_leave_the_tape_alone_on_another_venues_commit(
        cx: &mut TestAppContext,
    ) {
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let source = AnsweringMarketTradesSource::default();
        let widget = tape(
            MarketTradesConfig::default(),
            source.clone(),
            Fixture::new(entries),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        // The commits the fixture itself made — the venue loading, and the catalog publication —
        // are drained before anything is chosen: what this test is about is the commit below, and
        // one still in flight would be handled as a commit of a market the tape had only just
        // started following.
        cx.run_until_parked();
        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        let delivered = deliveries.load(Ordering::Acquire);

        cx.update(|cx| {
            assert!(
                widget.read(cx).config.selection.is_none(),
                "the pane is opened with nothing chosen and nothing to read"
            );
            assert!(
                widget.update(cx, |widget, context| widget
                    .select_selection(selection(listing()), context)),
                "the picker offers the listing the loaded venue publishes"
            );
        });
        settle_until(cx, "the tape to read the listing the user chose", |_| {
            !source.subscribed().is_empty()
        });
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        // The tape is reading: the trades it accepted are the whole of what it draws.
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            assert!(widget.read(cx).has_content(), "the tape is reading trades");
        });

        // Another venue commits: nothing about this tape's scope moved, so the read it is in the
        // middle of is left exactly as it is.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::Okx,
                    ]),
                    &[],
                    &[],
                )
                .expect("another venue loads");
        });
        settle_until(
            cx,
            "the other venue's commit to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) > delivered,
        );

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).directory_generation,
                generation,
                "a commit of a venue this tape does not follow must not start it over"
            );
            assert!(
                widget.read(cx).snapshot.is_some(),
                "the trades the user is reading must not be dropped"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "and the pane must not go back to connecting"
            );
        });
    }

    /// A refresh of the venue the tape reads replaces the catalogue generation its live stream
    /// resolved the listing's increments against, while the stream keeps what it resolved when it
    /// was opened. So the tape starts over — the trades go, because nothing on screen can be
    /// vouched for — and the request it sends is the one that opens a new stream, at the generation
    /// the load published.
    ///
    /// What proves both halves is the source's own record: the subscription it was asked for at the
    /// moment the service opened it, and the increments that listing was published at then.
    #[gpui_kit::test]
    fn a_refresh_of_the_tapes_own_venue_should_reopen_its_stream(cx: &mut TestAppContext) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let directory = Fixture::new(vec![listing_entry("1")]);
        let source = ResolvingMarketTradesSource {
            catalog: Arc::clone(directory.catalog_arc()),
            resolutions: Arc::clone(&resolutions),
        };
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            source,
            directory,
            WidgetConfigChange::default(),
            cx,
        );
        settle_until(cx, "the tape's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            assert!(widget.read(cx).has_content(), "the tape is reading trades");

            // The same venue loads again and the catalogue it republishes restates the listing's
            // price tick, which is the shape a production load has: it re-fetches the catalogue.
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the tape's venue loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![listing_entry("0.01")])]);
        });
        settle_until(cx, "the tape to open a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).directory_generation,
                generation + 1,
                "a refresh of the venue the tape reads starts it over"
            );
            assert!(
                !widget.read(cx).has_content(),
                "and the trades are gone: they were written at the generation that was replaced"
            );
            assert_eq!(
                resolutions_of(&resolutions)[1].price_tick.to_string(),
                "0.01",
                "the stream that was opened resolved at the generation this load published"
            );
        });
    }

    /// The other half of that rule: a scope loaded *after* the tape started reading is a scope the
    /// tape's picker offers. The control is built from the loaded venues on the frame that builds
    /// it, so a commit the pane does not act on still widens what the user may select — a reading
    /// kept from the last load would leave the venue the user just loaded out of the picker
    /// altogether, neither its column nor its catalog.
    ///
    /// The commit is proved to have reached the pane by a probe subscribed after it did, and what
    /// proves the freshness does not ride on starting over is the pane's own directory generation
    /// beside the trades it was drawing.
    #[gpui_kit::test]
    fn a_venue_loaded_after_the_tape_started_reading_should_be_offered_by_its_picker(
        cx: &mut TestAppContext,
    ) {
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let source = AnsweringMarketTradesSource::default();
        let widget = tape(
            MarketTradesConfig::default(),
            source.clone(),
            Fixture::new(entries),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                widget.update(cx, |widget, context| widget
                    .select_selection(selection(listing()), context)),
                "the picker offers the listing the loaded venue publishes"
            );
        });
        settle_until(cx, "the tape to read the listing the user chose", |_| {
            !source.subscribed().is_empty()
        });

        // The commits the fixture itself made — the venue loading, and the catalog publication —
        // are drained before the probe is subscribed: what this test is about is the commit below.
        cx.run_until_parked();
        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        let delivered = deliveries.load(Ordering::Acquire);
        let generation = cx.read(|cx| widget.read(cx).directory_generation);
        // The premise the assertion below turns on: the venue is not one the picker offers yet,
        // so what widens it is this commit and not a route the fixture had already loaded.
        cx.update(|cx| {
            assert!(
                !offers_route(
                    &widget.read(cx).instrument_filters(cx).routes,
                    &okx_spot_route(),
                    CapabilityId::MarketTrades
                ),
                "the venue is not loaded before the commit below"
            );
        });

        // The tape is reading: the trades it accepted are the whole of what it draws.
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            assert!(widget.read(cx).has_content(), "the tape is reading trades");
        });

        // Another venue commits: nothing about this tape's scope moved, so the read it is in the
        // middle of is left exactly as it is.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::Okx,
                    ]),
                    &[],
                    &[],
                )
                .expect("another venue loads");
        });
        settle_until(
            cx,
            "the other venue's commit to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) > delivered,
        );

        cx.update(|cx| {
            let filters = widget.read(cx).instrument_filters(cx);
            assert!(
                offers_route(
                    &filters.routes,
                    &okx_spot_route(),
                    CapabilityId::MarketTrades
                ),
                "a scope loaded after the tape started reading is one its picker offers"
            );
            assert_eq!(
                widget.read(cx).directory_generation,
                generation,
                "and the freshness does not ride on starting the tape over"
            );
            assert!(
                widget.read(cx).snapshot.is_some(),
                "the trades the user is reading are still there"
            );
        });
    }

    /// The destructive half of that same reading: a listing whose route is in memory but not in
    /// the reading this tape took when it last loaded, settled by the pane's own entry point —
    /// the gate that judges the selection has to answer with the venues as they are now. Judged
    /// by that reading instead, the route of a listing the pane holds is taken for a route that
    /// is no longer loaded and the selection is withdrawn: `config.selection` goes back to
    /// nothing, the change is published, and the pane reports "select an instrument" over a route
    /// the venue does publish.
    ///
    /// This is the tape's own entry point, which a click in the picker reaches: a dialog stays open
    /// across commits and follows them, so a commit that adds the venue while the dialog is in
    /// front of the user puts that venue's route in front of them while the pane's reading stays as
    /// the last load took it, and `confirmed_selection` passes the route — it judges by the
    /// selector's own filters, which the commit refreshed, so the route is in memory and the
    /// listing is offered at all. The test calls `select_selection` directly because that is the
    /// branch, not because a click could not reach it.
    ///
    /// What is asserted is that the choice survives and is followed, rather than the absence of a
    /// panic: the config still holds it, the status is not the prompt a tape with nothing chosen
    /// reports, and the tape really subscribed to it.
    #[gpui_kit::test]
    fn a_listing_chosen_on_a_venue_loaded_later_should_not_be_withdrawn(cx: &mut TestAppContext) {
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let source = AnsweringMarketTradesSource::default();
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            source.clone(),
            Fixture::new(entries),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(cx, "the tape to read the listing it was built over", |_| {
            !source.subscribed().is_empty()
        });

        // The commits the fixture itself made are drained before the probe is subscribed: what this
        // test is about is the commit below.
        cx.run_until_parked();
        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        let delivered = deliveries.load(Ordering::Acquire);
        // The premise the choice below turns on: the venue the user will pick on is not one the
        // picker's reading holds at this moment — the commit below is what loads it.
        cx.update(|cx| {
            assert!(
                !offers_route(
                    &widget.read(cx).instrument_filters(cx).routes,
                    &okx_spot_route(),
                    CapabilityId::MarketTrades
                ),
                "the venue is not loaded before the commit below"
            );
        });

        // Another venue loads, publishing the catalog the listing the user will pick is in.
        let chosen = test_support::listing_key(&okx_spot_route(), "BTCUSDT");
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::Okx,
                    ]),
                    &[],
                    &[],
                )
                .expect("another venue loads");
            test_support::publish_fixture_catalogs(
                cx,
                &[(
                    okx_spot_route(),
                    vec![test_support::entry(
                        &okx_spot_route(),
                        test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                        "BTCUSDT",
                    )],
                )],
            );
        });
        settle_until(
            cx,
            "the other venue's commit to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) > delivered,
        );
        assert!(
            matches!(
                cx.update(|cx| cx
                    .global::<MarketParticipantsConnector>()
                    .catalog()
                    .contains(&chosen)),
                Ok(true)
            ),
            "the listing the user picks is one the newly loaded route really publishes, so what \
             withdraws it is the reading and not the directory"
        );

        // The user picks the listing the newly loaded venue publishes, through the pane's own entry
        // point: the choice is theirs, and the route it is on is in memory.
        cx.update(|cx| {
            assert!(
                widget.update(cx, |widget, context| widget
                    .select_selection(selection(chosen.clone()), context)),
                "the pane accepts a listing of a route that is in memory"
            );
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(chosen.clone())),
                "a choice made on a route that is loaded now must not be withdrawn as one that \
                 is not"
            );
            assert_ne!(
                status(&widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.market_trades.select_instrument_prompt"
                )),
                "the pane must not report the prompt a tape with nothing chosen reports"
            );
        });
        settle_until(cx, "the tape to follow the listing the user chose", |_| {
            source.subscribed().contains(&chosen)
        });
        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(chosen)),
                "the tape settles the choice against the venues as it finds them"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "and the pane stays on the market it is reading"
            );
        });
    }

    /// The other direction of the same rule: a commit that takes the tape's own market away is a
    /// market it can no longer read, so the pane goes back to waiting for it — without being
    /// reopened, and without losing the choice the user made.
    ///
    /// A pane that is waiting with nothing chosen is not the case here: the tape was reading, and
    /// the commit is what moves its scope out of memory.
    #[gpui_kit::test]
    fn a_commit_that_takes_the_tapes_own_market_away_should_send_it_back_to_waiting(
        cx: &mut TestAppContext,
    ) {
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::new(entries),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the tape is reading the market it was built over"
            );
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            assert!(widget.read(cx).has_content(), "the tape is reading trades");
        });

        // The venue loads again and publishes no scope at all, which is what a shrinking commit
        // does: the market behind the tape is not in memory any more.
        cx.update(|cx| {
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the tape waits for the market it lost"
            );
            assert!(
                !widget.read(cx).has_content(),
                "and the trades it was drawing are not backed by a market any more"
            );
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(listing())),
                "a market that went away does not unmake the user's choice"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: MarketTradesWidget::listed_venues(),
                }),
                "waiting is not working"
            );
        });
    }

    /// A failure is terminal: a venue commit does not revive a failed tape, even when the
    /// commit takes the market its trades came from away and puts it back.
    ///
    /// The first commit is the one a tape without the terminal guard would act on: it takes the
    /// tape's own market away, so an unguarded handler would replace the failure with the neutral
    /// notice. The second puts the market back, which a handler that judged "did what I read
    /// change" without the guard would read as a scope that appeared and would start over for.
    #[gpui_kit::test]
    fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::new(entries),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        let failure = cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });

            let state = status(&widget, cx);
            assert!(state.is_failed(), "the tape is in its terminal failure");
            state
        });
        let venue = market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
            .expect("the fixture venue is canonical");

        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        // The market the tape follows leaves the loaded venues...
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        settle_until(cx, "the shrinking commit to reach its subscribers", |_| {
            deliveries.load(Ordering::Acquire) >= 1
        });
        // ...and comes back.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the venue loads again");
        });
        settle_until(
            cx,
            "the commit that puts the market back to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) >= 2,
        );

        cx.update(|cx| {
            assert_eq!(
                status(&widget, cx),
                failure,
                "a venue commit must not revive a failed tape"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "a tape in its terminal failure is not a tape waiting for a market"
            );
        });
    }

    #[gpui_kit::test]
    fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
        });
    }

    #[gpui_kit::test]
    fn an_accepted_snapshot_should_become_content_and_clear_the_status(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);

            assert!(widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Ready);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    /// A successful connection is enough to show the tape. An empty snapshot is
    /// content — an empty table — and the widget reports nothing about it: the table
    /// is what says no trade has printed.
    #[gpui_kit::test]
    fn an_empty_snapshot_should_be_content_and_report_nothing(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, empty_snapshot(), cx);

            assert!(widget.read(cx).has_content());
            let table = widget.read(cx).table.clone();
            assert_eq!(table.read(cx).rows_count(), 0);
            assert!(
                TableDataSet::<COLUMN_COUNT>::empty(table.read(cx), cx).is_some(),
                "an empty tape has to say it is empty rather than leave its body blank"
            );
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Ready);
            assert!(status.failure().is_none());
            assert!(
                status.notice().is_none(),
                "the empty state belongs to the table, not to the framework's placeholder"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_with_content_should_be_reported_and_keep_the_content(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });

            assert!(
                widget.read(cx).has_content(),
                "a failure must not throw away data the widget already has"
            );
            let status = status(&widget, cx);
            assert!(status.is_failed());
            assert_eq!(
                status.failure().map(|failure| failure.text(cx)),
                Some(disconnect().localized_message(cx))
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_without_content_should_be_reported_with_no_content(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });

            assert!(!widget.read(cx).has_content());
            assert!(status(&widget, cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn every_failure_should_offer_the_recovery_that_reloads_the_widget(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });

            let recovery = status(&widget, cx)
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failure should offer a way out");
            assert_eq!(recovery.label(cx).as_ref(), "Retry");
            assert!(recovery.is_enabled());

            recovery.run(cx);

            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    #[gpui_kit::test]
    fn a_late_stream_message_after_a_failure_should_not_change_the_status(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let key = MarketTradesStreamKey::new(listing());

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
                let failed = reported(widget, context);

                widget.apply_reliable(
                    key.clone(),
                    Arc::new(MarketTradesStreamMessage::Status(MarketTradesStatus::Ready)),
                    context,
                );
                widget.apply_latest(
                    key,
                    Arc::new(MarketTradesStreamMessage::Snapshot(snapshot(1))),
                    context,
                );

                assert_eq!(reported(widget, context), failed);
                assert!(reported(widget, context).is_failed());
                assert!(widget.snapshot.is_none());
            });
        });
    }

    /// Both streams are keyed by the listing they were opened for, and a message from
    /// a scope the widget has left says nothing about what it is showing now.
    #[gpui_kit::test]
    fn a_message_from_another_scope_should_be_ignored(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                widget.ready_received = true;
                let other = MarketTradesStreamKey::new(other_listing());

                widget.apply_latest(
                    other,
                    Arc::new(MarketTradesStreamMessage::Snapshot(snapshot(1))),
                    context,
                );

                assert!(widget.snapshot.is_none());
                assert!(widget.pending_snapshot.is_none());
            });
        });
    }

    /// Two listings of one instrument are two scopes: the config keeps the one the
    /// user picked, the stream key names it, and the other's messages are not its
    /// own.
    #[gpui_kit::test]
    fn two_listings_of_one_instrument_should_stay_independent(cx: &mut TestAppContext) {
        let instrument = test_support::spot_token(test_support::VENUE, "BTC", "USDT");
        let first = test_support::entry(&route(), instrument.clone(), "BTCUSDT");
        let second = test_support::entry(&route(), instrument, "BTCUSDT-PERP");
        assert_eq!(
            first.instrument.id(),
            second.instrument.id(),
            "the fixture is one instrument listed twice"
        );
        assert_ne!(first.listing.key, second.listing.key);
        let widget = widget_with(
            MarketTradesConfig {
                selection: Some(selection(first.listing.key.clone())),
            },
            cx,
        );

        cx.update(|cx| {
            let widget = widget.read(cx);
            assert_eq!(
                widget.config.selection,
                Some(selection(first.listing.key.clone()))
            );
            assert_eq!(
                widget.current_key(),
                Some(MarketTradesStreamKey::new(first.listing.key.clone())),
                "the stream key names the exact listing, not the instrument"
            );
            assert_ne!(
                widget.current_key(),
                Some(MarketTradesStreamKey::new(second.listing.key.clone()))
            );
        });
    }

    #[gpui_kit::test]
    fn a_snapshot_before_the_ready_boundary_should_await_it(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let key = MarketTradesStreamKey::new(listing());

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                widget.apply_latest(
                    key.clone(),
                    Arc::new(MarketTradesStreamMessage::Snapshot(snapshot(1))),
                    context,
                );
                assert!(widget.snapshot.is_none());
                assert!(!widget.has_content());

                widget.apply_reliable(
                    key,
                    Arc::new(MarketTradesStreamMessage::Status(MarketTradesStatus::Ready)),
                    context,
                );

                assert_eq!(
                    widget.snapshot.as_ref().map(MarketTradesSnapshot::len),
                    Some(1)
                );
                assert!(widget.has_content());
                assert_eq!(reported(widget, context), WidgetState::Ready);
            });
        });
    }

    #[gpui_kit::test]
    fn a_stream_failure_should_become_the_reported_state(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let key = MarketTradesStreamKey::new(listing());

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                widget.apply_reliable(
                    key,
                    Arc::new(MarketTradesStreamMessage::Status(
                        MarketTradesStatus::Error(disconnect()),
                    )),
                    context,
                );

                assert!(reported(widget, context).is_failed());
                assert!(!widget.has_content());
            });
        });
    }

    /// An explicit reload drops what it had and starts over from connecting.
    ///
    /// The distinction matters: the framework also calls `load`, and the two
    /// cases must not be confused. A widget that kept showing its old rows while
    /// it loaded again would be showing data it can no longer vouch for.
    #[gpui_kit::test]
    fn an_explicit_reload_should_start_over_from_connecting(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);
            assert!(widget.read(cx).has_content());

            widget.update(cx, |widget, context| widget.load(context));

            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    #[gpui_kit::test]
    fn a_retry_after_a_failure_should_keep_the_snapshot_the_widget_still_shows(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });

            let recovery = status(&widget, cx)
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failure should offer a way out");
            recovery.run(cx);

            assert_eq!(
                widget
                    .read(cx)
                    .snapshot
                    .as_ref()
                    .map(MarketTradesSnapshot::len),
                Some(1),
                "a retry must not blank data the user is still reading"
            );
        });
    }

    #[gpui_kit::test]
    fn an_accepted_snapshot_should_reach_the_table_region(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            assert_eq!(widget.read(cx).table.read(cx).rows_count(), 0);

            accept_snapshot(
                &widget,
                snapshot_of_listing(listing(), &[(1, 1, 100), (2, 2, 101)]),
                cx,
            );
            assert_eq!(widget.read(cx).table.read(cx).rows_count(), 2);

            widget.update(cx, |widget, context| widget.clear_trades(context));
            assert_eq!(widget.read(cx).table.read(cx).rows_count(), 0);
        });
    }

    #[gpui_kit::test]
    fn choosing_a_listing_should_start_following_it(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();

                assert!(widget.select_selection(selection(listing()), context));
            });
            assert_eq!(widget.read(cx).config.selection, Some(selection(listing())));
            assert_eq!(status(&widget, cx), WidgetState::Connecting);

            widget.update(cx, |widget, context| {
                // Selecting the same scope again is not a new transition.
                assert!(!widget.select_selection(selection(listing()), context));
                assert!(widget.select_selection(selection(other_listing()), context));
            });

            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(other_listing()))
            );
            assert_eq!(
                widget.read(cx).current_key(),
                Some(MarketTradesStreamKey::new(other_listing()))
            );
        });
    }

    /// A directory whose fixture route has published the given entries.
    ///
    /// The publication is what a display read answers from, so a test that asserts a
    /// display name has to load the route first: a fixture that was only constructed
    /// has published nothing yet.
    fn published_directory(
        cx: &mut App,
        entries: Vec<market_participants_connector::catalog::batch::CatalogEntry>,
    ) -> Fixture {
        global::install(cx).expect("test Tokio runtime should build");
        let directory = Fixture::new(entries);
        global::handle(cx)
            .handle()
            .block_on(directory.ensure())
            .expect("the fixture spot route should publish its catalog");
        directory
    }

    /// The column headers name the units the trades are measured in, which is what
    /// makes a price column readable on its own. The names are the display codes the
    /// published generation decided on, not the venue's own symbols.
    #[gpui_kit::test]
    fn the_headers_should_name_the_units_of_the_followed_listing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let headers = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            let directory = published_directory(
                cx,
                vec![test_support::entry(
                    &route(),
                    test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                    "BTCUSDT",
                )],
            );
            market_participants_connector::test_support::install_root(
                cx,
                Arc::clone(directory.catalog_arc()),
            );
            let snapshot = snapshot_of_listing(listing(), &[(1, 1, 100)]);
            let table = MarketTradesTable {
                units: MarketTradeUnits::of(&snapshot, cx),
                snapshot: Some(snapshot),
                buy: gpui_kit::green(),
                sell: gpui_kit::red(),
            };
            table.headers(cx)
        });

        assert_eq!(
            headers.map(|header| header.as_ref().to_owned()),
            [
                "Price (USDT)".to_owned(),
                "Amount (BTC)".to_owned(),
                "Time".to_owned()
            ]
        );
    }

    /// An asset the published generation does not name is the localized unknown: a
    /// reader cannot use the canonical identity, and a name invented here would
    /// disagree with every other pane about the same asset.
    ///
    /// The catalog names every asset a listed market references, so the asset
    /// without a name is the one *this* publication never registered — a currency the
    /// fixture vocabulary does not seed. The generation is the fixture's own,
    /// captured explicitly, so the assertion names the publication it is about rather
    /// than whichever connector happens to be installed globally.
    #[gpui_kit::test]
    fn an_unpublished_asset_should_be_named_as_unknown(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let label = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            let directory = Fixture::new(Vec::new());
            let display = directory.catalog().cached_display();
            // A currency the fixture vocabulary never registers. The code is a valid
            // ISO 4217 code, so the only reason it has no name here is that this
            // generation does not carry the asset.
            let absent = AssetId::Currency(CurrencyId::of_terms(&CurrencyTerms {
                code: Iso4217Code::new("CHF").expect("CHF is canonical"),
            }));

            assert!(
                display.resolver().asset_code(&absent).is_err(),
                "the fixture generation does not register {absent}"
            );
            asset_display(&display, &absent, cx)
        });

        assert_eq!(label.as_ref(), "Unknown asset");
    }

    /// A quantity counted in contracts is labelled as the unit it actually carries,
    /// not as base volume.
    #[gpui_kit::test]
    fn the_amount_header_should_name_the_unit_the_trades_carry(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let headers = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            let directory = published_directory(
                cx,
                vec![test_support::entry(
                    &route(),
                    test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                    "BTCUSDT",
                )],
            );
            market_participants_connector::test_support::install_root(
                cx,
                Arc::clone(directory.catalog_arc()),
            );
            let mut trade = trade(1, 1, 100);
            trade.quantity = MarketQuantity {
                value: Quantity::from(3_u64),
                unit: QuantityUnit::Contracts,
            };
            let mut state = MarketTradesState::new(1, precision());
            state.merge([trade]);
            let snapshot = state.snapshot();
            let table = MarketTradesTable {
                units: MarketTradeUnits::of(&snapshot, cx),
                snapshot: Some(snapshot),
                buy: gpui_kit::green(),
                sell: gpui_kit::red(),
            };
            table.headers(cx).map(|header| header.as_ref().to_owned())
        });

        // The contracts label is the unit's own, not the base asset the tape would
        // have named if it assumed every listing is measured in it.
        assert_eq!(
            headers,
            [
                "Price (USDT)".to_owned(),
                "Amount (Contracts)".to_owned(),
                "Time".to_owned()
            ]
        );
    }

    #[test]
    fn market_trade_display_should_fill_every_column() {
        let table = MarketTradesTable {
            snapshot: Some(snapshot(1)),
            units: None,
            buy: gpui_kit::green(),
            sell: gpui_kit::red(),
        };
        let display = table.snapshot.as_ref().expect("snapshot").rows()[0].display();

        // The numbers keep the increments of the listing the tape follows: a whole
        // price is still read with the decimals its tick states.
        assert_eq!(
            [
                display.price.as_ref(),
                display.quantity.as_ref(),
                display.time.as_ref(),
            ],
            ["100.00", "2.000", "00:00:00.001"]
        );
        assert_eq!(table.rows_count(), 1);
    }

    /// The scope is the one thing this widget persists, so it has to survive the
    /// config envelope the framework stores it in.
    #[test]
    fn the_selected_listing_should_survive_a_config_round_trip() {
        let config = MarketTradesConfig {
            selection: Some(selection(listing())),
        };

        let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let decoded = stored.decode::<MarketTradesConfig>();

        assert_eq!(decoded.selection, config.selection);
        // A tape created without a scope decodes with none either: it asks for a
        // listing rather than inventing one.
        assert!(MarketTradesConfig::default().selection.is_none());
    }

    /// The previous build persisted a route and an instrument; the listing key is what
    /// this build reads, so that payload is not a config it can decode. It is replaced
    /// by the default one, which the pane then writes back.
    #[test]
    fn an_old_payload_that_names_a_route_and_an_instrument_should_be_reset() {
        let stored: VersionedWidgetConfig = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "payload": concat!(
                r#"{"selection":{"route":{"venue":"binance","scope":"spot"},"#,
                r#""instrument":{"symbol":"BTCUSDT"}}}"#,
            ),
        }))
        .expect("the stored envelope should decode");

        let decoded = stored.decode::<MarketTradesConfig>();

        assert!(
            decoded.selection.is_none(),
            "a selection without a listing key is not a listing this build can follow"
        );
    }

    /// A stored listing the route no longer lists goes back to "nothing chosen", and
    /// the pane is told so it can be written back with the tape it is actually showing.
    #[gpui_kit::test]
    fn a_listing_the_route_no_longer_offers_should_go_back_to_nothing_chosen(
        cx: &mut TestAppContext,
    ) {
        let published = Rc::new(Cell::new(0));
        let widget = widgets(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            published.clone(),
        );

        // The route answers without the listing the config asked for.
        widget.update(cx, |widget, cx| {
            widget.settle_selection(selection(listing()), ListingSupport::Withdrawn, cx);
        });

        cx.update(|cx| {
            assert_eq!(widget.read(cx).config.selection, None);
            assert_eq!(
                status(&widget, cx).notice(),
                Some(&Notice::message(
                    "widget.market_trades.select_instrument_prompt"
                ))
            );
        });
        assert_eq!(
            published.get(),
            1,
            "the replacement is reported, or the next start resets it again"
        );
    }

    /// A lookup that failed proves nothing, so the selection is left alone and the
    /// subscription reports its own error.
    #[gpui_kit::test]
    fn a_lookup_that_failed_should_leave_the_selection_alone(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let widget = widgets(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            published.clone(),
        );

        cx.update(|cx| {
            let support = listing_support(&listing(), Err(no_answer()), cx);
            assert_eq!(support, ListingSupport::Unknown);
            widget.update(cx, |widget, cx| {
                widget.settle_selection(selection(listing()), support, cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(listing())),
                "the venue not answering is not the venue saying no"
            );
        });
        assert_eq!(published.get(), 0);
    }

    /// A choice the user made while the lookup was in flight is theirs, not the
    /// lookup's to overwrite.
    #[gpui_kit::test]
    fn a_choice_made_while_the_lookup_was_in_flight_should_survive_it(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let widget = widgets(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            published.clone(),
        );

        widget.update(cx, |widget, cx| {
            widget.config.selection = Some(selection(other_listing()));
            // The answer is about the listing the user has already moved away from.
            widget.settle_selection(selection(listing()), ListingSupport::Withdrawn, cx);
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(other_listing())),
                "a choice the user made is not the lookup's to overwrite"
            );
        });
        assert_eq!(published.get(), 0);
    }

    /// A market the user has not loaded is a wait rather than a failure (⑮): the
    /// pane names what to load and keeps the listing it was given, because a route
    /// nobody holds is not a route that dropped it.
    #[gpui_kit::test]
    fn a_listing_of_an_unloaded_market_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let published = published.clone();
            move |_| published.set(published.get() + 1)
        });
        let widget = tape_market(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::unanswering(),
            change,
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(listing())),
                "a market that is not loaded does not unmake the user's choice"
            );
            let status = status(&widget, cx);
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![listing().route.venue],
                    supported: MarketTradesWidget::listed_venues(),
                }),
                "the pane waits"
            );
            assert!(
                status.failure().is_none(),
                "waiting for a market is not a failure"
            );
            let notice = status.notice().expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );
        });
        assert_eq!(published.get(), 0, "waiting changes no configuration");
    }

    /// Loading the venue is what a waiting pane recovers from, by itself and
    /// without being reopened (⑯).
    #[gpui_kit::test]
    fn loading_the_venue_should_let_the_waiting_pane_subscribe(cx: &mut TestAppContext) {
        let source = AnsweringMarketTradesSource::default();
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let widget = tape_market(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            source.clone(),
            Fixture::new(entries.clone()),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                status(&widget, cx).notice().is_some(),
                "the pane waits for a market nobody loaded"
            );

            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            test_support::publish_fixture_catalogs(cx, &[(route(), entries)]);
        });
        // The directory signals its publications through its own task, so the event
        // that re-runs the load arrives on the executor.
        cx.run_until_parked();
        settle_until(cx, "the tape to subscribe", |_| {
            !source.subscribed().is_empty()
        });

        cx.update(|cx| {
            let status = status(&widget, cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert!(status.failure().is_none(), "recovery is not a failure");
        });
    }

    /// A market that lists nothing is a market whose listing is gone: the stored
    /// listing goes back to "nothing chosen" and the pane is told.
    #[gpui_kit::test]
    fn a_listing_of_a_market_that_lists_nothing_should_go_back_to_nothing_chosen(
        cx: &mut TestAppContext,
    ) {
        let published = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let published = published.clone();
            move |_| published.set(published.get() + 1)
        });
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::unanswering(),
            change,
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(cx, "the empty market to settle the listing", |cx| {
            widget.read(cx).config.selection.is_none()
        });

        cx.update(|cx| {
            assert!(
                status(&widget, cx).failure().is_none(),
                "a market that lists nothing is not a failure: nothing is chosen"
            );
        });
        assert_eq!(published.get(), 1);
    }

    /// A market revoked after the pane was built is a market that is not in memory any
    /// more, so the pane waits for the user to load it again instead of clearing
    /// what they chose.
    #[gpui_kit::test]
    fn a_market_revoked_after_construction_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let published = published.clone();
            move |_| published.set(published.get() + 1)
        });
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::new(entries),
            change,
            cx,
        );
        // The pane starts over a market that is really there, which is what makes the
        // revocation below a change of state rather than the state it was built in.
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                status(&widget, cx).notice().is_none(),
                "the market the fixture loaded is the one the pane reads"
            );
        });

        cx.update(|cx| {
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("an empty scope set unloads the venue's slot");
            widget.update(cx, |widget, cx| widget.load(cx));
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(listing())),
                "a market that is gone does not unmake the user's choice"
            );
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "a market that is no longer registered is not a failure"
            );
            let notice = status.notice().expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );
        });
        assert_eq!(published.get(), 0, "waiting changes no configuration");
    }

    /// The other half of the same story: a pane built while the market was never there
    /// waits for it from the first frame, which is the state a fresh workspace opens in
    /// before the user has loaded anything.
    #[gpui_kit::test]
    fn a_market_never_loaded_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let published = published.clone();
            move |_| published.set(published.get() + 1)
        });
        let entries = vec![test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )];
        let widget = tape_market(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::new(entries),
            change,
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.selection,
                Some(selection(listing())),
                "a market nobody loaded does not unmake the user's choice"
            );
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "a market nobody loaded is not a failure"
            );
            let notice = status.notice().expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );
        });
        assert_eq!(published.get(), 0, "waiting changes no configuration");
    }

    /// The words a waiting pane shows are the ones the window's language resolves them in:
    /// the notice is a reference into the catalogue, resolved on the frame that draws it, so the
    /// pane reports nothing again — nothing loads again, and nothing re-reports by hand.
    #[gpui_kit::test]
    fn a_pane_waiting_for_its_market_should_follow_the_language(cx: &mut TestAppContext) {
        let widget = tape_market(
            cx,
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        let (notice, english) = cx.update(|cx| {
            let notice = status(&widget, cx)
                .notice()
                .cloned()
                .expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );

            (notice, sentence.to_string())
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("币安"),
                "the same reference resolves in the language showing now: {sentence}"
            );
            assert_ne!(
                sentence, english,
                "the copy of the language the interface left is gone: {sentence}"
            );
        });
    }

    /// A listing its venue has settled or delisted is one no stream can serve, so it is
    /// reset like a listing the route dropped.
    #[gpui_kit::test]
    fn a_delisted_listing_should_go_back_to_nothing_chosen(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let published = published.clone();
            move |_| published.set(published.get() + 1)
        });
        let directory = Fixture::new(vec![test_support::with_status(
            test_support::entry(
                &route(),
                test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                "BTCUSDT",
            ),
            ListingStatus::Delisted,
        )]);
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            PendingMarketTradesSource,
            directory,
            change,
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(cx, "the route to answer", |cx| {
            widget.read(cx).config.selection.is_none()
        });

        cx.update(|cx| {
            assert!(
                status(&widget, cx).failure().is_none(),
                "a delisted listing is not a failure: the tape has nothing chosen"
            );
            assert_eq!(
                status(&widget, cx).notice(),
                Some(&Notice::message(
                    "widget.market_trades.select_instrument_prompt"
                ))
            );
        });
        assert_eq!(published.get(), 1);
    }

    /// The route is asked what it offers before anything is subscribed to: a listing
    /// it no longer publishes is never followed, not followed and then dropped.
    #[gpui_kit::test]
    fn a_listing_the_route_dropped_should_never_be_subscribed_to(cx: &mut TestAppContext) {
        let source = AnsweringMarketTradesSource::default();
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            source.clone(),
            Fixture::new(vec![test_support::entry(
                &route(),
                test_support::spot_token(test_support::VENUE, "ETH", "USDT"),
                "ETHUSDT",
            )]),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(cx, "the route to answer", |cx| {
            widget.read(cx).config.selection.is_none()
        });

        cx.update(|cx| {
            assert!(
                status(&widget, cx).failure().is_none(),
                "a listing the route dropped is not a failure: the tape has nothing chosen"
            );
            assert!(!widget.read(cx).has_content());
            assert_eq!(
                status(&widget, cx).notice(),
                Some(&Notice::message(
                    "widget.market_trades.select_instrument_prompt"
                ))
            );
        });
        assert!(
            source.subscribed().is_empty(),
            "the tape subscribed to a listing the route no longer publishes"
        );
    }

    /// The listing the route still publishes is the one the tape goes and follows, which
    /// is what makes the settle a step before the subscription rather than instead of it.
    #[gpui_kit::test]
    fn a_listing_the_route_still_offers_should_be_subscribed_to(cx: &mut TestAppContext) {
        let source = AnsweringMarketTradesSource::default();
        let widget = tape(
            MarketTradesConfig {
                selection: Some(selection(listing())),
            },
            source.clone(),
            Fixture::new(vec![test_support::entry(
                &route(),
                test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
                "BTCUSDT",
            )]),
            WidgetConfigChange::default(),
            cx,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(cx, "the tape to subscribe", |_| {
            !source.subscribed().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(widget.read(cx).config.selection, Some(selection(listing())));
        });
        assert_eq!(source.subscribed(), vec![listing()]);
    }
}
