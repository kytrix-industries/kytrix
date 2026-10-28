use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    empty_state::EmptyState,
    style::theme::{Theme, ThemeExt},
    table::{Table, TableDataSet},
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Empty, Entity, Hsla, InteractiveElement,
    IntoElement, Length, ParentElement, Render, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, base::Align, div, relative,
};
use i18n::t;
use market_participants_connector::{
    catalog::store::Catalog, market::error::MarketError, venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, market::units::PositionSide,
};
use serde::{Deserialize, Serialize};
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::is_catalog_not_loaded,
    identity::{WidgetId, WidgetInstanceId},
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::{data_time_zone, on_data_time_zone_change},
    venue::{VenueDisplay, route_display_string},
    view::WidgetView,
};
use widget_tokio_runtime::global;

use super::{
    model::{ForceOrderSnapshot, MAX_FORCE_ORDERS},
    service::{
        ForceOrderProtocol, ForceOrderRequest, ForceOrderResponse, ForceOrderService,
        ForceOrderSource, ForceOrderStatus, ForceOrderStreamKey, ForceOrderStreamMessage,
    },
};

const COLUMN_COUNT: usize = 5;
/// Fixed column widths in header order.
///
/// The shared table renders every column at its declared width, so the
/// instrument column is widened instead of taking the remaining space.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.15)),
    Length::Definite(relative(0.35)),
    Length::Definite(relative(0.175)),
    Length::Definite(relative(0.175)),
    Length::Definite(relative(0.15)),
];

/// Force orders expose no persisted options yet.
///
/// The config exists so the persisted schema version is owned by the widget
/// rather than inferred from the absence of a config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceOrderConfig;

impl WidgetConfig for ForceOrderConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The routes that report liquidations, as the loaded venues answer them now.
///
/// Which route reports liquidations is the loaded venues' own answer, and it is
/// re-read rather than remembered: the user may load a venue after this pane was
/// opened, and a scope may be withdrawn when its venue loads again.
fn needed_routes(cx: &App) -> Arc<[VenueRoute]> {
    let connector =
        cx.global::<market_participants_connector::connector::MarketParticipantsConnector>();

    loaded_routes_carrying(
        connector,
        market_participants_model::routing::CapabilityId::ForceOrder,
    )
    .into()
}

/// The loaded routes that carry one capability, in the order the venues answered them.
///
/// One question is asked of the loaded venues — which scopes carry the capability — so a pane
/// that reads a whole market never enumerates venues of its own. The route and the entry it is
/// judged by come from one reading of one commit: [`LoadedVenues::entries`] takes the venue
/// table's lock once, so a load landing while this runs cannot put one commit's route beside
/// another's capabilities.
fn loaded_routes_carrying(
    connector: &market_participants_connector::connector::MarketParticipantsConnector,
    capability: market_participants_model::routing::CapabilityId,
) -> Vec<VenueRoute> {
    connector
        .venues()
        .entries()
        .iter()
        .filter(|(_, entry)| {
            entry
                .capabilities
                .iter()
                .any(|support| support.capability == capability)
        })
        .map(|(route, _)| route.clone())
        .collect()
}

/// Whether one route's market is in memory: the scope is loaded.
///
/// Reading a market is only possible once the user has loaded it, and "not
/// loaded" is a wait rather than a failure.
fn market_ready(cx: &App, route: &VenueRoute) -> bool {
    cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
        .venues()
        .scope(route)
        .is_some()
}

/// Whether the routes the pane needs are a different set from the ones it holds.
///
/// The pane reads one stream over a *set* of routes, so a set that changed — every
/// scope a venue was loaded with, one a shrinking commit took away — is a different
/// stream rather than a repeat of the one being read. Compare as `(venue, scope)`
/// pairs: two sets that name the same markets in a different order are the same
/// stream.
fn routes_changed(held: &[VenueRoute], needed: &[VenueRoute]) -> bool {
    held.len() != needed.len()
        || !needed
            .iter()
            .all(|route| held.iter().any(|held| held == route))
}

/// The markets a notice names while nothing is loaded: the venues whose loader can
/// report liquidations, in the order supported_venues() answers.
///
/// This is the one static answer a pane may read: the capability's venue
/// name set, never a scope, an origin or a credential. It is what says which
/// markets could report liquidations at all, which is the only honest answer while
/// nothing is loaded.
///
/// What comes back is the venues themselves rather than their names: the notice is a
/// reference into the catalogue, and the sentence that names them is resolved by the
/// framework, in the language showing on the frame that draws it.
fn market_venues() -> Vec<VenueId> {
    market_participants_connector::venue::definition::venues_supporting(
        market_participants_model::routing::CapabilityId::ForceOrder,
    )
}

/// The force-order rows rendered by the body table.
///
/// The table is a separate entity because [`Table`] reads its rows from an
/// [`Entity`]; it keeps its own invalidation source, so a snapshot that changes
/// nothing the table draws does not rebuild it. It is the body's only expensive
/// region, so it is the region the body caches.
struct ForceOrderTable {
    snapshot: ForceOrderSnapshot,
    /// Direction colors, resolved once because `TableDataSet::cell` renders
    /// without a window or application context.
    long: Hsla,
    short: Hsla,
}

impl ForceOrderTable {
    fn new(theme: &Theme) -> Self {
        Self {
            snapshot: ForceOrderSnapshot::default(),
            long: theme.roles.market.up.hsla,
            short: theme.roles.market.down.hsla,
        }
    }
}

impl TableDataSet<COLUMN_COUNT> for ForceOrderTable {
    fn rows_count(&self) -> usize {
        self.snapshot.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        [
            t!(cx, "common.route"),
            t!(cx, "common.pair"),
            t!(cx, "common.price"),
            t!(cx, "common.amount"),
            t!(cx, "common.time"),
        ]
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        [
            Align::Start,
            Align::Start,
            Align::End,
            Align::End,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        // The newest order is displayed first, so display order is the reverse
        // of the chronological snapshot order.
        let row = row_index
            .checked_add(1)
            .and_then(|offset| self.snapshot.len().checked_sub(offset))
            .and_then(|index| self.snapshot.orders().get(index));
        let Some(row) = row else {
            return Empty.into_any_element();
        };
        let display = row.display(data_time_zone(cx));
        let direction = if row.order().position_side == PositionSide::Long {
            self.long
        } else {
            self.short
        };

        match column_index {
            0 => route_display_string(&row.order().listing.route, cx).into_any_element(),
            // The instrument column carries the direction colour because the
            // shared table cannot tint a whole row.
            1 => ForceOrderListingCell {
                listing: display.listing.clone(),
                color: direction,
            }
            .into_any_element(),
            2 => display.price.clone().into_any_element(),
            3 => display.quantity.clone().into_any_element(),
            4 => display.time.clone().into_any_element(),
            _ => Empty.into_any_element(),
        }
    }

    /// An accepted snapshot with no orders in it is the fact "there are none right
    /// now", so the table says so instead of leaving the body under its headers blank.
    fn empty(&self, cx: &App) -> Option<AnyElement> {
        Some(EmptyState::new("ForceOrderEmpty", t!(cx, "common.no_data")).into_any_element())
    }
}

/// One row's listing, as the generation the row was named by spelled it.
///
/// A generation that names no display for the listing is drawn as the localized
/// unknown rather than as the venue's symbol or as an identity digest: the name
/// belongs to the catalog, and a row that made one up would disagree with every
/// other pane about the same listing.
#[derive(IntoElement)]
struct ForceOrderListingCell {
    listing: super::model::ForceOrderListingLabel,
    color: Hsla,
}

impl RenderOnce for ForceOrderListingCell {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        div().text_color(self.color).child(
            self.listing
                .unwrap_or_else(|| t!(cx, "error.display.unknown_listing")),
        )
    }
}

pub struct ForceOrderWidget {
    binding: WidgetBinding<ForceOrderProtocol>,
    table: Entity<ForceOrderTable>,
    status: Entity<WidgetStatus>,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    /// The routes the stream the pane is running was started for.
    ///
    /// Which routes report liquidations is the registry's answer, and the user may
    /// load another venue at any time, so the set is re-read on every load
    /// rather than taken once. Empty means no market this pane reads is loaded,
    /// which is what keeps [`Self::has_content`] false while the pane waits.
    routes: Arc<[VenueRoute]>,
    snapshot: Option<ForceOrderSnapshot>,
    pending_snapshot: Option<ForceOrderSnapshot>,
    ready_received: bool,
    awaiting_snapshot: bool,
    /// Whether a venue this pane is reading was loaded or refreshed again.
    ///
    /// Such a commit replaces the catalog generation the live stream resolved its listings — their
    /// rules and displayed precision — against, while the stream itself keeps what it resolved when
    /// it was opened. So the pane has to start over, and the request it sends has to be the one that
    /// opens a new stream: [`Self::load`] reads this after its own gate — a pane a shrinking commit
    /// sent back to waiting keeps it, so the load that brings the market back reopens the stream too
    /// — and it is what makes the request a `Retry` rather than the idempotent `Load`, which the
    /// service answers without touching a stream for that key that is already live.
    market_reloaded: bool,
    /// Whether the pane is waiting for its markets rather than reading them.
    ///
    /// The pane keeps its stream while it waits — cancelling the binding is terminal,
    /// and this pane has to read the markets again once the user loads them — so a
    /// snapshot that was already in flight when the routes went away can still arrive.
    /// Accepting it would write a status over the notice the framework is drawing, so
    /// every message entry refuses while this is set.
    awaiting_market: bool,
    /// Wakes the pane when the zone its timestamps are read in moves.
    _time_zone: Option<Subscription>,
}

impl WidgetView for ForceOrderWidget {
    type Config = ForceOrderConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.force_order");
    const DISPLAY_NAME: &'static str = "widget.names.force_order";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str = "Live liquidation orders across Binance and OKX";
    const ICON: Option<&'static str> = Some("widgets/force_order.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        // The display context the rows are named from is the published catalog the
        // service captured, injected here rather than reached for inside the loop.
        let source = cx
            .global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .market();
        let display = cx
            .global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .catalog_arc();
        Self::new_with_source(source, display, status, recovery, cx)
    }

    /// The table is the content, and it is ready as soon as a snapshot has been
    /// accepted — including an empty one. An empty table is what "no force orders
    /// right now" looks like; it is not a state the widget reports.
    ///
    /// A pane that reads no route at all has nothing to draw however much it once
    /// accepted: the market it was reading is gone, and the neutral notice stands
    /// in its place.
    fn has_content(&self) -> bool {
        !self.routes.is_empty() && self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The markets this pane reads have to be loaded before anything else
        // happens here, and the check is first on purpose: with no route to read
        // there is no stream to start, and the pane waits for one instead of
        // reporting the failure the service would raise about a route nobody
        // serves.
        let routes = needed_routes(cx);
        if !self.market_ready(cx) {
            self.routes = Arc::from([]);
            self.wait_for_market(cx);
            return;
        }
        // The pane reached here only past its own gate, so the markets it was waiting
        // for are in memory again and messages belong to them once more.
        self.awaiting_market = false;
        // Read past the gate: a venue this pane reads loading again is what brings it back after a
        // shrinking commit, and the stream it still holds resolved against the retired generation.
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        // A route set that changed is a different stream rather than a repeat of
        // this one, and replacing it keeps the rows a reader is looking at.
        let retry = self.status.is_failed(cx)
            || (!self.routes.is_empty() && self.routes.as_ref() != routes.as_ref());
        if !retry {
            self.clear_orders(cx);
        }
        self.routes = routes.clone();
        self.pending_snapshot = None;
        self.ready_received = false;
        self.awaiting_snapshot = true;
        self.status.report(WidgetState::Connecting, cx);
        // The two decisions are separate: a reload still drops the rows — they were written at the
        // generation behind the commit — while the request is the replacing one.
        let request = if retry || market_reloaded {
            ForceOrderRequest::Retry { routes }
        } else {
            ForceOrderRequest::Load { routes }
        };
        self.binding.call(request, cx, Self::apply_rpc_result);
    }
}

impl ForceOrderWidget {
    fn new_with_source<S>(
        source: S,
        display: Arc<Catalog>,
        status: Entity<WidgetStatus>,
        recovery: WidgetAction,
        context: &mut Context<Self>,
    ) -> Self
    where
        S: ForceOrderSource,
    {
        let service = ForceOrderService::new(source, display, MAX_FORCE_ORDERS);
        let binding = WidgetBinding::spawn(
            global::handle(context).handle(),
            service,
            context,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let table = context.new(|cx| ForceOrderTable::new(cx.theme()));
        // The pane recovers from the markets it waits for without polling: a venue commit is
        // the one event that can change what may be read, and a change that arrives while the
        // pane has nothing to show is a reason to load again.
        let events = context
            .global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .venues()
            .events();
        context
            .subscribe(&events, |this, _, event: &VenuesUpdated, cx| {
                this.on_venues_updated(event, cx);
            })
            .detach();

        Self {
            binding,
            table,
            status,
            recovery,
            routes: Arc::from([]),
            snapshot: None,
            pending_snapshot: None,
            ready_received: false,
            awaiting_snapshot: true,
            awaiting_market: false,
            market_reloaded: false,
            // The rows are drawn by the table entity, which keeps what it drew until
            // it is told otherwise: moving the display zone has to wake the table.
            _time_zone: on_data_time_zone_change(context, |this, _, cx| {
                this.table.update(cx, |_, cx| cx.notify());
                cx.notify();
            }),
        }
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// The pane compares the markets it needs now — every loaded scope that reports
    /// liquidations — with the set it is reading. A commit that leaves the set as it is is a
    /// publication behind markets the pane already has; when it lands on a venue the pane is
    /// reading it is also the user loading that market again, and that replaces the generation the
    /// live stream resolved its listings against: the pane starts over and asks for a stream that
    /// is opened again — see `market_reloaded` — rather than only re-checking its precondition. A
    /// commit that changes neither the set nor a market the pane reads does only that: it
    /// re-checks the precondition and never starts over what the user is watching.
    ///
    /// A set that changed is a different stream rather than a publication of the one it is
    /// reading: a venue loaded while this pane was open adds its routes, and a shrinking commit
    /// takes them away. That is what lets the pane read a market loaded after it was opened —
    /// without it, the second venue would never be read and the pane would keep claiming to
    /// cover the whole market. A failure is terminal: the event does not revive a failed pane,
    /// only the user's own recovery does.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on a venue this pane is reading is that market being loaded again, so the pane
        // cannot keep reading the stream it holds. This is asked before the set comparison because
        // one commit can be both — the venue this pane reads is the one that shrank away — and the
        // flag then survives the gate below, so the load that brings the market back reopens the
        // stream too.
        if self.routes.iter().any(|route| route.venue == event.venue) {
            self.market_reloaded = true;
            self.load(cx);
            return;
        }
        if !routes_changed(&self.routes, &needed_routes(cx)) {
            if !self.market_ready(cx) {
                self.wait_for_market(cx);
            }
            return;
        }
        self.load(cx);
    }

    /// Whether the markets this pane reads are in memory: every route that reports
    /// liquidations is registered and its catalog has been published.
    fn market_ready(&self, cx: &App) -> bool {
        let routes = needed_routes(cx);
        !routes.is_empty() && routes.iter().all(|route| market_ready(cx, route))
    }

    /// Reports the neutral notice for the markets this pane reads, and drops what
    /// it was showing.
    ///
    /// The routes behind those rows are gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: the
    /// stream it was reading is still alive, and a snapshot that was already in
    /// flight would otherwise write a status over the notice.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.clear_orders(cx);
        self.report_markets_not_loaded(cx);
    }

    /// Reports the notice naming the markets that report liquidations.
    ///
    /// This is the one place that reference is produced: [`Self::wait_for_market`] reports it
    /// when the markets go away. The sentence is the framework's to resolve, in the language
    /// showing on the frame that draws it.
    ///
    /// The pane waits for every one of them and lists every one of them: this pane reads the
    /// whole market, so the venues it can read at all and the venues it waits for are the
    /// same answer.
    fn report_markets_not_loaded(&self, cx: &mut Context<Self>) {
        let venues = market_venues();
        self.status.report(
            WidgetState::waiting(Notice::MarketNotLoaded {
                supported: venues.clone(),
                venues,
            }),
            cx,
        );
    }

    /// Reports the neutral notice for a read the directory answered "not loaded"
    /// to, and drops what the pane was showing.
    ///
    /// The pane's own gate asks the directory before it starts a stream, so this is
    /// the answer to a question the gate could not have seen the answer to: the route
    /// was revoked between the two, which is what a venue re-discovering its scopes
    /// does. That is a wait rather than a failure — the connection action is
    /// what puts the catalog back, and the framework draws the notice while the pane
    /// reports no content. Returns whether it reported, so a caller keeps its own
    /// failure path for everything else.
    fn report_waiting_for_directory(
        &mut self,
        error: &MarketError,
        context: &mut Context<Self>,
    ) -> bool {
        if !is_catalog_not_loaded(error) {
            return false;
        }
        self.routes = Arc::from([]);
        self.wait_for_market(context);
        true
    }

    /// Publishes the accepted snapshot to the table entity.
    ///
    /// The table is the only part of the body that renders rows, so it is
    /// notified at the exact boundaries where the accepted snapshot changes.
    /// Notifying it is what busts the body's cache; nothing else repaints a
    /// cached body.
    fn sync_table(&mut self, context: &mut Context<Self>) {
        let snapshot = self.snapshot.clone().unwrap_or_default();
        self.table.update(context, |table, context| {
            table.snapshot = snapshot;
            context.notify();
        });
    }

    fn clear_orders(&mut self, context: &mut Context<Self>) {
        self.snapshot = None;
        self.sync_table(context);
    }

    /// Reports that the widget is showing its table.
    ///
    /// An accepted snapshot means the widget has content even when it holds no
    /// orders: connecting successfully is enough to show the table, and an empty
    /// table is the honest rendering of "nothing to show yet". There is no
    /// separate empty state to report.
    fn report_snapshot_state(&mut self, context: &mut Context<Self>) {
        self.status.report(WidgetState::Ready, context);
    }

    fn apply_snapshot(&mut self, snapshot: ForceOrderSnapshot, context: &mut Context<Self>) {
        self.snapshot = Some(snapshot);
        self.awaiting_snapshot = false;
        self.report_snapshot_state(context);
        self.sync_table(context);
    }

    fn apply_reliable(
        &mut self,
        key: ForceOrderStreamKey,
        message: Arc<ForceOrderStreamMessage>,
        context: &mut Context<Self>,
    ) {
        if key != ForceOrderStreamKey::AllMarkets
            || self.status.is_failed(context)
            || self.awaiting_market
        {
            return;
        }

        match message.as_ref() {
            ForceOrderStreamMessage::Status(status) => match status {
                ForceOrderStatus::Connecting => {
                    if self.snapshot.is_none() {
                        self.status.report(WidgetState::Connecting, context);
                    }
                }
                ForceOrderStatus::Ready => {
                    self.ready_received = true;
                    if let Some(snapshot) = self.pending_snapshot.take() {
                        self.apply_snapshot(snapshot, context);
                    } else if !self.awaiting_snapshot {
                        self.report_snapshot_state(context);
                    }
                }
                ForceOrderStatus::Error(error) => {
                    if self.report_waiting_for_directory(error, context) {
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status.report(
                        WidgetState::failed(Arc::new(error.clone()), recovery),
                        context,
                    );
                }
            },
            ForceOrderStreamMessage::Snapshot(_) => {}
        }
    }

    fn apply_latest(
        &mut self,
        key: ForceOrderStreamKey,
        message: Arc<ForceOrderStreamMessage>,
        context: &mut Context<Self>,
    ) {
        if key != ForceOrderStreamKey::AllMarkets
            || self.status.is_failed(context)
            || self.awaiting_market
        {
            return;
        }

        if let ForceOrderStreamMessage::Snapshot(snapshot) = message.as_ref() {
            if self.ready_received {
                self.apply_snapshot(snapshot.clone(), context);
            } else {
                self.pending_snapshot = Some(snapshot.clone());
            }
        }
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<ForceOrderProtocol>,
        context: &mut Context<Self>,
    ) {
        match result {
            Ok(ForceOrderResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status
                    .report(WidgetState::failed(Arc::new(error), recovery), context);
            }
        }
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
                "ForceOrderTableFrame",
            )))
            .aria_label(t!(
                cx,
                "widget.force_order.table",
                markets = widget::venue::joined_names(
                    cx,
                    &market_venues()
                        .iter()
                        .map(VenueDisplay::name_key)
                        .collect::<Vec<_>>()
                )
            ))
            .size_full()
            .min_h_0()
            .child(Table::new(
                // The table keeps its own scroll position in state keyed by this
                // id, so it has to stay stable across frames.
                ElementId::Name(SharedString::new_static("ForceOrderTable")),
                &self.table,
            ))
            .into_any_element()
    }
}

impl Render for ForceOrderWidget {
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui_kit::{App, Entity, TestAppContext, WeakEntity};
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        catalog::{batch::CatalogEntry, test_support},
        market::{domain::DisconnectReason, error::MarketError},
        routing::PERPETUAL_SCOPE,
    };
    use market_participants_model::{
        decimal::{Price, Quantity},
        identifier::VenueId,
        listing::{
            route::{VenueListingKey, VenueRoute},
            rules::ListingPrecision,
        },
        market::{
            book::ListingForceOrder,
            price::MarketPrice,
            units::{MarketQuantity, PositionSide, QuantityUnit},
        },
    };
    use widget::error::LocalizedError;

    use super::*;
    use crate::{
        model::{ForceOrderEntry, ForceOrderListing, ForceOrderState},
        service::{ForceOrderEventSubscription, ForceOrderSource},
    };

    fn venue() -> VenueId {
        VenueId::Binance
    }

    /// The route the fixture market reports liquidations on: the Binance perpetual scope,
    /// perpetual scope, because a route the registry cannot hold is a market the
    /// user cannot load either.
    fn route() -> VenueRoute {
        test_support::route("binance", PERPETUAL_SCOPE)
    }

    /// Installs the market participants root and the directory a pane reads, with the fixture
    /// market loaded or not.
    ///
    /// A loaded market is one commit of every one of its scopes, catalogs and metadata, so
    /// the directory publishes the fixture route here. What the service reads is
    /// the source a test hands it, so the fixture catalog itself holds nothing.
    fn install(cx: &mut App, loaded: bool) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        global::install(cx).expect("test Tokio runtime should build");
        let directory = test_support::Fixture::with_source(
            route(),
            "binance-usd-m-perp",
            test_support::pending(),
        );
        let connector = market_participants_connector::test_support::install_root(
            cx,
            Arc::clone(directory.catalog_arc()),
        );
        if loaded {
            connector
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            test_support::publish_fixture_catalogs(cx, &[(route(), Vec::new())]);
        }
    }

    #[derive(Clone, Copy)]
    struct PendingForceOrderSource;

    struct PendingForceOrderSubscription;

    impl ForceOrderSource for PendingForceOrderSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            std::future::pending().await
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            std::future::pending()
        }
    }

    impl ForceOrderEventSubscription for PendingForceOrderSubscription {
        fn recv(
            &mut self,
        ) -> impl Future<Output = Option<Result<Arc<Vec<ListingForceOrder>>, MarketError>>> + Send
        {
            std::future::pending()
        }
    }

    /// A source that counts what the pane asks it for, so a test can tell "the pane started
    /// over" from "the pane re-checked its precondition".
    ///
    /// A stream the pane keeps is one the service never asks for again, so the subscription
    /// count is what says the pane did not start one; the listing count is what says the service
    /// was not told to resolve the markets again either.
    #[derive(Clone, Default)]
    struct CountingForceOrderSource {
        subscriptions: Arc<std::sync::atomic::AtomicUsize>,
        listings: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ForceOrderSource for CountingForceOrderSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            self.listings
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Ok(vec![test_support::listing_key(route, "BTCUSDT")])
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            self.subscriptions
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            std::future::pending()
        }
    }

    /// A source that resolves a route's listings through the catalogue the fixture publishes, at the
    /// moment a stream is opened for it, and records the increments they were published at then.
    ///
    /// A stream's resolution is what a venue reload replaces, so a test about a reload has to see
    /// both that the pane asked for a new stream and which generation's rules that stream resolved
    /// its markets at: this is the one seam where the two are a single record.
    #[derive(Clone)]
    struct ResolvingForceOrderSource {
        catalog: Arc<Catalog>,
        resolutions: Arc<std::sync::Mutex<Vec<Vec<ListingPrecision>>>>,
    }

    /// The increments every market one resolving source was asked for was published at, one entry
    /// per stream it was asked for.
    fn resolutions_of(
        resolutions: &Arc<std::sync::Mutex<Vec<Vec<ListingPrecision>>>>,
    ) -> Vec<Vec<ListingPrecision>> {
        resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    impl ForceOrderSource for ResolvingForceOrderSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            let published = self.catalog.cached_listings(route).unwrap_or_default();
            self.resolutions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(
                    published
                        .iter()
                        .map(|entry| ListingPrecision::of(&entry.snapshot.rules))
                        .collect(),
                );
            Ok(published
                .iter()
                .map(|entry| entry.listing.key.clone())
                .collect())
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            std::future::pending()
        }
    }

    /// A source whose streams all resolve the fixture market's listing, and which counts the
    /// streams that were asked to.
    ///
    /// A stream resolves its routes' listings when it is opened, so this counts openings: it
    /// answers with a listing the route serves whether or not a catalog publishes one, which is
    /// what a test about the *stream* rather than about the generations behind it needs.
    #[derive(Clone)]
    struct ListingResolvingSource {
        resolutions: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ForceOrderSource for ListingResolvingSource {
        type Subscription = PendingForceOrderSubscription;

        async fn perpetual_listings(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            self.resolutions
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Ok(vec![restated_entry("0.01").listing.key])
        }

        fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
            std::future::pending()
        }
    }

    /// Counts the venue commits it is told about.
    ///
    /// A test that asserts a pane did *not* act on a commit still has to know the commit reached
    /// it: this probe subscribes the same way and after the pane did, so the delivery it counts is
    /// one the pane's own handler has already seen.
    struct CommitProbe {
        deliveries: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CommitProbe {
        fn new(
            cx: &mut Context<Self>,
            events: &Entity<market_participants_connector::venue::loaded::VenueEventSource>,
            deliveries: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            cx.subscribe(events, |probe, _, _: &VenuesUpdated, _| {
                probe
                    .deliveries
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
            .detach();
            Self { deliveries }
        }
    }

    fn entry(timestamp: i64) -> ForceOrderEntry {
        ForceOrderEntry {
            order: ListingForceOrder {
                listing: test_support::listing_key(&route(), "BTCUSDT"),
                price: MarketPrice {
                    value: Price::from(100_u64),
                    quote_asset: test_support::token("fixture", "USDT"),
                    basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
                },
                quantity: MarketQuantity::traded(Quantity::from(2_u64), QuantityUnit::Contracts)
                    .expect("a contract count is a traded quantity"),
                position_side: PositionSide::Long,
                timestamp: (timestamp),
            },
            listing: ForceOrderListing {
                label: Some(SharedString::new_static("BTCUSDT")),
                precision: Some(precision()),
            },
        }
    }

    /// A price tick of one cent and a quantity step of one thousandth, so a label
    /// that keeps the listing's precision is told apart from the canonical form.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity step should parse"),
        }
    }

    /// The market's listing again, published at another price tick.
    ///
    /// It is the shape a production load commits: the load re-fetches the catalogue, and what it
    /// publishes is the listing restated at the venue's own increments.
    fn restated_entry(price_tick: &str) -> CatalogEntry {
        let mut restated = test_support::entry(
            &route(),
            test_support::spot_token("binance", "BTC", "USDT"),
            "BTCUSDT",
        );
        restated.snapshot.rules.common.price.tick = price_tick
            .parse()
            .expect("the restated price tick should parse");
        restated
    }

    fn snapshot(timestamp: i64) -> ForceOrderSnapshot {
        let mut state = ForceOrderState::new(1);
        state.merge(&[entry(timestamp)]);
        state.snapshot()
    }

    fn empty_snapshot() -> ForceOrderSnapshot {
        ForceOrderState::new(1).snapshot()
    }

    fn disconnect() -> MarketError {
        MarketError::Disconnected(DisconnectReason::Normal)
    }

    /// Creates a widget the way the framework does: state first, then one load.
    fn widget(cx: &mut TestAppContext) -> Entity<ForceOrderWidget> {
        widget_over(cx, PendingForceOrderSource, Arc::new(Catalog::new()), true)
    }

    /// The same, over a source a test drives, a directory it arranges and a market
    /// it may leave unloaded.
    fn widget_over<S>(
        cx: &mut TestAppContext,
        source: S,
        display: Arc<Catalog>,
        loaded: bool,
    ) -> Entity<ForceOrderWidget>
    where
        S: ForceOrderSource,
    {
        widget_over_with(cx, loaded, move |_| (source, display))
    }

    /// The same, with the source and the display generation built from the catalogue the fixture
    /// installs.
    ///
    /// A row is named and written at the increments of the generation the service captured when it
    /// opened the stream, so a test about which generation a stream resolved at needs the two sides
    /// of that to be one catalogue rather than two: both are handed to `build`.
    fn widget_over_with<S>(
        cx: &mut TestAppContext,
        loaded: bool,
        build: impl FnOnce(&Arc<Catalog>) -> (S, Arc<Catalog>),
    ) -> Entity<ForceOrderWidget>
    where
        S: ForceOrderSource,
    {
        cx.executor().allow_parking();
        // The market is installed and its commit flushed before the pane is built, which is the
        // order production has: the user loads the market, and a pane opened afterwards reads the
        // generation that load published rather than a commit about a market it was not reading.
        cx.update(|cx| install(cx, loaded));
        cx.run_until_parked();
        cx.update(|cx| {
            let catalog = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .catalog_arc();
            let (source, display) = build(&catalog);
            let widget = cx.new(|cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<ForceOrderWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                ForceOrderWidget::new_with_source(source, display, status, recovery, cx)
            });
            widget.update(cx, |widget, cx| widget.load(cx));
            widget
        })
    }

    fn status(widget: &Entity<ForceOrderWidget>, cx: &App) -> WidgetState {
        let status = widget.read(cx).status.clone();
        status.state(cx)
    }

    /// Lets the pane and the application runtime take a turn each until `ready`.
    ///
    /// A request crosses both executors — the pane sends it from GPUI's, the service
    /// answers on the application runtime — and the directory signals its publications
    /// from its own task, so waiting for one of them alone waits for nothing.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The same read, for the places where the widget is already borrowed.
    fn reported(widget: &ForceOrderWidget, cx: &App) -> WidgetState {
        widget.status.state(cx)
    }

    /// Reports a ready snapshot as the stream would, then returns the widget.
    fn accept_snapshot(
        widget: &Entity<ForceOrderWidget>,
        snapshot: ForceOrderSnapshot,
        cx: &mut App,
    ) {
        widget.update(cx, |widget, context| {
            widget.binding.cancel();
            widget.ready_received = true;
            widget.apply_snapshot(snapshot, context);
        });
    }

    #[gpui_kit::test]
    fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
        });
    }

    /// A market the user has not loaded is a wait rather than a failure (⑮): the
    /// pane names what to load and has nothing to draw — not even for a snapshot
    /// it once accepted, because nothing on screen is backed by a market any more
    /// (⑰).
    #[gpui_kit::test]
    fn an_unloaded_market_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let widget = widget_over(cx, PendingForceOrderSource, Arc::new(Catalog::new()), false);

        cx.update(|cx| {
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "an unloaded market is not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: market_venues(),
                    supported: market_venues(),
                }),
                "waiting is not working"
            );
            let notice = status.notice().expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance") && sentence.contains("OKX"),
                "the notice names the markets that report liquidations: {sentence}"
            );
            assert!(
                !sentence.contains("Hyperliquid"),
                "the notice names the venues whose loader can report liquidations, not every \
                 venue this build speaks: {sentence}"
            );
            assert!(!widget.read(cx).has_content());

            accept_snapshot(&widget, snapshot(1), cx);
            assert!(
                !widget.read(cx).has_content(),
                "a route the registry does not hold is no market to draw"
            );
        });
    }

    /// A pane that is waiting reads its notice where it is reported, so a language chosen after
    /// it reported — with no reload and nothing asked again — is the language it waits in.
    ///
    /// The notice is a reference into the catalogue rather than a sentence, so the pane reports
    /// nothing again: nothing but the frame that draws it resolves the words.
    #[gpui_kit::test]
    fn a_language_change_should_re_report_the_market_notice(cx: &mut TestAppContext) {
        let widget = widget_over(cx, PendingForceOrderSource, Arc::new(Catalog::new()), false);

        let (notice, english) = cx.update(|cx| {
            let notice = status(&widget, cx)
                .notice()
                .cloned()
                .expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance") && sentence.contains("OKX"),
                "the pane waits in the language it reported in: {sentence}"
            );

            (notice, sentence.to_string())
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));
        cx.run_until_parked();

        cx.update(|cx| {
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("币安") && sentence.contains("欧易"),
                "the same reference resolves in the language showing now: {sentence}"
            );
            assert_ne!(
                sentence, english,
                "and it is not the copy of the language the interface left: {sentence}"
            );
        });
    }

    /// Loading the venue is what a waiting pane recovers from, by itself and
    /// without a poll (⑯).
    #[gpui_kit::test]
    fn loading_the_venue_should_let_the_waiting_pane_start(cx: &mut TestAppContext) {
        let widget = widget_over(cx, PendingForceOrderSource, Arc::new(Catalog::new()), false);
        cx.update(|cx| {
            assert!(status(&widget, cx).notice().is_some());
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            test_support::publish_fixture_catalogs(cx, &[(route(), Vec::new())]);
        });
        // The directory signals its publications through its own task, so the
        // event that re-runs the load arrives on the executor.
        cx.run_until_parked();

        cx.update(|cx| {
            let status = status(&widget, cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert!(status.failure().is_none());
            assert_eq!(status, WidgetState::Connecting);
        });
    }

    /// The directory answering "this route's catalog is not in memory" is a wait, not a
    /// failure: the pane's own gate asked the same question a moment earlier, so
    /// this is the answer changing underneath the request — a venue re-discovering its
    /// scopes is what does it — and loading the market again is what brings it back.
    /// Reporting it as `failed` would leave the pane terminal over a route the next
    /// publication brings back, and no later event may revive a failed pane.
    #[gpui_kit::test]
    fn a_directory_that_has_not_loaded_a_catalog_should_leave_the_pane_waiting(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);

        cx.update(|cx| {
            // The route is really there and its catalog is really published, so the
            // answer this test delivers is one only a revocation mid-request produces.
            assert!(
                cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                    .catalog()
                    .cached_listings(&route())
                    .is_some(),
                "the fixture market is loaded before the directory's answer arrives"
            );

            widget.update(cx, |widget, context| {
                let key = ForceOrderStreamKey::AllMarkets;
                widget.apply_reliable(
                    key,
                    Arc::new(ForceOrderStreamMessage::Status(ForceOrderStatus::Error(
                        MarketError::Catalog(Box::new(
                            market_participants_connector::catalog::error::CatalogError::CatalogNotLoaded {
                                route: route(),
                            },
                        )),
                    ))),
                    context,
                );
            });

            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "a catalog that has not loaded is a wait, not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: market_venues(),
                    supported: market_venues(),
                }),
                "waiting is not working"
            );
            let notice = status
                .notice()
                .expect("the pane reports the neutral notice");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance") || sentence.contains("OKX"),
                "the notice names the markets that report liquidations: {sentence}"
            );
            assert!(
                !sentence.contains("Hyperliquid"),
                "the notice names the venues whose loader can report liquidations, not every \
                 venue this build speaks: {sentence}"
            );
            assert!(!widget.read(cx).has_content());
        });
    }

    /// A stream that is still alive after its markets went away must not write a status
    /// over the notice: the pane waits, and a message already in flight belongs to
    /// markets the pane no longer reports reading. Without the guard the snapshot is
    /// accepted, the `Ready` clears the notice, and the framework draws the
    /// placeholder instead of the notice the user is supposed to act on.
    #[gpui_kit::test]
    fn a_late_message_after_the_market_went_away_should_not_clear_the_notice(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);
        let key = ForceOrderStreamKey::AllMarkets;

        cx.update(|cx| {
            // The pane is reading, which is the state a revocation interrupts.
            accept_snapshot(&widget, snapshot(1), cx);
            assert!(widget.read(cx).has_content(), "the rows are what arrived");

            // The venue loaded again and published a scope set without this one: the pane's own
            // gate is what notices, and it reports the neutral notice.
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue(), Vec::new())], &[])
                .expect("an empty scope set unloads the venue's slot");
            widget.update(cx, |widget, context| widget.load(context));

            let waiting = status(&widget, cx);
            assert!(
                waiting.notice().is_some(),
                "the pane reports the neutral notice"
            );
            assert!(waiting.failure().is_none(), "waiting is not a failure");

            widget.update(cx, |widget, context| {
                widget.apply_latest(
                    key,
                    Arc::new(ForceOrderStreamMessage::Snapshot(snapshot(9))),
                    context,
                );
            });
            assert!(
                !widget.read(cx).has_content(),
                "a snapshot from the market that went away must not become content again"
            );
            assert!(
                status(&widget, cx).notice().is_some(),
                "the notice is what the pane reports after a late snapshot"
            );

            widget.update(cx, |widget, context| {
                widget.apply_reliable(
                    key,
                    Arc::new(ForceOrderStreamMessage::Status(ForceOrderStatus::Ready)),
                    context,
                );
            });
            assert!(
                status(&widget, cx).notice().is_some(),
                "the notice is what the pane reports after a late status"
            );
        });
    }

    #[gpui_kit::test]
    fn an_accepted_snapshot_should_become_content_and_clear_the_status(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);

            assert!(widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Ready);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    /// A successful connection is enough to show the table. An empty snapshot is
    /// content — an empty table — and the widget reports nothing about it: the table
    /// itself is what says it holds no orders.
    #[gpui_kit::test]
    fn an_empty_snapshot_should_be_content_and_report_nothing(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, empty_snapshot(), cx);

            assert!(widget.read(cx).has_content());
            let table = widget.read(cx).table.clone();
            assert_eq!(table.read(cx).rows_count(), 0);
            assert!(
                TableDataSet::<COLUMN_COUNT>::empty(table.read(cx), cx).is_some(),
                "an empty table has to say it is empty rather than leave its body blank"
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
        let widget = widget(cx);

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
        let widget = widget(cx);

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
        let widget = widget(cx);

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
        let widget = widget(cx);
        let key = ForceOrderStreamKey::AllMarkets;

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
                    key,
                    Arc::new(ForceOrderStreamMessage::Status(ForceOrderStatus::Ready)),
                    context,
                );
                widget.apply_latest(
                    key,
                    Arc::new(ForceOrderStreamMessage::Snapshot(snapshot(1))),
                    context,
                );

                assert_eq!(reported(widget, context), failed);
                assert!(reported(widget, context).is_failed());
                assert!(widget.snapshot.is_none());
            });
        });
    }

    #[gpui_kit::test]
    fn a_snapshot_before_the_ready_boundary_should_await_it(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let key = ForceOrderStreamKey::AllMarkets;

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                widget.apply_latest(
                    key,
                    Arc::new(ForceOrderStreamMessage::Snapshot(snapshot(1))),
                    context,
                );
                assert!(widget.snapshot.is_none());
                assert!(!widget.has_content());

                widget.apply_reliable(
                    key,
                    Arc::new(ForceOrderStreamMessage::Status(ForceOrderStatus::Ready)),
                    context,
                );

                assert_eq!(
                    widget.snapshot.as_ref().map(ForceOrderSnapshot::len),
                    Some(1)
                );
                assert!(widget.has_content());
                assert_eq!(reported(widget, context), WidgetState::Ready);
            });
        });
    }

    #[gpui_kit::test]
    fn a_stream_failure_should_become_the_reported_state(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let key = ForceOrderStreamKey::AllMarkets;

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                widget.apply_reliable(
                    key,
                    Arc::new(ForceOrderStreamMessage::Status(ForceOrderStatus::Error(
                        disconnect(),
                    ))),
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
        let widget = widget(cx);

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot(1), cx);
            assert!(widget.read(cx).has_content());

            widget.update(cx, |widget, context| widget.load(context));

            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    /// This pane reads the whole market, so a venue loaded while it already has
    /// content is a *different* route set rather than a publication of the one it is
    /// reading: the event has to let it through, or the second venue would never be
    /// read and the pane would keep claiming to cover every venue. The protection that
    /// stays is against restarting a stream that already reads the same set.
    #[gpui_kit::test]
    fn a_venue_loaded_after_the_rows_arrived_should_be_read_too(cx: &mut TestAppContext) {
        let widget = widget_over(cx, PendingForceOrderSource, Arc::new(Catalog::new()), false);
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            test_support::publish_fixture_catalogs(cx, &[(route(), Vec::new())]);
        });
        settle_until(cx, "the first venue to be read", |cx| {
            !widget.read(cx).routes.is_empty()
        });

        cx.update(|cx| {
            // The rows arrive the way a served snapshot does — through the very entry the
            // binding calls — rather than through the fixture that cancels the binding:
            // the binding stays alive, so the question this test asks is the only thing
            // that changes anything here.
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            assert!(widget.read(cx).has_content());
            assert_eq!(
                widget.read(cx).routes.as_ref(),
                [route()].as_slice(),
                "the pane is reading the one venue that is loaded"
            );
        });

        cx.update(|cx| {
            let okx = market_participants_model::identifier::VenueId::Okx;
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[okx]),
                    &[],
                    &[],
                )
                .expect("the second venue's routes should load");
            test_support::publish_fixture_catalogs(
                cx,
                &[
                    (route(), Vec::new()),
                    (test_support::route("okx", "swap"), Vec::new()),
                ],
            );
        });
        settle_until(cx, "the venue loaded after the rows to be read", |cx| {
            widget.read(cx).routes.len() == 2
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).routes.len(),
                2,
                "the venue loaded after the rows arrived has to be read as well: {:?}",
                widget.read(cx).routes
            );
            assert!(
                widget
                    .read(cx)
                    .routes
                    .iter()
                    .any(|route| route.venue.as_str() == "okx"),
                "the new venue is one of the routes the pane reads"
            );
        });
    }

    /// This pane reads the whole market, so the set of markets it needs is what a commit is
    /// judged by one rule: a venue whose scopes enter the loaded venues is a set that grew — the
    /// pane starts a stream that covers it — and a shrinking commit that takes them away is a
    /// set that shrank, so the pane starts over on what is left.
    ///
    /// What proves the pane started over is the source's own count of the streams it was asked
    /// for, beside the route set it holds: a handler that only re-checked its precondition would
    /// leave the count where it was and never read the venue that had just loaded.
    #[gpui_kit::test]
    fn a_whole_market_pane_wakes_when_its_routes_change(cx: &mut TestAppContext) {
        let source = CountingForceOrderSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let widget = widget_over(cx, source, Arc::new(Catalog::new()), false);
        let okx = VenueId::Okx;
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            test_support::publish_fixture_catalogs(cx, &[(route(), Vec::new())]);
        });
        settle_until(cx, "the first venue to be read", |_| {
            demands.load(std::sync::atomic::Ordering::Acquire) >= 1
        });
        let reading = demands.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            reading >= 1,
            "the pane asked for a stream over the venue that is loaded"
        );

        // A venue loaded while this pane is reading is a set that grew, so it is read too.
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&okx),
                    ),
                    &[],
                    &[],
                )
                .expect("the second venue's routes should load");
            test_support::publish_fixture_catalogs(
                cx,
                &[
                    (route(), Vec::new()),
                    (test_support::route("okx", "swap"), Vec::new()),
                ],
            );
        });
        settle_until(cx, "the venue loaded after the pane to be read", |_| {
            demands.load(std::sync::atomic::Ordering::Acquire) > reading
        });

        cx.update(|cx| {
            assert!(
                widget
                    .read(cx)
                    .routes
                    .iter()
                    .any(|route| route.venue.as_str() == "okx"),
                "the venue loaded after the pane started is one of the routes it reads"
            );
            assert!(
                demands.load(std::sync::atomic::Ordering::Acquire) > reading,
                "a set that grew starts a stream that covers it, rather than leaving the one it had"
            );
        });

        // A shrinking commit takes the second venue's scopes away: the set is different again,
        // so the pane starts over on what is left rather than reading a market nobody loaded.
        let before_shrink = demands.load(std::sync::atomic::Ordering::Acquire);
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(&[], &[(okx, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        settle_until(cx, "the withdrawn venue to leave the set", |_| {
            demands.load(std::sync::atomic::Ordering::Acquire) > before_shrink
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).routes.as_ref(),
                [route()].as_slice(),
                "the withdrawn venue is no longer a market this pane reads"
            );
            assert!(
                demands.load(std::sync::atomic::Ordering::Acquire) > before_shrink,
                "a set that shrank starts over on what is left"
            );
        });
    }

    /// The other half of reading the reload flag past the gate, and the case that makes that
    /// placement load-bearing: a shrinking commit takes the pane's markets away, and waiting for
    /// them empties the very set a commit is compared against — so the commit that brings the
    /// market back cannot land on one of them and ask for the reload a second time. The flag the
    /// shrinking commit left has to survive the wait, and the load it unblocks has to open a new
    /// stream rather than be answered idempotently for the stream that is still running.
    ///
    /// What proves it is the source's own record: a stream resolves its routes' listings when it is
    /// opened, so a second resolution is a second stream.
    #[gpui_kit::test]
    fn a_market_that_comes_back_after_a_shrinking_commit_should_reopen_the_stream(
        cx: &mut TestAppContext,
    ) {
        let resolutions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let widget = widget_over_with(cx, true, {
            let resolutions = Arc::clone(&resolutions);
            move |catalog| (ListingResolvingSource { resolutions }, Arc::clone(catalog))
        });
        settle_until(cx, "the markets to be read", |_| {
            resolutions.load(std::sync::atomic::Ordering::Acquire) == 1
        });
        cx.update(|cx| {
            assert!(
                !widget.read(cx).status.is_failed(cx),
                "the pane is reading the market it was built over: {:?}",
                widget.read(cx).status.state(cx)
            );
        });

        // The venue loads again with no scope at all, which is what a shrinking commit does.
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue(), Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        settle_until(cx, "the pane to wait for the market it lost", |cx| {
            widget.read(cx).awaiting_market
        });
        cx.update(|cx| {
            assert!(
                widget.read(cx).routes.is_empty(),
                "the markets the pane was reading are gone, so a commit can no longer land on one"
            );
        });

        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the venue loads again");
        });
        settle_until(cx, "the markets to be read again", |_| {
            resolutions.load(std::sync::atomic::Ordering::Acquire) == 2
        });

        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the pane is reading its markets again"
            );
            assert_eq!(
                resolutions.load(std::sync::atomic::Ordering::Acquire),
                2,
                "and the load the returning market unblocked opened a new stream"
            );
        });
    }
    /// A refresh of a venue the pane reads replaces the catalogue generation its live stream
    /// resolved those markets' listings — their rules and displayed precision — against, while the
    /// stream keeps what it resolved when it was opened. So the pane starts over — the rows go,
    /// because nothing on screen can be vouched for — and the request it sends is the one that
    /// opens a new stream, whose listings are resolved at the generation this load published.
    ///
    /// What proves both halves is the source's own record: the markets it was asked to resolve at
    /// the moment the service opened the stream, and the increments they were published at then.
    #[gpui_kit::test]
    fn a_refresh_of_a_market_the_pane_reads_should_reopen_its_stream(cx: &mut TestAppContext) {
        let resolutions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let widget = widget_over_with(cx, true, {
            let resolutions = Arc::clone(&resolutions);
            move |catalog| {
                (
                    ResolvingForceOrderSource {
                        catalog: Arc::clone(catalog),
                        resolutions,
                    },
                    Arc::clone(catalog),
                )
            }
        });
        settle_until(cx, "the markets to be read", |_| {
            resolutions_of(&resolutions).len() == 1
        });
        let generation = cx.read(|cx| widget.read(cx).routes.clone());

        // Rows arrive the way a served snapshot does, so the pane has something a reader is watching
        // when the commit lands.
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.ready_received = true;
                widget.apply_snapshot(snapshot(1), context);
            });
            assert!(
                widget.read(cx).has_content(),
                "the pane is showing its rows"
            );

            // The same venue loads again and the catalogue it republishes restates the listing's
            // price tick, which is the shape a production load has: it re-fetches the catalogue.
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the venue loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
        });
        settle_until(cx, "the markets to be read again", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).routes,
                generation,
                "a refresh of a market the pane reads leaves it reading the same ones"
            );
            assert!(
                !widget.read(cx).has_content(),
                "and the rows are gone: they were written at the generation that was replaced"
            );
            assert_eq!(
                resolutions_of(&resolutions)[1]
                    .first()
                    .expect("the stream that was opened resolved its route's listings")
                    .price_tick
                    .to_string(),
                "0.01",
                "the stream that was opened resolved at the generation this load published"
            );
        });
    }

    /// A failure is terminal: a venue commit does not revive a failed pane, even when the set of
    /// markets it reads changes under it.
    ///
    /// The commit here really does change the set — a venue the pane was not reading loads — so a
    /// pane without the terminal guard would start over and read it. What it does instead is stay
    /// where the user left it, with the stream count unmoved.
    #[gpui_kit::test]
    fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let source = CountingForceOrderSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let widget = widget_over(cx, source, Arc::new(Catalog::new()), false);
        let okx = VenueId::Okx;
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue()]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            test_support::publish_fixture_catalogs(cx, &[(route(), Vec::new())]);
        });
        settle_until(cx, "the venue to be read", |_| {
            demands.load(std::sync::atomic::Ordering::Acquire) >= 1
        });
        let reading = demands.load(std::sync::atomic::Ordering::Acquire);

        // The pane fails, and a failure is the state only the user's own recovery leaves.
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });
            assert!(
                status(&widget, cx).failure().is_some(),
                "the pane is in its terminal failure"
            );
        });

        let deliveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx
                .global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .venues()
                .events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        cx.update(|cx| {
            cx.global::<market_participants_connector::connector::MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&okx),
                    ),
                    &[],
                    &[],
                )
                .expect("the second venue's routes should load");
            test_support::publish_fixture_catalogs(
                cx,
                &[
                    (route(), Vec::new()),
                    (test_support::route("okx", "swap"), Vec::new()),
                ],
            );
        });
        settle_until(cx, "the commit to reach its subscribers", |_| {
            deliveries.load(std::sync::atomic::Ordering::Acquire) > 0
        });

        cx.update(|cx| {
            assert!(
                status(&widget, cx).failure().is_some(),
                "a venue commit does not revive a failed pane"
            );
            assert_eq!(
                widget.read(cx).routes.as_ref(),
                [route()].as_slice(),
                "the failed pane reads nothing new"
            );
            assert_eq!(
                demands.load(std::sync::atomic::Ordering::Acquire),
                reading,
                "the failed pane started no stream"
            );
        });
    }

    #[gpui_kit::test]
    fn a_retry_after_a_failure_should_keep_the_snapshot_the_widget_still_shows(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);

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
                    .map(ForceOrderSnapshot::len),
                Some(1),
                "a retry must not blank data the user is still reading"
            );
        });
    }

    #[gpui_kit::test]
    fn an_accepted_snapshot_should_reach_the_table_region(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            assert_eq!(widget.read(cx).table.read(cx).rows_count(), 0);

            accept_snapshot(&widget, snapshot(1), cx);
            assert_eq!(widget.read(cx).table.read(cx).rows_count(), 1);

            widget.update(cx, |widget, context| widget.clear_orders(context));
            assert_eq!(widget.read(cx).table.read(cx).rows_count(), 0);
        });
    }

    /// Every cached column of a row is filled, and none of them is the route: the
    /// route is drawn from the row's own typed key, so a name that changed language
    /// between two frames is never served stale out of a cache.
    #[test]
    fn force_order_display_should_fill_every_cached_column() {
        let table = ForceOrderTable {
            snapshot: snapshot(1),
            long: gpui_kit::green(),
            short: gpui_kit::red(),
        };
        let display = table.snapshot.orders()[0].display(time::UtcOffset::UTC);

        assert_eq!(
            [
                display
                    .listing
                    .as_ref()
                    .map(SharedString::as_ref)
                    .unwrap_or(""),
                display.price.as_ref(),
                display.quantity.as_ref(),
                display.time.as_ref(),
            ],
            // The numbers keep the increments of the listing the liquidation was
            // reported on: a whole price is still read with the decimals its tick
            // states.
            ["BTCUSDT", "100.00", "2.000", "00:00:00.001"]
        );
        assert_eq!(table.rows_count(), 1);
    }

    /// The route column is a user-visible route, so it is written with the one formatter
    /// every route a reader sees is rendered by — not with the canonical lower-case
    /// identity that belongs in logs.
    ///
    /// The column draws `route_display_string(&row.order().listing.route, cx)`, so
    /// this states the same expression over the same row the column is handed: the
    /// row's own route has to be the fixture route, and formatting it has to produce
    /// the user-visible form. Drawing `route()` — the canonical identity — in that
    /// column while leaving both halves true would take a *different* expression, so
    /// the pair is what can be pinned without extracting the column into a wrapper
    /// that exists only for the test.
    #[gpui_kit::test]
    fn a_force_order_row_should_offer_the_user_visible_route(cx: &mut TestAppContext) {
        let table = ForceOrderTable {
            snapshot: snapshot(1),
            long: gpui_kit::green(),
            short: gpui_kit::red(),
        };

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            let row = &table.snapshot.orders()[0];
            let drawn = row.order().listing.route.clone();

            assert_eq!(
                drawn,
                route(),
                "the column's input is the row's own route, which is the fixture route"
            );
            assert_eq!(
                route_display_string(&drawn, cx).as_ref(),
                "Binance/USD-M-PERP",
                "and formatting that input states the route the way a reader sees it"
            );
            assert_ne!(
                route_display_string(&drawn, cx).as_ref(),
                drawn.to_string(),
                "the canonical lower-case route is a machine identity"
            );
        });
    }

    /// Guards against a widget silently losing the recovery the framework gave
    /// it: every failure path must republish the same one.
    #[test]
    fn the_widget_should_keep_one_recovery_for_every_failure_it_reports() {
        let seen = Cell::new(0);
        let recovery = WidgetAction::new("common.retry", move |_| seen.set(seen.get() + 1));
        let clone = recovery.clone();

        assert_eq!(recovery, clone);
    }
}
