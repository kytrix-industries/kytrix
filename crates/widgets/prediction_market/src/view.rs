//! The prediction market pane.
//!
//! The pane follows one event and draws one line per option. It loads a bounded
//! history and then follows the venue's live probability stream; the event
//! selector is the header control, and a venue reload drops the chart it can no
//! longer vouch for. The range the history is read over is one of the pane's four
//! persisted options: the header's timeframe picker states the ranges the venue
//! publishes, the book's toggle, notch and price grouping are the others, and a
//! range switch drops the chart while the new reading lands under the framework's
//! connecting spinner.

use std::{
    cell::Cell,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::manager::ConfigManager;
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, CursorStyle, DragMoveEvent, ElementId, Empty,
    Entity, InteractiveElement, IntoElement, MouseButton, MouseMoveEvent, ParentElement, Pixels,
    Point, Render, Role, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    base::{ElementExt, Selectable},
    div,
    prelude::FluentBuilder,
    px, relative,
};
use i18n::t;
use instrument_selector_ui::{
    prediction_button::PredictionEventButton, prediction_locale::polymarket_locale,
};
use market_participants_connector::{
    connector::MarketParticipantsConnector, venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    identifier::VenueId,
    prediction::{
        PredictionEventRef, PredictionMarketGroup, PredictionMarketRef, PredictionOrderBook,
        PredictionRange,
    },
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::{LocalizedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::notify_on_data_time_zone_change,
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    book::{
        DEFAULT_BOOK_RATIO, GROUPING_MULTIPLIERS, MAX_BOOK_RATIO, MIN_BOOK_RATIO,
        PredictionBookPanel, PredictionOrderBookError, PredictionOrderBookProtocol,
        PredictionOrderBookRequest, PredictionOrderBookService, PredictionOrderBookSource,
        PredictionOrderBookStatus, PredictionOrderBookStreamKey, PredictionOrderBookStreamMessage,
        clamp_ratio, empty_state, grouping_steps, settle_grouping, split_ratio,
    },
    chart::canvas::{ChartCanvasOptions, ChartPaintCaches, chart_element},
    model::{PredictionMarketChoice, PredictionMarketSnapshot, PredictionOrderBookTarget},
    service::{
        PredictionMarketFetchError, PredictionMarketProtocol, PredictionMarketRequest,
        PredictionMarketResponse, PredictionMarketService, PredictionMarketServiceError,
        PredictionMarketSource, PredictionMarketStatus, PredictionMarketStreamKey,
        PredictionMarketStreamMessage,
    },
};

/// A failure the pane reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionMarketError {
    Fetch(PredictionMarketFetchError),
    Rpc(RpcCallError<PredictionMarketServiceError>),
}

impl LocalizedError for PredictionMarketError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for PredictionMarketFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Market(error) => error.localized_message(cx),
            Self::NoMarkets => t!(cx, "error.prediction_market.no_markets"),
            Self::Timeout(timeout) => t!(
                cx,
                "error.prediction_market.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl LocalizedError for PredictionMarketServiceError {
    fn localized_message(&self, _cx: &App) -> SharedString {
        match *self {}
    }
}

/// The options the prediction market persists.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PredictionMarketConfig {
    /// The range the history is read over.
    #[serde(rename = "range")]
    pub range: PredictionRange,
    /// Whether the order book is drawn below the chart.
    #[serde(rename = "order_book_visible")]
    pub order_book_visible: bool,
    /// The share of the body the order book takes.
    #[serde(rename = "order_book_ratio")]
    pub order_book_ratio: f32,
    /// The book's price grouping, as a multiple of the venue's own tick.
    #[serde(rename = "order_book_grouping")]
    pub order_book_grouping: u32,
}

impl Default for PredictionMarketConfig {
    fn default() -> Self {
        Self {
            range: PredictionRange::default(),
            order_book_visible: true,
            order_book_ratio: DEFAULT_BOOK_RATIO,
            order_book_grouping: GROUPING_MULTIPLIERS[0],
        }
    }
}

impl PredictionMarketConfig {
    /// Settles the book's two drawing choices against what this build offers.
    ///
    /// A stored ratio or grouping can stop being valid — a layout written by
    /// another build, a hand-edited file — and such a choice goes back to the
    /// default one rather than being drawn at a value nobody picked. The default
    /// is not in the range for its own sake: the ratio is what a splitter
    /// clamps, and the grouping is what the venue's tick can express. Answers
    /// whether anything was replaced, which is what the pane reports so the
    /// replacement is written back.
    fn settle(&mut self) -> bool {
        let defaults = Self::default();
        let mut settled = false;
        if clamp_ratio(self.order_book_ratio) != self.order_book_ratio {
            self.order_book_ratio = defaults.order_book_ratio;
            settled = true;
        }
        if !GROUPING_MULTIPLIERS.contains(&self.order_book_grouping) {
            self.order_book_grouping = defaults.order_book_grouping;
            settled = true;
        }
        settled
    }
}

impl WidgetConfig for PredictionMarketConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The whole breathing effect at the newest point: one 700 ms cycle.
const PULSE_DURATION: Duration = Duration::from_millis(700);

/// The endpoint breathing effect, started by a live reading that advanced the newest
/// point.
///
/// The phase is derived from how long the pulse has been running rather than stored
/// frame by frame, so a frame that arrives late draws the phase that is actually true
/// and the effect still ends on time.
#[derive(Clone, Copy, Debug)]
struct Pulse {
    started: Instant,
}

impl Pulse {
    /// The phase in `0..1`, or `None` once the effect has ended.
    fn phase(&self, now: Instant) -> Option<f32> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= PULSE_DURATION {
            return None;
        }
        Some(elapsed.as_secs_f32() / PULSE_DURATION.as_secs_f32())
    }
}

/// The pointer position of one frame.
///
/// Mouse moves arrive faster than frames, so only the newest position is kept: what
/// the frame does with it is idempotent, and applying each event would repaint
/// several times per frame.
#[derive(Default)]
struct PointerFrameState {
    pending: Option<Point<Pixels>>,
    scheduled: bool,
}

impl PointerFrameState {
    /// Queues `position`, and answers whether this is the call that has to schedule
    /// the frame.
    fn queue(&mut self, position: Point<Pixels>) -> bool {
        self.pending = Some(position);
        if self.scheduled {
            return false;
        }
        self.scheduled = true;
        true
    }

    /// Takes the queued position, marking the frame as applied.
    fn take(&mut self) -> Option<Point<Pixels>> {
        self.scheduled = false;
        self.pending.take()
    }

    /// Drops a position queued for a frame that no longer has anything to draw.
    fn clear_pending(&mut self) {
        self.pending = None;
    }
}

/// The height of the splitter between the chart and the book.
const BOOK_SPLITTER_HEIGHT: Pixels = px(7.0);

/// The shortest the book panel is drawn, so a short pane still shows its
/// failure and the way back rather than a clipped strip of both.
const MIN_BOOK_HEIGHT: Pixels = px(56.0);

/// Where a splitter drag started.
///
/// A drag is a gesture, not a value being moved: what it carries is where the
/// pointer went down, which is what every later pointer position is measured
/// against. It lives in the widget rather than in the drag value because GPUI
/// hands `on_drag_move` the value the drag started with.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SplitDragStart {
    /// The book's share of the chart-and-book area when the drag started.
    start_ratio: f32,
    /// The pointer's window position when the drag started.
    start_y: Pixels,
}

/// The preview entity a splitter drag carries.
///
/// The gesture's origin is widget state, so the preview only exists because
/// [`InteractiveElement::on_drag`] has to name a type whose drag is followed by
/// `on_drag_move`; it draws nothing.
#[derive(Clone, Copy)]
struct BookSplitterDrag;

impl Render for BookSplitterDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The prediction market.
pub struct PredictionMarketWidget {
    config: PredictionMarketConfig,
    config_change: WidgetConfigChange,
    selection: Option<PredictionEventRef>,
    selection_title: Option<SmolStr>,
    market: Option<PredictionMarketRef>,
    binding: WidgetBinding<PredictionMarketProtocol>,
    /// The book the panel is following, which is one leg of the drawn market.
    book_binding: WidgetBinding<PredictionOrderBookProtocol>,
    book_snapshot: Option<Arc<PredictionOrderBook>>,
    book_error: Option<PredictionOrderBookError>,
    book_key: Option<PredictionOrderBookStreamKey>,
    book_epoch: u64,
    /// The panel's own state: the grouped ladder, the scroll place a reader left
    /// it at and the depth bars between two books.
    book_panel: PredictionBookPanel,
    /// Where the chart-and-book area was last drawn, which is what a splitter drag
    /// divides by.
    split_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Where the pointer went down on the splitter, while a gesture is running.
    split_drag: Option<SplitDragStart>,
    /// Whether that gesture has moved the notch, which is what makes the release
    /// worth writing back.
    split_dragging: bool,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    snapshot: Option<Arc<PredictionMarketSnapshot>>,
    /// The frames the chart has prepared, shared with the canvas element that is
    /// rebuilt every frame.
    chart_caches: Rc<ChartPaintCaches>,
    /// The pointer in window coordinates while it is over the chart, which is what
    /// the canvas snaps to a sample.
    pointer: Option<Point<Pixels>>,
    pointer_frame: PointerFrameState,
    /// The breathing effect around the endpoints, while it is running.
    pulse: Option<Pulse>,
    locale: Option<SmolStr>,
    /// The local epoch of the stream currently requested.
    epoch: u64,
    awaiting_market: bool,
    _venues: Subscription,
    _time_zone: Option<Subscription>,
}

impl WidgetView for PredictionMarketWidget {
    type Config = PredictionMarketConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.prediction_market");
    const DISPLAY_NAME: &'static str = "widget.names.prediction_market";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "Prediction event probabilities over time";
    const ICON: Option<&'static str> = Some("widgets/prediction_market.png");

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

    fn load(&mut self, cx: &mut Context<Self>) {
        // The stored book choices are judged before any work is started, so the
        // pane never draws or requests a notch and a grouping this build does
        // not offer.
        if self.config.settle() {
            log::warn!(
                "the prediction market's stored book ratio or grouping is not offered here; \
                 it was reset to the default"
            );
            self.config_change.publish(cx);
        }
        if !self.market_ready(cx) {
            self.awaiting_market = true;
            self.snapshot = None;
            self.forget_chart_state();
            // The chart cannot be drawn for a market nothing holds any more, and
            // neither can its book: the stream the panel was reading is retired
            // with the reading it belonged to.
            self.clear_book(cx);
            self.report_notice(cx);
            return;
        }
        self.awaiting_market = false;
        let Some(selection) = self.selection.clone() else {
            self.status.report(
                WidgetState::waiting(Notice::message(
                    "widget.prediction_market.select_event_prompt",
                )),
                cx,
            );
            return;
        };
        self.locale = cx
            .try_global::<ConfigManager>()
            .and_then(|manager| polymarket_locale(manager.read().language));
        self.epoch = self.epoch.wrapping_add(1);
        let retry = self.status.is_failed(cx);
        self.status.report(WidgetState::Connecting, cx);
        let request = if retry {
            PredictionMarketRequest::Retry {
                event: selection,
                market: self.market.clone(),
                range: self.config.range,
                locale: self.locale.clone(),
                epoch: self.epoch,
            }
        } else {
            PredictionMarketRequest::Load {
                event: selection,
                market: self.market.clone(),
                range: self.config.range,
                locale: self.locale.clone(),
                epoch: self.epoch,
            }
        };
        self.binding.call(request, cx, Self::apply_load_result);
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some() && !self.awaiting_market
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        if self.awaiting_market || !self.market_ready(cx) {
            return WidgetHeader::empty();
        }
        let widget = cx.entity().downgrade();
        let label = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.title.clone())
            .or_else(|| self.selection_title.clone());
        let event_button = PredictionEventButton::new(
            "PredictionMarketEvent",
            self.selection.clone(),
            label,
            "widget.prediction_market.select_event",
            move |event, title, app| {
                let _ = widget.update(app, |this, cx| this.select(event, title, cx));
            },
        )
        .into_any_element();
        let mut left = div().flex().items_center().gap_1().child(event_button);
        if let Some(snapshot) = self.snapshot.as_ref()
            && snapshot.markets.len() > 1
            && let Some(selected) = snapshot.selected_choice(self.market.as_ref())
        {
            let groups = snapshot.market_groups();
            if groups.len() > 1 {
                left = left.child(self.market_group_select(snapshot, &groups, &selected.group, cx));
            }
            left = left.child(self.market_select(snapshot, selected, cx));
        }
        // The grouping belongs between the toggle and the timeframe, and only a
        // book on screen has a tick to group by.
        let book_supported = self.supported_book_target().is_some();
        let mut right = div().flex().items_center().gap_1();
        if book_supported {
            right = right.child(self.book_toggle(cx));
        }
        if book_supported && self.config.order_book_visible && self.has_grouping_choice() {
            right = right.child(self.book_grouping_select(cx));
        }
        WidgetHeader::empty()
            .left(left)
            .right(right.child(self.range_select(cx)))
    }
}

impl PredictionMarketWidget {
    /// The seam a test answers through.
    pub(crate) fn new_with_source<S>(
        config: PredictionMarketConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: PredictionMarketSource + PredictionOrderBookSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            PredictionMarketService::new(source.clone()),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let book_binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            PredictionOrderBookService::new(source),
            cx,
            Self::apply_book_reliable,
            Self::apply_book_latest,
        );
        let venues = cx.global::<MarketParticipantsConnector>().venues().events();
        let subscription = cx.subscribe(&venues, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        });
        Self {
            config,
            config_change,
            selection: None,
            selection_title: None,
            market: None,
            binding,
            book_binding,
            book_snapshot: None,
            book_error: None,
            book_key: None,
            book_epoch: 0,
            book_panel: PredictionBookPanel::new(),
            split_bounds: Rc::new(Cell::new(None)),
            split_drag: None,
            split_dragging: false,
            status,
            recovery,
            snapshot: None,
            chart_caches: Rc::new(ChartPaintCaches::default()),
            pointer: None,
            pointer_frame: PointerFrameState::default(),
            pulse: None,
            locale: None,
            epoch: 0,
            awaiting_market: false,
            _venues: subscription,
            _time_zone: notify_on_data_time_zone_change(cx),
        }
    }

    fn market_group_select(
        &self,
        snapshot: &PredictionMarketSnapshot,
        groups: &[&PredictionMarketGroup],
        selected: &PredictionMarketGroup,
        cx: &Context<Self>,
    ) -> Select {
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "PredictionMarketGroupSelect",
        )))
        .placeholder(SharedString::new(selected.label.clone()))
        .ghost()
        .small()
        .max_w(px(200.0))
        .menu_max_h(px(600.0))
        .selected_item(Some(SharedString::new(selected.key.as_str())))
        .aria_label(t!(cx, "widget.prediction_market.select_market_group"));
        for group in groups {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new(group.key.as_str()),
                SharedString::new(group.label.clone()),
            )));
        }
        let choices = snapshot.markets.clone();
        select.on_change({
            let widget = cx.entity();
            move |id, _, app| {
                let Some(id) = id.as_deref() else {
                    return;
                };
                let Some(market) = choices
                    .iter()
                    .find(|choice| choice.group.key.as_str() == id)
                    .map(|choice| choice.market.clone())
                else {
                    return;
                };
                widget.update(app, |this, cx| this.select_market(market, cx));
            }
        })
    }

    fn market_select(
        &self,
        snapshot: &PredictionMarketSnapshot,
        selected: &PredictionMarketChoice,
        cx: &Context<Self>,
    ) -> Select {
        let selected_id = SharedString::new(selected.market.native_market.as_str());
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "PredictionMarketMarketSelect",
        )))
        .placeholder(SharedString::new(selected.label.clone()))
        .ghost()
        .small()
        .max_w(px(320.0))
        .menu_max_h(px(600.0))
        .selected_item(Some(selected_id))
        .aria_label(t!(cx, "widget.prediction_market.select_market"));
        for market in snapshot.markets_in_group(&selected.group) {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new(market.market.native_market.as_str()),
                SharedString::new(market.label.clone()),
            )));
        }
        let choices = snapshot.markets.clone();
        let group_key = selected.group.key.clone();
        select.on_change({
            let widget = cx.entity();
            move |id, _, app| {
                let Some(id) = id.as_deref() else {
                    return;
                };
                let Some(market) = choices
                    .iter()
                    .find(|choice| {
                        choice.group.key == group_key && choice.market.native_market.as_str() == id
                    })
                    .map(|choice| choice.market.clone())
                else {
                    return;
                };
                widget.update(app, |this, cx| this.select_market(market, cx));
            }
        })
    }

    /// The six ranges, as the header's timeframe picker.
    fn range_select(&self, cx: &Context<Self>) -> AnyElement {
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "PredictionMarketRange",
        )))
        .placeholder(range_label(self.config.range, cx))
        .ghost()
        .small()
        .aria_label(t!(cx, "widget.prediction_market.timeframe"))
        .selected_item(Some(SharedString::new_static(range_id(self.config.range))));
        for range in PredictionRange::ALL {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new_static(range_id(range)),
                range_label(range, cx),
            )));
        }
        div()
            .debug_selector(|| "TEST_PREDICTION_MARKET_RANGE".into())
            .occlude()
            .child(select.on_change({
                let widget = cx.entity();
                move |id, _, app| {
                    let Some(range) = id.as_deref().and_then(range_of_id) else {
                        return;
                    };
                    widget.update(app, |this, cx| this.select_range(range, cx));
                }
            }))
            .into_any_element()
    }

    /// The book's price grouping, as the header's picker.
    ///
    /// The choices are the steps this book's tick can express, and the settled one
    /// is what the picker shows: a stored grouping the tick cannot express is
    /// never offered, so the placeholder is the step the ladder was actually built
    /// at.
    fn book_grouping_select(&self, cx: &Context<Self>) -> AnyElement {
        let Some(book) = self.book_snapshot.as_ref() else {
            return div().into_any_element();
        };
        let settled = settle_grouping(book.tick_size, self.config.order_book_grouping);
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "PredictionMarketBookGrouping",
        )))
        .placeholder(self.book_grouping_label().unwrap_or_default())
        .ghost()
        .small()
        .aria_label(t!(cx, "widget.prediction_market.book.price_grouping"))
        .selected_item(Some(SharedString::new(format_smolstr!("{settled}"))));
        for (multiplier, label) in grouping_steps(book.tick_size) {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new(format_smolstr!("{multiplier}")),
                SharedString::new(label),
            )));
        }
        div()
            .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_GROUPING".into())
            .occlude()
            .child(select.on_change({
                let widget = cx.entity();
                move |id, _, app| {
                    let Some(multiplier) = id.as_deref().and_then(|id| id.parse::<u32>().ok())
                    else {
                        return;
                    };
                    widget.update(app, |this, cx| this.select_book_grouping(multiplier, cx));
                }
            }))
            .into_any_element()
    }

    /// The label of the grouping the ladder is built at.
    ///
    /// A tick this build cannot group at all is named by the tick itself, which is
    /// the step the book is drawn at then.
    fn book_grouping_label(&self) -> Option<SharedString> {
        let book = self.book_snapshot.as_ref()?;
        let settled = settle_grouping(book.tick_size, self.config.order_book_grouping);
        let label = grouping_steps(book.tick_size)
            .into_iter()
            .find(|(multiplier, _)| *multiplier == settled)
            .map_or_else(|| format_smolstr!("{}", book.tick_size), |(_, label)| label);
        Some(SharedString::new(label))
    }

    /// Whether the panel's book offers a grouping at all.
    ///
    /// A picker is a choice only when there is more than one step to pick and a
    /// level to group: a tick of one share or more expresses no step at all, and
    /// an empty book has nothing to regroup.
    fn has_grouping_choice(&self) -> bool {
        let Some(book) = self.book_snapshot.as_ref() else {
            return false;
        };
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
        has_levels && grouping_steps(book.tick_size).len() > 1
    }

    /// Whether this build can read the order book of `event`'s venue.
    ///
    /// A venue can publish prediction events without publishing their books:
    /// Hyperliquid's outcome events are chartable, but only Polymarket's CLOB
    /// answers a book request. Asking an unsupported venue would turn every
    /// reading into a failure the user cannot retry away.
    fn book_supported(event: &PredictionEventRef) -> bool {
        Self::book_venues().contains(&event.route.venue)
    }

    /// The venues this build can read an order book from.
    ///
    /// The table is static, so it is resolved once rather than rebuilt for every
    /// header and body frame that asks whether the drawn event has a book.
    fn book_venues() -> &'static [VenueId] {
        static VENUES: OnceLock<Vec<VenueId>> = OnceLock::new();
        VENUES.get_or_init(|| {
            market_participants_connector::venue::definition::venues_supporting(
                CapabilityId::PredictionOrderBook,
            )
        })
    }

    /// The book the drawn reading points at, when this build can read it.
    fn supported_book_target(&self) -> Option<PredictionOrderBookTarget> {
        let target = self.snapshot.as_ref()?.book_target()?;
        Self::book_supported(&target.event).then_some(target)
    }

    /// Regroups the book on screen at `multiplier`, and reports the choice.
    fn select_book_grouping(&mut self, multiplier: u32, cx: &mut Context<Self>) {
        let Some(book) = self.book_snapshot.as_ref() else {
            return;
        };
        let settled = settle_grouping(book.tick_size, multiplier);
        if settled == self.config.order_book_grouping {
            return;
        }
        self.config.order_book_grouping = settled;
        self.config_change.publish(cx);
        self.book_panel.set_multiplier(settled);
        cx.notify();
    }

    fn select(&mut self, event: PredictionEventRef, title: SmolStr, cx: &mut Context<Self>) {
        self.selection = Some(event);
        self.selection_title = Some(title);
        self.market = None;
        self.snapshot = None;
        self.clear_book(cx);
        self.clear_pointer(cx);
        self.pulse = None;
        self.load(cx);
        cx.notify();
    }

    fn select_market(&mut self, market: PredictionMarketRef, cx: &mut Context<Self>) {
        if self.market.as_ref() == Some(&market) && !self.status.is_failed(cx) {
            return;
        }
        self.market = Some(market);
        self.snapshot = None;
        self.clear_book(cx);
        self.clear_pointer(cx);
        self.pulse = None;
        self.load(cx);
        cx.notify();
    }

    /// Switches the range the chart is read over.
    ///
    /// The reading on screen answers the window the user just left, so it is dropped
    /// before the request: the pane has no content while the new reading is fetched,
    /// and the framework draws its connecting spinner in the body.
    fn select_range(&mut self, range: PredictionRange, cx: &mut Context<Self>) {
        if self.config.range == range {
            return;
        }
        self.config.range = range;
        self.config_change.publish(cx);
        self.snapshot = None;
        self.forget_chart_state();
        self.load(cx);
        cx.notify();
    }

    fn current_key(&self) -> Option<PredictionMarketStreamKey> {
        self.selection
            .as_ref()
            .map(|event| PredictionMarketStreamKey {
                event: event.clone(),
                market: self.market.clone(),
                range: self.config.range,
                locale: self.locale.clone(),
                epoch: self.epoch,
            })
    }

    fn market_ready(&self, cx: &App) -> bool {
        let venues = cx.global::<MarketParticipantsConnector>().venues();
        // A venue being loaded is not enough: Hyperliquid loads its core scopes
        // even when its outcome metadata published no prediction scope. Gate on
        // a scope that actually declares the capability the selector reads.
        if !venues.entries().iter().any(|(route, entry)| {
            Self::listed_venues().contains(&route.venue)
                && entry
                    .capabilities
                    .iter()
                    .any(|support| support.capability == CapabilityId::PredictionEvents)
        }) {
            return false;
        }
        let Some(event) = &self.selection else {
            return true;
        };
        venues.scope(&event.route).is_some()
    }

    /// Reports that the market this chart needs is not loaded.
    ///
    /// Only the not-loaded path calls this: a pane with no event selected is
    /// told to select one by `load` itself.
    fn report_notice(&self, cx: &mut Context<Self>) {
        let venues = self
            .selection
            .as_ref()
            .map(|event| vec![event.route.venue])
            .unwrap_or_else(Self::listed_venues);
        self.status.report(
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues,
                supported: Self::listed_venues(),
            }),
            cx,
        );
    }

    fn listed_venues() -> Vec<VenueId> {
        market_participants_connector::venue::definition::venues_supporting(
            CapabilityId::PredictionEvents,
        )
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<PredictionMarketProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(PredictionMarketResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(PredictionMarketError::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    /// Forgets the book on screen and retires the stream that fed it.
    ///
    /// Another market's book cannot be vouched for, and the epoch means a
    /// message from the stream that was reading it is refused even if the same
    /// leg is chosen again later. The stream itself is stopped rather than left
    /// publishing into a channel nobody draws: a hidden or repointed panel holds
    /// no lease on the venue.
    fn clear_book(&mut self, cx: &mut Context<Self>) {
        self.stop_book(cx);
        self.book_snapshot = None;
        self.book_error = None;
        self.book_epoch = self.book_epoch.wrapping_add(1);
        self.book_panel.clear();
    }

    /// Retires the book stream, keeping whatever the panel is drawing.
    ///
    /// Hiding, clearing or repointing the book aborts a splitter gesture that is
    /// running: the bar the gesture was measured on is gone, and the window-wide
    /// release listener must not write a notch the user never finished choosing.
    fn stop_book(&mut self, cx: &mut Context<Self>) {
        self.split_drag = None;
        self.split_dragging = false;
        self.book_binding
            .call(PredictionOrderBookRequest::Stop, cx, Self::apply_book_stop);
        self.book_key = None;
    }

    /// Points the book panel at the market the chart is drawing.
    ///
    /// The request carries every side of the drawn market in venue order, which
    /// is what makes it share the connection lease the chart's own subscription
    /// already holds. A reading of the same event and leg is the book already on
    /// screen, so switching the chart range does not restart it.
    ///
    /// Nothing is asked for while the book is hidden, and nothing is asked of a
    /// venue that cannot answer: a hidden panel starts its stream when it is
    /// shown, and a chart whose venue publishes no book never asks for one.
    fn sync_book_target(&mut self, cx: &mut Context<Self>) {
        if !self.config.order_book_visible {
            return;
        }
        let Some(target) = self.supported_book_target() else {
            // A reading with no book to point at, or one whose venue this build
            // cannot read: the panel keeps nothing it can no longer draw.
            if self.book_key.is_some() || self.book_snapshot.is_some() || self.book_error.is_some()
            {
                self.clear_book(cx);
                cx.notify();
            }
            return;
        };
        if self
            .book_key
            .as_ref()
            .is_some_and(|key| key.event == target.event && key.leg == target.leg)
        {
            return;
        }
        // Showing the book again is instant: the ladder of the same leg is still
        // drawn while the new request runs, and only another leg replaces it.
        let same_ladder = self
            .book_snapshot
            .as_ref()
            .is_some_and(|book| book.leg == target.leg);
        self.book_epoch = self.book_epoch.wrapping_add(1);
        if !same_ladder {
            self.book_snapshot = None;
            self.book_error = None;
            self.book_panel.clear();
        }
        self.book_key = Some(PredictionOrderBookStreamKey {
            event: target.event.clone(),
            leg: target.leg.clone(),
            epoch: self.book_epoch,
        });
        self.book_binding.call(
            PredictionOrderBookRequest::Load {
                target,
                epoch: self.book_epoch,
            },
            cx,
            Self::apply_book_rpc,
        );
        cx.notify();
    }

    /// Reports a book command the service never answered.
    ///
    /// The service answers every command before it does any work, so a failed
    /// command is a failure of the channel rather than of the book: the book's
    /// own outcome arrives as a stream status, which is what normally reports a
    /// failure.
    fn apply_book_rpc(
        &mut self,
        result: WidgetRpcResult<PredictionOrderBookProtocol>,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = result {
            log::warn!("the prediction order book request failed: {error}");
            self.book_error = Some(PredictionOrderBookError::Rpc(error));
            cx.notify();
        }
    }

    /// Ignores the answer to a stop.
    ///
    /// A stop is housekeeping, not a reading: whether the service was already
    /// closed is not something the panel reports, because the book it draws is
    /// gone on purpose.
    fn apply_book_stop(
        &mut self,
        _result: WidgetRpcResult<PredictionOrderBookProtocol>,
        _cx: &mut Context<Self>,
    ) {
    }

    fn apply_book_reliable(
        &mut self,
        key: PredictionOrderBookStreamKey,
        message: Arc<PredictionOrderBookStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.book_key.as_ref() != Some(&key) || self.status.is_failed(cx) || self.awaiting_market
        {
            return false;
        }
        let PredictionOrderBookStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        let changed = match status {
            PredictionOrderBookStatus::Connecting => {
                let changed = self.book_error.is_some();
                self.book_error = None;
                changed
            }
            PredictionOrderBookStatus::Ready => false,
            PredictionOrderBookStatus::Failed { error } => {
                let changed = self.book_error.as_ref() != Some(error);
                self.book_error = Some(error.clone());
                changed
            }
        };
        // A hidden panel keeps the outcome for the moment it is shown again, but
        // never asks for a repaint of a body that does not draw it.
        changed && self.config.order_book_visible
    }

    fn apply_book_latest(
        &mut self,
        key: PredictionOrderBookStreamKey,
        message: Arc<PredictionOrderBookStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.book_key.as_ref() != Some(&key) || self.status.is_failed(cx) || self.awaiting_market
        {
            return false;
        }
        let PredictionOrderBookStreamMessage::Snapshot(book) = message.as_ref() else {
            return false;
        };
        // A failure is the answer until a retry starts a new generation: a
        // snapshot of the failed stream that arrives late describes the book the
        // panel already gave up on, and drawing it would show stale data with no
        // way back.
        if self.book_error.is_some() {
            return false;
        }
        // The stored grouping is settled against the tick this book published, not
        // against the config alone: which steps exist depends on the tick, and a
        // grouping the venue can no longer express goes back to the finest one. An
        // empty page has no level to group, and the tick it is stated at is a
        // fallback rather than the venue's, so it must not rewrite a grouping the
        // user chose on a book that does have levels.
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
        let multiplier = if has_levels {
            let settled = settle_grouping(book.tick_size, self.config.order_book_grouping);
            if settled != self.config.order_book_grouping {
                self.config.order_book_grouping = settled;
                self.config_change.publish(cx);
            }
            settled
        } else {
            self.config.order_book_grouping
        };
        self.book_snapshot = Some(book.clone());
        self.book_panel.set_leg_label(self.book_leg_label());
        self.book_panel.set_book(book.clone(), multiplier);
        // The state is stored while the panel is hidden — showing it again draws
        // the newest ladder at once — but a hidden body is not repainted.
        self.config.order_book_visible
    }

    /// Re-reads the book the panel is pointed at.
    ///
    /// A retry is a new generation of the same book: the error stops being the
    /// answer, and the reading arrives under a key no message of the failed
    /// stream can name. The ladder the reader was reading stays on screen while
    /// the new reading lands; only a panel that never had one shows loading.
    fn retry_book(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.supported_book_target() else {
            return;
        };
        self.book_epoch = self.book_epoch.wrapping_add(1);
        self.book_error = None;
        self.book_key = Some(PredictionOrderBookStreamKey {
            event: target.event.clone(),
            leg: target.leg.clone(),
            epoch: self.book_epoch,
        });
        self.book_binding.call(
            PredictionOrderBookRequest::Retry {
                target,
                epoch: self.book_epoch,
            },
            cx,
            Self::apply_book_rpc,
        );
        cx.notify();
    }

    /// Whether the panel is waiting for its first reading.
    fn book_loading(&self) -> bool {
        self.book_key.is_some() && self.book_snapshot.is_none() && self.book_error.is_none()
    }

    /// Takes the book panel out of the body and reports the change.
    ///
    /// A hidden panel holds no stream: the last ladder stays in memory so
    /// showing it again is instant, but the book is not read while nothing draws
    /// it. Showing it starts the reading the chart's own target already names.
    fn toggle_book(&mut self, cx: &mut Context<Self>) {
        self.config.order_book_visible = !self.config.order_book_visible;
        if self.config.order_book_visible {
            self.sync_book_target(cx);
        } else {
            self.stop_book(cx);
        }
        self.config_change.publish(cx);
        // The toggle itself changed shape even when no book request was started.
        cx.notify();
    }

    /// The header control that shows and hides the book.
    fn book_toggle(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();
        let label = t!(cx, "widget.prediction_market.book.title");
        div()
            .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_TOGGLE".into())
            // A control in the header row has to take its own clicks: the row is
            // the pane's drag area as well.
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "PredictionMarketBookToggle",
                )))
                .ghost()
                .small()
                .selected(self.config.order_book_visible)
                .aria_label(label.clone())
                .child(label)
                .on_click(move |_, _, cx| {
                    if let Some(widget) = widget.upgrade() {
                        widget.update(cx, |this, cx| this.toggle_book(cx));
                    }
                }),
            )
            .into_any_element()
    }

    fn apply_reliable(
        &mut self,
        key: PredictionMarketStreamKey,
        message: Arc<PredictionMarketStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let PredictionMarketStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        match status {
            PredictionMarketStatus::Connecting => self.status.report(WidgetState::Connecting, cx),
            PredictionMarketStatus::Ready { .. } => {
                if self.snapshot.is_some() {
                    self.status.report(WidgetState::Ready, cx);
                }
            }
            PredictionMarketStatus::Failed { error } => {
                if let PredictionMarketFetchError::Market(market) = error
                    && (market_is_catalog_not_loaded(market) || market.is_venue_not_loaded())
                {
                    self.awaiting_market = true;
                    self.snapshot = None;
                    self.forget_chart_state();
                    // The market the book was read for is not in memory: the book
                    // stream is retired with the reading it belonged to.
                    self.clear_book(cx);
                    self.report_notice(cx);
                    cx.notify();
                    return false;
                }
                // A terminal chart failure is the pane's own: the chart's stream is
                // gone, so the book's lease is released too. The last ladder stays on
                // screen — it is what the reader was reading — but nothing reads it
                // any more.
                self.stop_book(cx);
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(
                        Arc::new(PredictionMarketError::Fetch(error.clone())),
                        recovery,
                    ),
                    cx,
                );
            }
        }
        false
    }

    fn apply_latest(
        &mut self,
        key: PredictionMarketStreamKey,
        message: Arc<PredictionMarketStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let PredictionMarketStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        if self
            .snapshot
            .as_ref()
            .is_some_and(|held| snapshot.revision < held.revision)
        {
            return false;
        }
        let newest_before = self
            .snapshot
            .as_ref()
            .and_then(|held| held.latest_timestamp());
        let newest = snapshot.latest_timestamp();
        self.snapshot = Some(snapshot.clone());
        self.selection_title = Some(snapshot.title.clone());
        // The book follows the market the reading draws, so a switch of market
        // re-points it here rather than in the header's handler: only a reading
        // knows which sides the market it fell back to actually publishes.
        self.sync_book_target(cx);
        self.status.report(WidgetState::Ready, cx);
        // The pulse marks a *new observation*, not a first reading: a chart opening on
        // a market has nothing to breathe about, and a reading whose newest point did
        // not move is the same moment drawn again. A tick that lands while a breath is
        // still running does not restart it: a venue pushing several ticks a second
        // would otherwise pin the effect to its first phase, which reads as a static
        // halo rather than a pulse.
        let now = Instant::now();
        let breathing = self.pulse.is_some_and(|pulse| pulse.phase(now).is_some());
        if !cx.reduce_motion()
            && !breathing
            && newest_before
                .is_some_and(|previous| newest.is_some_and(|current| current > previous))
        {
            self.pulse = Some(Pulse { started: now });
        }
        true
    }

    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.route.venue == event.venue)
        {
            // A refresh replaces the catalog generation the chart and the book
            // stream were opened against. Neither can be vouched for any more, and
            // the book has to be asked for again under the reloaded catalog: the
            // next chart reading re-points it at the same leg and starts it over.
            self.snapshot = None;
            self.forget_chart_state();
            self.clear_book(cx);
            self.load(cx);
            cx.notify();
            return;
        }
        if !self.market_ready(cx) {
            self.awaiting_market = true;
            self.snapshot = None;
            self.forget_chart_state();
            // The chart cannot be drawn for a market nothing holds any more, and
            // neither can its book: the stream the panel was reading is retired
            // with the reading it belonged to.
            self.clear_book(cx);
            self.report_notice(cx);
            cx.notify();
            return;
        }
        if self.awaiting_market {
            self.load(cx);
        }
    }

    /// Queues the newest pointer position and schedules the one frame that applies it.
    fn queue_mouse_move(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.pointer_frame.queue(position) {
            return;
        }
        cx.on_next_frame(window, |this, _, cx| {
            let Some(position) = this.pointer_frame.take() else {
                return;
            };
            if this.pointer != Some(position) {
                this.pointer = Some(position);
                cx.notify();
            }
        });
    }

    /// Forgets the hovered position, scheduling nothing.
    fn clear_pointer(&mut self, cx: &mut Context<Self>) {
        self.pointer_frame.clear_pending();
        if self.pointer.take().is_some() {
            cx.notify();
        }
    }

    /// Drops everything derived from a reading the pane can no longer vouch for.
    ///
    /// A pointer is a position in a chart that is about to be replaced or removed;
    /// left in place, the next reading would draw a crosshair over a sample that no
    /// longer exists. The prepared chart goes too: its key begins with the snapshot's
    /// identity, so it can never be reused, and keeping it would hold the invisible
    /// reading's geometry alive.
    fn forget_chart_state(&mut self) {
        self.pointer = None;
        self.pointer_frame.clear_pending();
        self.pulse = None;
        self.chart_caches = Rc::new(ChartPaintCaches::default());
    }

    /// The legend above the chart: one dot, a name and its latest integer percentage
    /// per series, in the order the venue published the sides.
    ///
    /// The order is the snapshot's, and it is also what the chart colours are assigned
    /// from, so a reload cannot shuffle either.
    fn legend(&self, snapshot: &PredictionMarketSnapshot, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let palette = &theme.roles.chart.series;
        let mut row = div()
            .debug_selector(|| "TEST_PREDICTION_MARKET_LEGEND".into())
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_2()
            .py_1();
        for line in snapshot.series.iter() {
            let color = palette[line.color % palette.len()].hsla;
            let text = match line.latest {
                Some(latest) => format_smolstr!(
                    "{} {:.0}%",
                    line.label,
                    latest.ratio().inner() as f64 / 1e16
                ),
                None => line.label.clone(),
            };
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().size(px(8.0)).rounded_full().bg(color))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.roles.fg.muted.hsla)
                            .child(SharedString::new(text)),
                    ),
            );
        }
        if snapshot.omitted > 0 {
            row = row.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "widget.prediction_market.more_lines",
                        count = snapshot.omitted
                    )),
            );
        }
        row.into_any_element()
    }

    /// The book below the chart: the full ladder while there is one, and the
    /// region that says why there is not.
    ///
    /// The panel sits below the splitter in the chart-and-book area, so it takes
    /// a fixed share of that area and the chart keeps the rest; a short pane still
    /// gives it [`MIN_BOOK_HEIGHT`], because a failure and its way back have to
    /// fit. The panel owns its own padding so its column header, its rows and
    /// their depth bars share one content edge; this wrapper only gives it the
    /// share, and a compact error strip above it when the book failed after a
    /// reading was already drawn.
    fn order_book_panel(&mut self, cx: &Context<Self>) -> AnyElement {
        let panel = div()
            .id(ElementId::Name(SharedString::new_static(
                "PredictionMarketBookPanel",
            )))
            .role(Role::Group)
            .aria_label(t!(cx, "widget.prediction_market.book.title"))
            .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK".into())
            .flex_none()
            .h(relative(clamp_ratio(self.config.order_book_ratio)))
            .min_h(MIN_BOOK_HEIGHT)
            .overflow_hidden()
            .flex()
            .flex_col();
        if self.book_snapshot.is_none() {
            let body = match self.book_error.clone() {
                Some(error) => self.book_failure(&error, cx),
                None if self.book_loading() => centered_muted(t!(cx, "common.loading"), cx),
                None => empty_state(cx),
            };
            return panel.child(body).into_any_element();
        }
        let ladder = self.book_panel.render(cx);
        match self.book_error.clone() {
            Some(error) => panel
                .child(self.book_error_strip(&error, cx))
                .child(div().flex_1().min_h_0().child(ladder))
                .into_any_element(),
            None => panel
                .child(div().flex_1().min_h_0().child(ladder))
                .into_any_element(),
        }
    }

    /// The one-line failure a book that was already on screen can leave behind.
    ///
    /// The last reading is still the ladder the user was reading, so it stays; the
    /// sentence and the way back sit above it instead of replacing it.
    fn book_error_strip(&self, error: &PredictionOrderBookError, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();
        div()
            .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_ERROR".into())
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap_1()
            .px_2()
            .py_1()
            .text_size(px(11.0))
            .text_color(cx.theme().roles.fg.muted.hsla)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(error.localized_message(cx)),
            )
            .child(
                div()
                    .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_RETRY".into())
                    .occlude()
                    .child(
                        Button::new(ElementId::Name(SharedString::new_static(
                            "PredictionMarketBookRetry",
                        )))
                        .ghost()
                        .small()
                        .child(t!(cx, "common.retry"))
                        .on_click(move |_, _, cx| {
                            if let Some(widget) = widget.upgrade() {
                                widget.update(cx, |this, cx| this.retry_book(cx));
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The label of the side the panel is reading, from the venue's own list.
    fn book_leg_label(&self) -> Option<SmolStr> {
        let (Some(key), Some(snapshot)) = (self.book_key.as_ref(), self.snapshot.as_ref()) else {
            return None;
        };
        snapshot
            .legs
            .iter()
            .find(|leg| leg.leg == key.leg)
            .map(|leg| leg.label.clone())
    }

    /// The panel's failure: the sentence the error type states, and a way back.
    ///
    /// The two sit as close as they can: at the smallest share of a short pane
    /// this region is all the room there is, and a clause of padding is a clipped
    /// button.
    fn book_failure(&self, error: &PredictionOrderBookError, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_0p5()
            .px_2()
            .py_0p5()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(cx.theme().roles.fg.muted.hsla)
                    .child(error.localized_message(cx)),
            )
            .child(
                div()
                    .debug_selector(|| "TEST_PREDICTION_MARKET_BOOK_RETRY".into())
                    .occlude()
                    .child(
                        Button::new(ElementId::Name(SharedString::new_static(
                            "PredictionMarketBookRetry",
                        )))
                        .ghost()
                        .small()
                        .child(t!(cx, "common.retry"))
                        .on_click(move |_, _, cx| {
                            if let Some(widget) = widget.upgrade() {
                                widget.update(cx, |this, cx| this.retry_book(cx));
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The strip between the chart and the book, which gives the book more or
    /// less of the area they share.
    ///
    /// The bar is a hit area rather than a painted control: it is a one-pixel line
    /// with a seven-pixel grab band around it. The gesture's origin is recorded on
    /// the press, because the value GPUI hands `on_drag_move` is the one the drag
    /// started with: measuring against it would treat every drag as starting at
    /// the origin of the window. A drag only moves the notch in memory; the choice
    /// is written back once, when the pointer is released.
    fn book_splitter(&self, cx: &Context<Self>) -> AnyElement {
        let line = cx.theme().roles.line.border.hsla;
        let down_widget = cx.entity().downgrade();
        let move_widget = cx.entity().downgrade();
        let up_widget = cx.entity().downgrade();
        let up_out_widget = cx.entity().downgrade();
        div()
            .id(ElementId::Name(SharedString::new_static(
                "PredictionMarketBookSplitter",
            )))
            .debug_selector(|| "TEST_PREDICTION_MARKET_SPLITTER".into())
            .role(Role::Splitter)
            .aria_label(t!(cx, "widget.prediction_market.book.resize"))
            .flex_none()
            .h(BOOK_SPLITTER_HEIGHT)
            .w_full()
            .flex()
            .items_center()
            .occlude()
            .cursor_ns_resize()
            .on_mouse_down(MouseButton::Left, move |event, _, app| {
                if let Some(widget) = down_widget.upgrade() {
                    widget.update(app, |this, _| this.begin_book_split(event.position));
                }
            })
            .on_drag(BookSplitterDrag, move |_, _, _, cx| {
                // The preview follows the pointer and draws nothing: the notch is
                // the feedback, and the drag's origin is the widget's own state.
                cx.new(|_| BookSplitterDrag)
            })
            .on_drag_move(move |event: &DragMoveEvent<BookSplitterDrag>, _, app| {
                let position = event.event.position;
                if let Some(widget) = move_widget.upgrade() {
                    widget.update(app, |this, cx| this.drag_book_splitter(position, cx));
                }
            })
            .on_mouse_up(MouseButton::Left, move |_, _, app| {
                if let Some(widget) = up_widget.upgrade() {
                    widget.update(app, |this, cx| this.finish_book_split(cx));
                }
            })
            .on_mouse_up_out(MouseButton::Left, move |_, _, app| {
                if let Some(widget) = up_out_widget.upgrade() {
                    widget.update(app, |this, cx| this.finish_book_split(cx));
                }
            })
            .child(div().h(px(1.0)).w_full().bg(line))
            .into_any_element()
    }

    /// Remembers where a splitter drag started.
    ///
    /// The origin is the pointer's window position, not the bar's: the drag moves
    /// the bar under the pointer, so measuring from the bar itself would make
    /// each move chase the layout it just produced.
    fn begin_book_split(&mut self, position: Point<Pixels>) {
        self.split_drag = Some(SplitDragStart {
            start_ratio: clamp_ratio(self.config.order_book_ratio),
            start_y: position.y,
        });
        self.split_dragging = false;
    }

    /// Moves the notch to the ratio the pointer asks for.
    ///
    /// The config is updated in memory on every move, and reported only when the
    /// gesture ends: a write at pointer speed would rewrite the layout file at the
    /// rate of the pointer.
    fn drag_book_splitter(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(start) = self.split_drag else {
            return;
        };
        let Some(area) = self.split_bounds.get() else {
            return;
        };
        let ratio = split_ratio(
            start.start_ratio,
            position.y - start.start_y,
            area.size.height,
        );
        // The drawn panel never gets shorter than its minimum height, so the stored
        // notch must not go below the share that height takes either: otherwise the
        // splitter stops at the drawn edge while the value keeps moving and
        // persists a notch nothing on screen chose.
        let height = f32::from(area.size.height);
        let ratio = if height > 0.0 {
            ratio
                .max(MIN_BOOK_RATIO.max(f32::from(MIN_BOOK_HEIGHT) / height))
                .min(MAX_BOOK_RATIO)
        } else {
            ratio
        };
        if ratio == self.config.order_book_ratio {
            return;
        }
        self.config.order_book_ratio = ratio;
        self.split_dragging = true;
        cx.notify();
    }

    /// Ends a splitter gesture, reporting the notch only when the gesture moved
    /// it away from where it started.
    ///
    /// The two release listeners are not both scoped to the bar: `on_mouse_up`
    /// only fires while the pointer is over it, so a drag released elsewhere is
    /// seen by `on_mouse_up_out`, which hears every release in the window. A
    /// press is therefore the gesture's identity: a release with no press armed —
    /// a click anywhere else, or a gesture the book hid away — is not a gesture,
    /// and a drag that came back to its starting ratio chose nothing. Both fields
    /// are cleared either way.
    fn finish_book_split(&mut self, cx: &mut Context<Self>) {
        let start = self.split_drag.take();
        let dragged = std::mem::take(&mut self.split_dragging);
        let changed =
            dragged && start.is_some_and(|start| self.config.order_book_ratio != start.start_ratio);
        if changed {
            self.config_change.publish(cx);
            cx.notify();
        }
    }

    /// The chart, or the sentence that stands in for it when the reading has no points.
    ///
    /// The pointer is stored in window coordinates and snapped to a sample by the
    /// canvas, so a frame only has to carry it across.
    fn chart_area(
        &self,
        snapshot: &Arc<PredictionMarketSnapshot>,
        has_points: bool,
        pulse_phase: Option<f32>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let move_widget = cx.entity().downgrade();
        let hover_widget = cx.entity().downgrade();
        div()
            .id(ElementId::Name(SharedString::new_static(
                "PredictionMarketChart",
            )))
            .debug_selector(|| "TEST_PREDICTION_MARKET_CHART".into())
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .cursor(CursorStyle::Crosshair)
            .on_mouse_move(move |event: &MouseMoveEvent, window, app| {
                let position = event.position;
                let _ = move_widget.update(app, |this, cx| {
                    this.queue_mouse_move(position, window, cx);
                });
            })
            .on_hover(move |hovering, _, app| {
                if !*hovering {
                    let _ = hover_widget.update(app, |this, cx| this.clear_pointer(cx));
                }
            })
            .child(div().relative().size_full().min_h_0().child(chart_element(
                snapshot.clone(),
                ChartCanvasOptions {
                    pointer: self.pointer,
                    pulse_phase,
                },
                self.chart_caches.clone(),
            )))
            .when(!has_points, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(EmptyState::new(
                            "PredictionMarketEmpty",
                            t!(cx, "common.no_data"),
                        )),
                )
            })
            .into_any_element()
    }
}

/// One muted sentence, centered in the space the panel would otherwise use.
fn centered_muted(text: SharedString, cx: &Context<PredictionMarketWidget>) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(cx.theme().roles.fg.muted.hsla)
        .child(text)
        .into_any_element()
}

impl Render for PredictionMarketWidget {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is cached, and a cached subtree is only repainted when its own
        // view says so. The pulse and the book's depth bars are exactly that case:
        // each asks for one frame at a time and stops asking as soon as its
        // movement has ended.
        let reduce_motion = cx.reduce_motion();
        let pulse_phase = self
            .pulse
            .and_then(|pulse| pulse.phase(Instant::now()))
            .filter(|_| !reduce_motion);
        if pulse_phase.is_none() {
            self.pulse = None;
        }

        let Some(snapshot) = self.snapshot.clone() else {
            return div().size_full().min_h_0().into_any_element();
        };
        let book_active = self.book_panel.advance(Instant::now(), reduce_motion);
        if pulse_phase.is_some() || (book_active && self.config.order_book_visible) {
            window.request_animation_frame();
        }

        let has_points = snapshot.series.iter().any(|line| !line.points.is_empty());
        // Only a venue this build can read a book from is divided with one: an
        // event whose venue publishes no CLOB keeps the whole body for its chart.
        let shows_book = self.config.order_book_visible && self.supported_book_target().is_some();
        let mut body = div().size_full().min_h_0().flex().flex_col();
        // A reading with no points still carries one series per side: the legend is
        // what names them, so it stays exactly as long as the snapshot does.
        if !snapshot.series.is_empty() {
            body = body.child(self.legend(&snapshot, cx));
        }
        // The chart and the book share what the legend leaves: the chart takes
        // the rest of it, and a hidden book leaves it all to the chart. The
        // splitter between them divides the area both are laid out in, so that
        // area's bounds are what a drag measures its pointer against.
        let split_bounds = self.split_bounds.clone();
        let mut content = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .on_prepaint(move |bounds, _, _| split_bounds.set(Some(bounds)));
        content = content.child(self.chart_area(&snapshot, has_points, pulse_phase, cx));
        if shows_book {
            content = content.child(self.book_splitter(cx));
            content = content.child(self.order_book_panel(cx));
        }
        body.child(content).into_any_element()
    }
}

/// The message key of one range's label.
fn range_label(range: PredictionRange, cx: &App) -> SharedString {
    match range {
        PredictionRange::Hour1 => t!(cx, "widget.prediction_market.range.hour_1"),
        PredictionRange::Hour6 => t!(cx, "widget.prediction_market.range.hour_6"),
        PredictionRange::Day1 => t!(cx, "widget.prediction_market.range.day_1"),
        PredictionRange::Week1 => t!(cx, "widget.prediction_market.range.week_1"),
        PredictionRange::Month1 => t!(cx, "widget.prediction_market.range.month_1"),
        PredictionRange::All => t!(cx, "widget.prediction_market.range.all"),
    }
}

/// The stable picker id of one range, independent of its translated label.
fn range_id(range: PredictionRange) -> &'static str {
    match range {
        PredictionRange::Hour1 => "hour_1",
        PredictionRange::Hour6 => "hour_6",
        PredictionRange::Day1 => "day_1",
        PredictionRange::Week1 => "week_1",
        PredictionRange::Month1 => "month_1",
        PredictionRange::All => "all",
    }
}

/// The range a picker id names.
fn range_of_id(id: &str) -> Option<PredictionRange> {
    PredictionRange::ALL
        .into_iter()
        .find(|range| range_id(*range) == id)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        rc::Rc,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use app_config::settings::Config;
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{
        AppContext, Entity, IntoElement, Modifiers, ParentElement, Render, TestAppContext,
        VisualTestContext, Window, div, point, px, size,
    };
    use market_participants_connector::{
        catalog::error::CatalogError,
        connector::MarketParticipantsConnector,
        market::error::MarketError,
        venue::definition::{CapabilitySupport, ScopeEntry},
    };
    use market_participants_model::{
        decimal::{Fraction, Ratio},
        identifier::{IdentifierValue, VenueScopeId},
        listing::route::VenueRoute,
        prediction::{
            PredictionEventView, PredictionLeg, PredictionOrderBookUpdate, PredictionPricePoint,
            PredictionSeries,
        },
    };
    use widget_tokio_runtime::global;

    use super::*;
    use crate::{
        book::PredictionOrderBookEventSubscription,
        chart::paint::{reset_log, take_log},
        model::PredictionMarketSnapshot,
        service::{
            PredictionMarketSource, PredictionMarketStreamKey, PredictionMarketStreamMessage,
            PredictionSeriesEventSubscription,
        },
        test_support::{charted_market, event_of, event_ref, event_with, level, order_book},
    };

    /// A source that answers one event and one two-point history, and records the range
    /// every history request asked for.
    ///
    /// The request itself is not observable from the pane: it crosses the service, so
    /// the source is the only place a test can see which range was read.
    #[derive(Clone, Default)]
    struct FakeSource {
        ranges: Arc<Mutex<Vec<PredictionRange>>>,
        book_targets: Arc<Mutex<Vec<PredictionOrderBookTarget>>>,
        book_streams_closed: Arc<Mutex<usize>>,
    }

    impl FakeSource {
        fn ranges(&self) -> Vec<PredictionRange> {
            self.ranges
                .lock()
                .expect("the fake's range log is not poisoned")
                .clone()
        }

        fn book_targets(&self) -> Vec<PredictionOrderBookTarget> {
            self.book_targets
                .lock()
                .expect("the fake's book log is not poisoned")
                .clone()
        }

        /// How many book channels the service has closed.
        fn book_streams_closed(&self) -> usize {
            *self
                .book_streams_closed
                .lock()
                .expect("the fake's stream log is not poisoned")
        }
    }

    impl PredictionMarketSource for FakeSource {
        type Subscription = PendingSubscription;

        async fn event(
            &self,
            _event: PredictionEventRef,
            _locale: Option<SmolStr>,
        ) -> Result<PredictionEventView, MarketError> {
            let event = event_ref();
            let (market, _) = charted_market(&event, "charted");
            Ok(event_with(vec![market]))
        }

        async fn series(
            &self,
            _event: PredictionEventRef,
            legs: Vec<PredictionLeg>,
            range: PredictionRange,
        ) -> Result<Vec<PredictionSeries>, MarketError> {
            self.ranges
                .lock()
                .expect("the fake's range log is not poisoned")
                .push(range);
            Ok(legs
                .iter()
                .map(|leg| PredictionSeries {
                    leg: leg.leg.clone(),
                    points: vec![price_point(1_000, 40), price_point(2_000, 60)],
                })
                .collect())
        }

        async fn subscribe_series(
            &self,
            _event: PredictionEventRef,
            _legs: Vec<PredictionLeg>,
        ) -> Result<Self::Subscription, MarketError> {
            Ok(PendingSubscription)
        }
    }

    impl PredictionOrderBookSource for FakeSource {
        type Subscription = WatchedBookSubscription;

        async fn fetch_order_book(
            &self,
            target: PredictionOrderBookTarget,
        ) -> Result<PredictionOrderBook, MarketError> {
            self.book_targets
                .lock()
                .expect("the fake's book log is not poisoned")
                .push(target.clone());
            let mut book = order_book(
                target.leg,
                "0.001",
                vec![level("0.34", "50")],
                vec![level("0.35", "100")],
            );
            book.last_trade_price = Some(
                "0.345"
                    .parse::<Fraction>()
                    .expect("the fixture price is a fraction"),
            );
            Ok(book)
        }

        async fn subscribe_order_book(
            &self,
            _target: PredictionOrderBookTarget,
        ) -> Result<Self::Subscription, MarketError> {
            Ok(WatchedBookSubscription {
                closed: self.book_streams_closed.clone(),
            })
        }
    }

    /// A live stream that publishes nothing: every state a test asserts is reported to
    /// the pane directly, so a source that spoke would race the assertion.
    struct PendingSubscription;

    impl PredictionSeriesEventSubscription for PendingSubscription {
        async fn recv(&mut self) -> Option<Result<Vec<PredictionSeries>, MarketError>> {
            std::future::pending().await
        }
    }

    /// A book channel that publishes nothing and counts its own retirement.
    ///
    /// The service owns the subscription, so the count going up is the observable
    /// proof that a stop — or a replacing request — really cancelled the stream.
    struct WatchedBookSubscription {
        closed: Arc<Mutex<usize>>,
    }

    impl Drop for WatchedBookSubscription {
        fn drop(&mut self) {
            *self
                .closed
                .lock()
                .expect("the fake's stream log is not poisoned") += 1;
        }
    }

    impl PredictionOrderBookEventSubscription for WatchedBookSubscription {
        async fn recv(&mut self) -> Option<Result<Vec<PredictionOrderBookUpdate>, MarketError>> {
            std::future::pending().await
        }
    }

    /// One pane's body in a window of its own.
    struct BodyHarness {
        pane: Entity<PredictionMarketWidget>,
    }

    impl Render for BodyHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.pane.clone())
        }
    }

    /// One pane's header in a window of its own.
    struct HeaderHarness {
        pane: Entity<PredictionMarketWidget>,
    }

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let header = self.pane.update(cx, |pane, cx| pane.header(window, cx));
            div().size_full().child(
                div()
                    .debug_selector(|| "TEST_PREDICTION_MARKET_HEADER_ROW".into())
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .children(header.left)
                    .children(header.right),
            )
        }
    }

    /// One probability sample at `timestamp`, in percent points.
    fn price_point(timestamp: i64, percent: u32) -> PredictionPricePoint {
        PredictionPricePoint {
            timestamp,
            probability: Fraction::new(Ratio::from_inner(
                u128::from(percent) * 1_000_000_000_000_000,
            ))
            .expect("the fixture probability is a fraction"),
        }
    }

    /// A reading of the charted market whose every series carries `timestamps`.
    fn reading(
        revision: u64,
        range: PredictionRange,
        timestamps: &[i64],
    ) -> Arc<PredictionMarketSnapshot> {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let series = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.clone(),
                points: timestamps
                    .iter()
                    .map(|timestamp| price_point(*timestamp, 50))
                    .collect(),
            })
            .collect();
        Arc::new(PredictionMarketSnapshot::new(
            revision,
            range,
            event_with(vec![market.clone()]),
            market,
            series,
        ))
    }

    /// A reading of the charted market with one empty series per side.
    ///
    /// Every source publishes the legs of the selected market even when the window
    /// holds no sample, so an empty reading still names what it would have drawn.
    fn empty_reading() -> Arc<PredictionMarketSnapshot> {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let series = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.clone(),
                points: Vec::new(),
            })
            .collect();
        Arc::new(PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            event_with(vec![market.clone()]),
            market,
            series,
        ))
    }

    /// The stream key a pane built from the default config reports.
    fn stream_key() -> PredictionMarketStreamKey {
        PredictionMarketStreamKey {
            event: event_ref(),
            market: None,
            range: PredictionRange::All,
            locale: None,
            epoch: 0,
        }
    }

    /// Installs the application state, and commits the venues a test addresses.
    ///
    /// A pane only reads a market that is in the loaded-venues view, and the fixture
    /// seam is how a test commits one without a venue answering. `scopes` states a
    /// scope set a fixture owns instead of the venue's static one, which is how a
    /// test gives Hyperliquid a prediction scope.
    fn install_market(cx: &mut App, venues: &[VenueId], scopes: &[(VenueId, Vec<ScopeEntry>)]) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let manager = ConfigManager::in_memory(Config::default(), cx);
        cx.set_global(manager);
        MarketParticipantsConnector::install(cx)
            .expect("the market participants root should install");
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(venues),
                scopes,
                &[],
            )
            .expect("the fixture venue should load");
    }

    /// The scope set a Hyperliquid prediction fixture commits.
    ///
    /// Hyperliquid publishes outcome events on a scope of its own, and it declares
    /// no order-book capability: that asymmetry is what the fixture is for.
    fn hyperliquid_prediction_scopes() -> Vec<ScopeEntry> {
        vec![ScopeEntry {
            scope: VenueScopeId::new("predict").expect("the fixture scope is canonical"),
            catalog_source: None,
            normalization: Vec::new(),
            capabilities: vec![
                CapabilitySupport {
                    capability: CapabilityId::PredictionEvents,
                    needs_auth: false,
                },
                CapabilitySupport {
                    capability: CapabilityId::PredictionSeries,
                    needs_auth: false,
                },
            ],
        }]
    }

    /// The fixture event on a Polymarket scope the fixture did not load.
    ///
    /// A pane reading it has been cut off from its market by a shrinking commit
    /// without the venue itself being the one that changed.
    fn cut_event_ref() -> PredictionEventRef {
        PredictionEventRef {
            route: VenueRoute::new(
                VenueId::Polymarket,
                VenueScopeId::new("cut").expect("the fixture scope is canonical"),
            ),
            native_event: IdentifierValue::new("event").expect("the fixture event id is valid"),
        }
    }

    /// The fixture event on Hyperliquid's prediction scope.
    fn hyperliquid_event_ref() -> PredictionEventRef {
        PredictionEventRef {
            route: VenueRoute::new(
                VenueId::Hyperliquid,
                VenueScopeId::new("predict").expect("the fixture scope is canonical"),
            ),
            native_event: IdentifierValue::new("event").expect("the fixture event id is valid"),
        }
    }

    /// The report a pane publishes its persisted config through.
    fn reports() -> Rc<Cell<usize>> {
        Rc::new(Cell::new(0))
    }

    /// A pane the framework would have built, with a fake source behind it.
    fn pane_with(
        cx: &mut TestAppContext,
        config: PredictionMarketConfig,
        reports: Rc<Cell<usize>>,
    ) -> (Entity<PredictionMarketWidget>, FakeSource) {
        pane_with_market(cx, config, reports, &[VenueId::Polymarket], &[])
    }

    /// A pane whose fixture loaded `venues` instead of the default one.
    fn pane_with_market(
        cx: &mut TestAppContext,
        config: PredictionMarketConfig,
        reports: Rc<Cell<usize>>,
        venues: &[VenueId],
        scopes: &[(VenueId, Vec<ScopeEntry>)],
    ) -> (Entity<PredictionMarketWidget>, FakeSource) {
        cx.executor().allow_parking();
        cx.update(|cx| install_market(cx, venues, scopes));
        // The fixture commit announces itself on the application executor. Let that
        // announcement land before the pane subscribes: a pane that was already
        // listening would read it as a venue refresh and start its own first load
        // over, which is a race the assertions below cannot see.
        cx.run_until_parked();
        let source = FakeSource::default();
        let pane = cx.update(|cx| {
            cx.new(|cx: &mut Context<PredictionMarketWidget>| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |pane, cx| pane.load(cx));
                });
                let config_change =
                    WidgetConfigChange::new(move |_| reports.set(reports.get() + 1));
                PredictionMarketWidget::new_with_source(
                    config,
                    source.clone(),
                    status,
                    config_change,
                    recovery,
                    cx,
                )
            })
        });
        (pane, source)
    }

    /// Selects the fixture event, which is what starts a load.
    fn select_event(pane: &Entity<PredictionMarketWidget>, cx: &mut App) {
        pane.update(cx, |pane, cx| {
            pane.select(event_ref(), SmolStr::new_static("An event"), cx);
        });
    }

    /// Reports a reading as the stream would.
    fn report_snapshot(
        pane: &Entity<PredictionMarketWidget>,
        key: PredictionMarketStreamKey,
        snapshot: Arc<PredictionMarketSnapshot>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_latest(
                key,
                Arc::new(PredictionMarketStreamMessage::Snapshot(snapshot)),
                cx,
            )
        })
    }

    /// Reports one reliable status as the stream would.
    fn report_status(
        pane: &Entity<PredictionMarketWidget>,
        key: PredictionMarketStreamKey,
        status: PredictionMarketStatus,
        cx: &mut App,
    ) {
        pane.update(cx, |pane, cx| {
            pane.apply_reliable(
                key,
                Arc::new(PredictionMarketStreamMessage::Status(status)),
                cx,
            );
        });
    }

    /// The stream key of one fixture book generation.
    fn book_stream_key(epoch: u64) -> PredictionOrderBookStreamKey {
        let (_, legs) = charted_market(&event_ref(), "charted");
        PredictionOrderBookStreamKey {
            event: event_ref(),
            leg: legs[0].clone(),
            epoch,
        }
    }

    /// One reading of the fixture book.
    fn book_reading() -> Arc<PredictionOrderBook> {
        let (_, legs) = charted_market(&event_ref(), "charted");
        Arc::new(order_book(
            legs[0].clone(),
            "0.001",
            vec![level("0.34", "50")],
            vec![level("0.35", "100")],
        ))
    }

    /// One book of the fixture market with `asks` and `bids` levels on each side.
    ///
    /// A long ladder is what proves the panel scrolls instead of drawing every
    /// row it was given.
    fn long_book(asks: usize, bids: usize) -> Arc<PredictionOrderBook> {
        let (_, legs) = charted_market(&event_ref(), "charted");
        Arc::new(order_book(
            legs[0].clone(),
            "0.001",
            (0..bids)
                .map(|index| level(&format!("0.{:03}", 1 + index), "10"))
                .collect(),
            (0..asks)
                .map(|index| level(&format!("0.{:03}", 100 + index), "10"))
                .collect(),
        ))
    }

    /// A book with no resting level on either side.
    fn empty_book() -> Arc<PredictionOrderBook> {
        let (_, legs) = charted_market(&event_ref(), "charted");
        Arc::new(order_book(legs[0].clone(), "0.001", Vec::new(), Vec::new()))
    }

    /// A reading of the Hyperliquid prediction fixture, whose venue this build
    /// cannot read a book from.
    fn hyperliquid_reading() -> Arc<PredictionMarketSnapshot> {
        let event = hyperliquid_event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let series = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.clone(),
                points: vec![price_point(1_000, 50)],
            })
            .collect();
        Arc::new(PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            event_of(event, vec![market.clone()]),
            market,
            series,
        ))
    }

    /// The stream key the Hyperliquid fixture event is reported under.
    fn hyperliquid_stream_key() -> PredictionMarketStreamKey {
        PredictionMarketStreamKey {
            event: hyperliquid_event_ref(),
            market: None,
            range: PredictionRange::All,
            locale: None,
            epoch: 0,
        }
    }

    /// Points the pane at the fixture market and hands it one book reading.
    fn draw_book(
        pane: &Entity<PredictionMarketWidget>,
        book: Arc<PredictionOrderBook>,
        cx: &mut App,
    ) {
        pane.update(cx, |pane, cx| {
            pane.selection = Some(event_ref());
            pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
            pane.book_key = Some(book_stream_key(1));
            pane.book_epoch = 1;
            pane.apply_book_latest(
                book_stream_key(1),
                Arc::new(PredictionOrderBookStreamMessage::Snapshot(book)),
                cx,
            );
            pane.status.report(WidgetState::Ready, cx);
        });
    }

    /// Draws one pane's body in its own window and repaints it once.
    fn body_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<PredictionMarketWidget>,
        area: gpui_kit::Size<Pixels>,
    ) -> &'a mut VisualTestContext {
        let (_, window) = cx.add_window_view(|_, _| BodyHarness { pane: pane.clone() });
        window.simulate_resize(area);
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        window
    }

    /// Reports a book reading as its stream would.
    fn report_book_snapshot(
        pane: &Entity<PredictionMarketWidget>,
        key: PredictionOrderBookStreamKey,
        book: Arc<PredictionOrderBook>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_book_latest(
                key,
                Arc::new(PredictionOrderBookStreamMessage::Snapshot(book)),
                cx,
            )
        })
    }

    /// Reports one book status as its stream would.
    fn report_book_status(
        pane: &Entity<PredictionMarketWidget>,
        key: PredictionOrderBookStreamKey,
        status: PredictionOrderBookStatus,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_book_reliable(
                key,
                Arc::new(PredictionOrderBookStreamMessage::Status(status)),
                cx,
            )
        })
    }

    /// Lets the pane and its service run until what the test waits for happens.
    ///
    /// Two executors are involved: the pane sends its command from GPUI's own and the
    /// service answers on the application runtime, so both are given a turn — the pane
    /// first, because a command that was not sent is a service with nothing to answer.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_widget_and_its_config_have_their_stable_identity_and_defaults() {
        assert_eq!(
            PredictionMarketWidget::ID.as_str(),
            "kytrix.prediction_market"
        );
        assert_eq!(
            PredictionMarketWidget::DISPLAY_NAME,
            "widget.names.prediction_market"
        );
        assert_eq!(
            PredictionMarketConfig::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
        assert_eq!(
            PredictionMarketConfig::default().range,
            PredictionRange::All,
            "the live site opens on the whole life of the market"
        );
    }

    #[gpui_kit::test]
    fn the_config_is_the_range_the_pane_was_built_with(cx: &mut TestAppContext) {
        let config = PredictionMarketConfig {
            range: PredictionRange::Week1,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports());

        cx.update(|cx| {
            assert_eq!(pane.read(cx).config(), Some(&config));
        });
    }

    #[gpui_kit::test]
    fn a_pane_without_an_event_should_wait_with_the_centered_prompt(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());

        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.load(cx));
            assert!(
                i18n::t!(cx, "widget.prediction_market.select_event_prompt")
                    .contains("align=\"center\""),
                "the placeholder prompt is centered"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message(
                    "widget.prediction_market.select_event_prompt"
                )),
                "the placeholder points at the centered prompt"
            );
            assert!(!pane.read(cx).has_content());
        });
    }

    #[gpui_kit::test]
    fn the_timeframe_picker_should_sit_in_the_header_right_group(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());

        let (_harness, cx) = cx.add_window_view(|_, _| HeaderHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(300.0)));
        let header = cx.update(|window, cx| pane.update(cx, |pane, cx| pane.header(window, cx)));
        assert!(header.left.is_some(), "the event picker stays on the left");
        assert!(
            header.right.is_some(),
            "the timeframe picker belongs to the right group"
        );

        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_HEADER_ROW")
                .is_some(),
            "the header row is in the frame"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_RANGE").is_some(),
            "the header draws the timeframe picker"
        );
    }

    #[gpui_kit::test]
    fn a_load_should_read_the_configured_range_and_draw_it(cx: &mut TestAppContext) {
        let (pane, source) = pane_with(cx, PredictionMarketConfig::default(), reports());

        cx.update(|cx| select_event(&pane, cx));
        settle_until(cx, "the load to reach the venue", |_| {
            !source.ranges().is_empty()
        });
        assert_eq!(source.ranges(), vec![PredictionRange::All]);
        settle_until(cx, "the first reading", |cx| pane.read(cx).has_content());

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert_eq!(
                pane.snapshot.as_ref().map(|snapshot| snapshot.range),
                Some(PredictionRange::All)
            );
        });
    }

    #[gpui_kit::test]
    fn switching_the_range_should_drop_the_chart_and_connect_until_the_reading_lands(
        cx: &mut TestAppContext,
    ) {
        let reports = reports();
        let (pane, source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| select_event(&pane, cx));
        settle_until(cx, "the first reading", |cx| pane.read(cx).has_content());
        assert_eq!(reports.get(), 0);

        cx.update(|cx| {
            let caches = pane.read(cx).chart_caches.clone();
            pane.update(cx, |pane, cx| {
                pane.pointer = Some(point(px(40.0), px(30.0)));
                pane.pointer_frame.queue(point(px(41.0), px(31.0)));
                pane.pulse = Some(Pulse {
                    started: Instant::now(),
                });
                pane.select_range(PredictionRange::Hour1, cx);
            });
            assert_eq!(
                pane.update(cx, |pane, _| pane.pointer_frame.take()),
                None,
                "the position queued for the dropped chart is gone too"
            );
            let pane = pane.read(cx);
            assert_eq!(pane.config.range, PredictionRange::Hour1);
            assert_eq!(reports.get(), 1, "a persisted change is reported once");
            assert_eq!(pane.pointer, None, "another range has no hovered sample");
            assert!(
                pane.pulse.is_none(),
                "the old range's pulse does not carry over"
            );
            assert!(
                pane.snapshot.is_none(),
                "the chart answered the window the user just left"
            );
            assert!(
                !pane.has_content(),
                "so the framework draws its connecting spinner instead of the old chart"
            );
            assert_eq!(
                pane.status.state(cx),
                WidgetState::Connecting,
                "the switch is a new connection, not a silent refresh"
            );
            assert!(
                !Rc::ptr_eq(&caches, &pane.chart_caches),
                "the dropped chart's prepared frame is not kept alive"
            );
        });

        settle_until(cx, "the new range's reading", |cx| {
            pane.read(cx)
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.range == PredictionRange::Hour1)
        });
        assert!(source.ranges().contains(&PredictionRange::Hour1));
        assert_eq!(reports.get(), 1, "the switch is reported exactly once");
        cx.update(|cx| {
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        });
    }

    #[gpui_kit::test]
    fn a_live_reading_should_pulse_only_when_the_newest_point_advances(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());

        cx.update(|cx| {
            pane.update(cx, |pane, _| pane.selection = Some(event_ref()));
            let key = stream_key();
            assert!(
                report_snapshot(
                    &pane,
                    key.clone(),
                    reading(1, PredictionRange::All, &[1_000]),
                    cx
                ),
                "the first reading of a scope is accepted"
            );
            assert!(
                pane.read(cx).pulse.is_none(),
                "opening a chart is not a new observation"
            );

            assert!(report_snapshot(
                &pane,
                key.clone(),
                reading(2, PredictionRange::All, &[1_000]),
                cx
            ));
            assert!(
                pane.read(cx).pulse.is_none(),
                "the newest point did not move, so nothing breathes"
            );

            cx.set_reduce_motion(true);
            assert!(report_snapshot(
                &pane,
                key.clone(),
                reading(3, PredictionRange::All, &[2_000]),
                cx
            ));
            assert!(
                pane.read(cx).pulse.is_none(),
                "a reader who asked for less movement is shown none"
            );

            cx.set_reduce_motion(false);
            assert!(report_snapshot(
                &pane,
                key,
                reading(4, PredictionRange::All, &[3_000]),
                cx
            ));
            assert!(
                pane.read(cx).pulse.is_some(),
                "a newer sample breathes once"
            );
        });
    }

    #[gpui_kit::test]
    fn a_tick_during_a_running_pulse_should_not_restart_it(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());

        cx.update(|cx| {
            pane.update(cx, |pane, _| pane.selection = Some(event_ref()));
            let key = stream_key();
            assert!(report_snapshot(
                &pane,
                key.clone(),
                reading(1, PredictionRange::All, &[1_000]),
                cx
            ));
            assert!(report_snapshot(
                &pane,
                key.clone(),
                reading(2, PredictionRange::All, &[1_000, 2_000]),
                cx
            ));
            let started = pane
                .read(cx)
                .pulse
                .expect("the first tick breathes")
                .started;

            assert!(report_snapshot(
                &pane,
                key.clone(),
                reading(3, PredictionRange::All, &[1_000, 2_000, 3_000]),
                cx
            ));
            assert_eq!(
                pane.read(cx)
                    .pulse
                    .expect("the pulse is still running")
                    .started,
                started,
                "a tick that lands mid-breath keeps the running pulse instead of \
                 restarting it at phase zero"
            );

            // A reading that lands after the breath has ended starts a new one.
            pane.update(cx, |pane, _| {
                pane.pulse = Some(Pulse {
                    started: Instant::now() - Duration::from_millis(1_500),
                });
            });
            assert!(report_snapshot(
                &pane,
                key,
                reading(4, PredictionRange::All, &[1_000, 2_000, 3_000, 4_000]),
                cx
            ));
            let restarted = pane
                .read(cx)
                .pulse
                .expect("the next tick breathes again")
                .started;
            assert!(
                restarted > started,
                "an ended pulse is replaced by the next observation"
            );
        });
    }

    #[test]
    fn the_endpoint_pulse_should_run_one_cycle_and_then_end() {
        let started = Instant::now();
        let pulse = Pulse { started };

        assert_eq!(pulse.phase(started), Some(0.0));
        let halfway = pulse
            .phase(started + Duration::from_millis(350))
            .expect("the cycle is still running");
        assert!(
            (halfway - 0.5).abs() < 1e-4,
            "the phase is the fraction of the single cycle: {halfway}"
        );
        assert!(pulse.phase(started + Duration::from_millis(699)).is_some());
        assert_eq!(
            pulse.phase(started + Duration::from_millis(700)),
            None,
            "the effect ends after 0.7 s"
        );
    }

    #[test]
    fn a_pointer_frame_should_keep_only_the_newest_position() {
        let mut frame = PointerFrameState::default();
        let first = point(px(10.0), px(20.0));
        let latest = point(px(30.0), px(40.0));

        assert!(
            frame.queue(first),
            "the first position is the one that schedules the frame"
        );
        assert!(
            !frame.queue(latest),
            "a second move before the frame joins the one already scheduled"
        );
        assert_eq!(
            frame.take(),
            Some(latest),
            "the frame applies the newest position"
        );
        assert!(frame.queue(first), "a taken frame can schedule again");
        assert_eq!(frame.take(), Some(first));
    }

    #[test]
    fn a_pointer_frame_that_was_cleared_should_apply_nothing() {
        let mut frame = PointerFrameState::default();
        assert!(frame.queue(point(px(10.0), px(20.0))));
        frame.clear_pending();

        assert_eq!(
            frame.take(),
            None,
            "the frame that was already scheduled has no position left to apply"
        );
    }

    #[gpui_kit::test]
    fn a_message_from_another_range_or_an_older_revision_should_be_refused(
        cx: &mut TestAppContext,
    ) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());

        cx.update(|cx| {
            pane.update(cx, |pane, _| pane.selection = Some(event_ref()));
            let key = stream_key();
            assert!(report_snapshot(
                &pane,
                key.clone(),
                reading(5, PredictionRange::All, &[1_000]),
                cx
            ));

            assert!(
                !report_snapshot(
                    &pane,
                    key.clone(),
                    reading(4, PredictionRange::All, &[2_000]),
                    cx
                ),
                "a revision older than the one on screen is stale"
            );
            let other_range = PredictionMarketStreamKey {
                range: PredictionRange::Hour1,
                ..key
            };
            assert!(
                !report_snapshot(
                    &pane,
                    other_range,
                    reading(6, PredictionRange::Hour1, &[2_000]),
                    cx
                ),
                "another range is another chart"
            );

            let held = pane
                .read(cx)
                .snapshot
                .clone()
                .expect("the accepted reading is still the one on screen");
            assert_eq!(held.revision, 5);
            assert_eq!(held.range, PredictionRange::All);
        });
    }

    #[gpui_kit::test]
    fn a_reliable_message_should_be_refused_by_key_waiting_and_failure(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(5, PredictionRange::All, &[1_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
            let key = stream_key();
            let held = pane
                .read(cx)
                .snapshot
                .clone()
                .expect("the fixture reading is held");

            report_status(
                &pane,
                PredictionMarketStreamKey {
                    epoch: 1,
                    ..key.clone()
                },
                PredictionMarketStatus::Failed {
                    error: PredictionMarketFetchError::NoMarkets,
                },
                cx,
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "a message from a stream this pane no longer follows is refused"
            );
            assert!(
                Arc::ptr_eq(
                    pane.read(cx)
                        .snapshot
                        .as_ref()
                        .expect("the reading is still held"),
                    &held
                ),
                "a refused message does not replace the reading"
            );

            pane.update(cx, |pane, cx| {
                pane.awaiting_market = true;
                pane.status.report(WidgetState::Connecting, cx);
            });
            report_status(
                &pane,
                key.clone(),
                PredictionMarketStatus::Ready { revision: 9 },
                cx,
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Connecting,
                "a pane waiting for its market refuses a late reading"
            );
            assert!(
                Arc::ptr_eq(
                    pane.read(cx)
                        .snapshot
                        .as_ref()
                        .expect("the reading is still held"),
                    &held
                ),
                "the waiting pane's held reading is not replaced either"
            );

            pane.update(cx, |pane, cx| {
                pane.awaiting_market = false;
                pane.status.report(
                    WidgetState::failed(
                        Arc::new(PredictionMarketError::Fetch(
                            PredictionMarketFetchError::NoMarkets,
                        )),
                        WidgetAction::new("common.retry", |_| {}),
                    ),
                    cx,
                );
            });
            let failed = pane.read(cx).status.state(cx);
            report_status(
                &pane,
                key.clone(),
                PredictionMarketStatus::Ready { revision: 10 },
                cx,
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                failed,
                "a terminal failure is not overwritten by a late ready"
            );
            report_status(
                &pane,
                key,
                PredictionMarketStatus::Failed {
                    error: PredictionMarketFetchError::Timeout(Duration::from_secs(1)),
                },
                cx,
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                failed,
                "a second failure does not replace the one the reader is looking at"
            );
            assert!(
                Arc::ptr_eq(
                    pane.read(cx)
                        .snapshot
                        .as_ref()
                        .expect("the failed reading is still held"),
                    &held
                ),
                "a failed pane keeps the reading its banner is drawn over"
            );
        });
    }

    #[gpui_kit::test]
    fn a_venue_reload_should_drop_the_reading_and_read_the_market_again(cx: &mut TestAppContext) {
        let (pane, source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| select_event(&pane, cx));
        settle_until(cx, "the first reading", |cx| pane.read(cx).has_content());
        assert_eq!(source.ranges().len(), 1);

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.pointer = Some(point(px(40.0), px(30.0)));
                pane.pointer_frame.queue(point(px(41.0), px(31.0)));
                pane.pulse = Some(Pulse {
                    started: Instant::now(),
                });
                pane.on_venues_updated(
                    &VenuesUpdated {
                        venue: VenueId::Polymarket,
                        revision: 2,
                    },
                    cx,
                );
                assert_eq!(
                    pane.pointer, None,
                    "the dropped reading's crosshair cannot reappear on the new one"
                );
                assert_eq!(
                    pane.pointer_frame.take(),
                    None,
                    "the position queued for the dropped reading is gone too"
                );
                assert!(
                    pane.pulse.is_none(),
                    "the dropped reading's pulse stops with it"
                );
            });
            assert!(
                pane.read(cx).snapshot.is_none(),
                "a refreshed catalog generation cannot vouch for the chart read from it"
            );
        });

        settle_until(cx, "the reading the reload asked for", |cx| {
            pane.read(cx).has_content()
        });
        assert_eq!(
            source.ranges().len(),
            2,
            "a venue reload reads the market again instead of keeping the old stream"
        );
    }

    #[gpui_kit::test]
    fn a_failed_pane_should_ignore_a_venue_commit(cx: &mut TestAppContext) {
        let (pane, source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        let held = reading(3, PredictionRange::All, &[1_000]);
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(held.clone());
                pane.status.report(
                    WidgetState::failed(
                        Arc::new(PredictionMarketError::Fetch(
                            PredictionMarketFetchError::NoMarkets,
                        )),
                        WidgetAction::new("common.retry", |_| {}),
                    ),
                    cx,
                );
                let before = pane.status.state(cx);
                pane.on_venues_updated(
                    &VenuesUpdated {
                        venue: VenueId::Polymarket,
                        revision: 2,
                    },
                    cx,
                );
                assert_eq!(
                    pane.status.state(cx),
                    before,
                    "a failure is terminal: only the reader's recovery revives the pane"
                );
                assert_eq!(
                    pane.snapshot.as_ref().map(|snapshot| snapshot.revision),
                    Some(3),
                    "the failed reading is left exactly as it was"
                );
            });
        });
        assert!(
            source.ranges().is_empty(),
            "a failed pane does not ask the venue for a new reading"
        );
    }

    #[gpui_kit::test]
    fn an_empty_reading_should_keep_the_legend_and_the_ranges_around_an_empty_chart(
        cx: &mut TestAppContext,
    ) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(empty_reading());
                pane.status.report(WidgetState::Ready, cx);
            });
            assert!(
                pane.read(cx).has_content(),
                "an empty reading is the pane's content, not a placeholder"
            );
        });

        let (_, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(420.0)));
        reset_log();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("EMPTY_STATE").is_some(),
            "an empty reading says there is no data"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_LEGEND").is_some(),
            "the sides the empty reading is about are still named"
        );
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_CHART").is_some());
        let painted = take_log();
        assert!(
            painted.strokes.is_empty() && painted.texts.is_empty(),
            "a reading with no points paints no chart behind the empty state"
        );
    }

    #[gpui_kit::test]
    fn a_reading_with_points_should_draw_the_legend_above_the_chart(cx: &mut TestAppContext) {
        let config = PredictionMarketConfig {
            order_book_visible: false,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });

        let (_, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(420.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_LEGEND").is_some());
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_CHART").is_some());
        assert!(cx.debug_bounds("EMPTY_STATE").is_none());
    }

    #[gpui_kit::test]
    fn a_chart_snapshot_should_start_the_book_request_with_the_markets_legs(
        cx: &mut TestAppContext,
    ) {
        let (pane, source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| select_event(&pane, cx));
        settle_until(cx, "the book request", |_| {
            !source.book_targets().is_empty()
        });
        settle_until(cx, "the first book reading", |cx| {
            pane.read(cx).book_snapshot.is_some()
        });

        let (_, legs) = charted_market(&event_ref(), "charted");
        let targets = source.book_targets();
        assert_eq!(targets[0].event, event_ref());
        assert_eq!(
            targets[0].leg, legs[0],
            "the market's default side is the book the panel shows"
        );
        assert_eq!(
            targets[0]
                .legs
                .iter()
                .map(|leg| leg.leg.clone())
                .collect::<Vec<_>>(),
            legs,
            "the request carries every side of the market, in venue order, so it \
             shares the chart's topic lease"
        );
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.book_error.is_none());
            assert_eq!(
                pane.book_key.as_ref().map(|key| key.leg.clone()),
                Some(legs[0].clone())
            );
        });
    }

    #[gpui_kit::test]
    fn a_book_reading_from_a_book_the_pane_no_longer_follows_should_be_refused(
        cx: &mut TestAppContext,
    ) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, _| {
                pane.selection = Some(event_ref());
                pane.book_key = Some(book_stream_key(2));
                pane.book_epoch = 2;
            });
            assert!(
                !report_book_snapshot(&pane, book_stream_key(1), book_reading(), cx),
                "a reading of the previous book is refused"
            );
            assert!(pane.read(cx).book_snapshot.is_none());
            assert!(report_book_snapshot(
                &pane,
                book_stream_key(2),
                book_reading(),
                cx
            ));
            assert!(pane.read(cx).book_snapshot.is_some());
        });
    }

    #[gpui_kit::test]
    fn a_book_failure_should_be_shown_in_the_panel_and_recoverable(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                pane.status.report(WidgetState::Ready, cx);
            });
            assert!(report_book_status(
                &pane,
                book_stream_key(1),
                PredictionOrderBookStatus::Failed {
                    error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                },
                cx,
            ));
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "a book failure is the panel's, not the pane's"
            );
        });

        let (_, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(520.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_some());
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_RETRY")
                .is_some(),
            "the panel offers a way to read the book again"
        );

        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.retry_book(cx)));
        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(pane.book_error.is_none(), "a retry forgets the failure");
            assert_eq!(
                pane.book_key.as_ref().map(|key| key.epoch),
                Some(2),
                "the retry is a new generation of the same book"
            );
            assert!(pane.book_loading());
        });
    }

    #[gpui_kit::test]
    fn the_toggle_should_hide_the_book_panel_and_report_the_change(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(book_reading())),
                    cx,
                );
                pane.status.report(WidgetState::Ready, cx);
            });
        });

        let (_, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(520.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_some());
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_ROW").is_some(),
            "the book the panel holds is drawn row by row"
        );
        assert!(cx.debug_bounds("EMPTY_STATE").is_none());

        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        cx.update(|_, cx| assert!(!pane.read(cx).config.order_book_visible));
        assert_eq!(reports.get(), 1, "hiding the book is written back");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_none());
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_CHART").is_some(),
            "the chart keeps the whole body while the book is hidden"
        );

        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_some());
        assert_eq!(reports.get(), 2);
    }

    #[gpui_kit::test]
    fn the_full_book_panel_should_draw_its_header_ladder_and_spread_row(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        let cx = body_window(cx, &pane, size(px(900.0), px(520.0)));

        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_some());
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_HEADER")
                .is_some(),
            "the column labels are drawn above the ladder"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_ROW").is_some(),
            "the grouped levels are drawn"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_SPREAD")
                .is_some(),
            "the last and spread row is drawn between the two sides"
        );
        assert!(cx.debug_bounds("EMPTY_STATE").is_none());
    }

    #[gpui_kit::test]
    fn a_long_book_should_scroll_to_the_spread_row(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, long_book(60, 60), cx));
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));

        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_SPREAD")
                .is_some(),
            "the anchored spread row is the one the list is scrolled to"
        );
        let (scrollable, offset) = cx.update(|_, cx| {
            let scroll = pane.read(cx).book_panel.scroll.clone();
            let scrollable = scroll.is_scrollable();
            let offset = scroll.0.borrow().base_handle.offset().y;
            (scrollable, offset)
        });
        assert!(scrollable, "a book deeper than the panel scrolls");
        assert!(
            offset < px(0.0),
            "the viewport was moved down from the first ask: {offset:?}"
        );
        assert!(
            -offset >= px(22.0) && -offset <= px(61.0 * 22.0),
            "the spread row is at the ask count, and the asks above it stay visible: {offset:?}"
        );
    }

    #[gpui_kit::test]
    fn a_live_ask_should_not_move_the_spread_row(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, long_book(60, 60), cx));
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        let spread_y = |cx: &mut VisualTestContext| {
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_SPREAD")
                .expect("the anchored spread row is drawn")
                .origin
                .y
        };
        let before = spread_y(cx);

        // One more ask above the spread pushes every row below it down a row; the
        // reader's place is a screen position, so the viewport follows it.
        cx.update(|_, cx| {
            assert!(report_book_snapshot(
                &pane,
                book_stream_key(1),
                long_book(61, 60),
                cx
            ));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let after_addition = spread_y(cx);
        assert!(
            (before - after_addition).abs() <= px(1.0),
            "an ask added above the spread must not shift it: {before:?} -> {after_addition:?}"
        );

        // And the mirror: an ask removed above it.
        cx.update(|_, cx| {
            assert!(report_book_snapshot(
                &pane,
                book_stream_key(1),
                long_book(60, 60),
                cx
            ));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let after_removal = spread_y(cx);
        assert!(
            (before - after_removal).abs() <= px(1.0),
            "an ask removed above the spread must not shift it either: {before:?} -> {after_removal:?}"
        );

        cx.update(|_, cx| {
            let scroll = pane.read(cx).book_panel.scroll.clone();
            assert!(scroll.is_scrollable(), "the scroll handle still works");
            assert!(
                scroll.0.borrow().base_handle.offset().y < px(0.0),
                "the viewport is still scrolled to the spread"
            );
        });
    }

    #[gpui_kit::test]
    fn a_bottom_anchored_ladder_should_keep_its_spread_when_asks_arrive(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        // The window is laid out before the ladder arrives, so the first anchor is
        // computed against the panel the reader actually has.
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        // A book deep enough that the anchored spread row is at the very bottom of
        // the viewport: the offset is at the maximum the current content allows.
        cx.update(|_, cx| draw_book(&pane, long_book(400, 1), cx));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let spread_y = |cx: &mut VisualTestContext| {
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_SPREAD")
                .expect("the anchored spread row is drawn")
                .origin
                .y
        };
        let before = spread_y(cx);
        cx.update(|_, cx| {
            let scroll = pane.read(cx).book_panel.scroll.clone();
            assert!(
                scroll.is_scrollable(),
                "the ladder is deeper than the panel"
            );
            let handle = scroll.0.borrow();
            let offset = handle.base_handle.offset().y;
            let max = handle.base_handle.max_offset().y;
            assert!(offset < px(0.0), "the spread row is below the content top");
            assert!(
                (-offset - max).abs() <= px(1.0),
                "the anchored spread row sits at the very bottom of the content: \
                 offset {offset:?}, bound {max:?}"
            );
        });

        for (asks, added) in [(401, 1), (411, 10)] {
            cx.update(|_, cx| {
                assert!(report_book_snapshot(
                    &pane,
                    book_stream_key(1),
                    long_book(asks, 1),
                    cx
                ));
            });
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let after = spread_y(cx);
            assert!(
                (before - after).abs() <= px(1.0),
                "{added} ask(s) added above the spread must not move it: {before:?} -> {after:?}"
            );
        }

        // And the mirror: a level removed above the spread still moves the viewport
        // with it, whatever the content bound is.
        cx.update(|_, cx| {
            assert!(report_book_snapshot(
                &pane,
                book_stream_key(1),
                long_book(400, 1),
                cx
            ));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let after_removal = spread_y(cx);
        assert!(
            (before - after_removal).abs() <= px(1.0),
            "an ask removed above the spread must not shift it either: {before:?} -> {after_removal:?}"
        );
    }

    #[gpui_kit::test]
    fn an_empty_book_between_two_ladders_should_reanchor_the_spread(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        // The window is laid out before the ladder arrives, so every anchor below
        // is computed against the same panel.
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        cx.update(|_, cx| draw_book(&pane, long_book(60, 60), cx));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let spread_y = |cx: &mut VisualTestContext| {
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_SPREAD")
                .expect("the anchored spread row is drawn")
                .origin
                .y
        };
        let before = spread_y(cx);

        // The whole book is replaced by a page with no level at all — a same-leg,
        // same-tick Replace with empty sides, or a re-read that answered 404.
        cx.update(|_, cx| {
            assert!(report_book_snapshot(
                &pane,
                book_stream_key(1),
                empty_book(),
                cx
            ));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_SPREAD")
                .is_none(),
            "an empty ladder has no spread row to draw"
        );
        assert!(cx.debug_bounds("EMPTY_STATE").is_some());
        cx.update(|_, cx| {
            assert!(
                pane.read(cx).book_panel.anchor_pending,
                "the empty page leaves the spread waiting to be brought back"
            );
        });

        // The refilled ladder is a new ladder: it is anchored again instead of
        // being dragged as if its rows had been on screen all along.
        cx.update(|_, cx| {
            assert!(report_book_snapshot(
                &pane,
                book_stream_key(1),
                long_book(60, 60),
                cx
            ));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let after = spread_y(cx);
        assert!(
            (before - after).abs() <= px(1.0),
            "the refilled ladder anchors its spread where the first one was: {before:?} -> {after:?}"
        );
        cx.update(|_, cx| {
            let panel = &pane.read(cx).book_panel;
            assert!(
                !panel.anchor_pending,
                "the refill's first frame anchored it"
            );
            assert!(
                panel.scroll.is_scrollable(),
                "the refilled ladder is the one scrolling again"
            );
        });
    }

    #[gpui_kit::test]
    fn the_grouping_select_should_appear_only_with_a_book(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        let (_, cx): (Entity<HeaderHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| HeaderHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(300.0)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_GROUPING")
                .is_none(),
            "a pane with no book has no tick to group by"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_TOGGLE")
                .is_none(),
            "a pane with no reading has no book to show or hide"
        );

        cx.update(|_, cx| draw_book(&pane, book_reading(), cx));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_GROUPING")
                .is_some(),
            "a book offers the steps its tick can express"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_TOGGLE")
                .is_some(),
            "a supported reading offers the book again"
        );
    }

    #[gpui_kit::test]
    fn a_tick_with_no_step_should_not_offer_a_grouping(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        let (_, cx): (Entity<HeaderHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| HeaderHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(300.0)));
        cx.run_until_parked();

        cx.update(|_, cx| {
            draw_book(&pane, book_reading(), cx);
            pane.update(cx, |pane, cx| {
                let (_, legs) = charted_market(&event_ref(), "charted");
                let book = Arc::new(order_book(
                    legs[0].clone(),
                    "1",
                    vec![level("0.5", "10")],
                    Vec::new(),
                ));
                assert!(pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(book)),
                    cx,
                ));
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_GROUPING")
                .is_none(),
            "a tick of one share expresses no grouping step"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_TOGGLE")
                .is_some(),
            "the book itself is still a choice"
        );
    }

    #[gpui_kit::test]
    fn an_empty_book_should_not_offer_a_grouping(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        let (_, cx): (Entity<HeaderHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| HeaderHarness { pane: pane.clone() });
        cx.simulate_resize(size(px(900.0), px(300.0)));
        cx.run_until_parked();

        cx.update(|_, cx| draw_book(&pane, empty_book(), cx));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_GROUPING")
                .is_none(),
            "an empty book has nothing to regroup"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_TOGGLE")
                .is_some(),
            "the book itself is still a choice"
        );
    }

    #[gpui_kit::test]
    fn choosing_a_grouping_should_persist_and_regroup(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.book_grouping_label().map(|label| label.to_string()),
                Some("0.001".to_owned()),
                "the picker shows the step the ladder is built at"
            );
            assert_eq!(pane.book_panel.multiplier, 1);
        });

        cx.update(|cx| pane.update(cx, |pane, cx| pane.select_book_grouping(10, cx)));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.config.order_book_grouping, 10);
            assert_eq!(pane.book_panel.multiplier, 10);
        });
        assert_eq!(reports.get(), 1, "the choice is reported once");
    }

    #[gpui_kit::test]
    fn a_tick_change_should_reset_an_impossible_grouping(cx: &mut TestAppContext) {
        let reports = reports();
        let config = PredictionMarketConfig {
            order_book_grouping: 100,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports.clone());

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                let (_, legs) = charted_market(&event_ref(), "charted");
                let book = Arc::new(order_book(
                    legs[0].clone(),
                    "0.01",
                    vec![level("0.30", "10")],
                    vec![level("0.35", "10")],
                ));
                pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(book)),
                    cx,
                );
                assert_eq!(
                    pane.config.order_book_grouping, 1,
                    "a hundred-tick step is not a price at a cent tick"
                );
                assert_eq!(pane.book_panel.multiplier, 1);
            });
        });
        assert_eq!(reports.get(), 1, "the reset is written back");
    }

    #[gpui_kit::test]
    fn an_empty_page_should_not_rewrite_the_stored_grouping(cx: &mut TestAppContext) {
        let reports = reports();
        let config = PredictionMarketConfig {
            order_book_grouping: 100,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports.clone());
        let (_, legs) = charted_market(&event_ref(), "charted");

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                // A token with no CLOB book answers 404: the page states no level at
                // all, and its cent tick is a fallback rather than the venue's.
                let empty = Arc::new(order_book(
                    legs[0].clone(),
                    "0.01",
                    Vec::new(),
                    Vec::new(),
                ));
                assert!(pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(empty)),
                    cx,
                ));
                assert_eq!(
                    pane.config.order_book_grouping, 100,
                    "an empty page has no level to group, so its fallback tick must not reset the choice"
                );
                assert!(pane.book_panel.is_empty());
                assert_eq!(
                    pane.book_panel.multiplier, 100,
                    "the panel keeps the grouping it was drawing"
                );
            });
        });
        assert_eq!(
            reports.get(),
            0,
            "nothing was reset, so nothing is written back"
        );

        // The real book of the same market states a tick a grouping of a hundred is
        // a price at.
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                let book = Arc::new(order_book(
                    legs[0].clone(),
                    "0.001",
                    vec![level("0.34", "50")],
                    vec![level("0.35", "100")],
                ));
                assert!(pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(book)),
                    cx,
                ));
                assert_eq!(pane.config.order_book_grouping, 100);
                assert_eq!(pane.book_panel.multiplier, 100);
            });
        });
        assert_eq!(reports.get(), 0, "a valid grouping is never rewritten");
    }

    #[gpui_kit::test]
    fn the_splitter_should_be_drawn_only_while_the_book_is_visible(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(520.0)));

        let splitter = cx
            .debug_bounds("TEST_PREDICTION_MARKET_SPLITTER")
            .expect("the splitter sits between the chart and the book");
        assert_eq!(splitter.size.height, px(7.0), "the grab band is 7px");

        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_SPLITTER").is_none(),
            "a hidden book leaves the splitter out"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_CHART").is_some(),
            "the chart keeps the whole body while the book is hidden"
        );
    }

    #[gpui_kit::test]
    fn the_configured_ratio_should_move_the_panel_against_the_chart(cx: &mut TestAppContext) {
        let config = PredictionMarketConfig {
            order_book_ratio: 0.3,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(520.0)));

        let bounds = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is in the frame"))
        };
        let book = bounds(cx, "TEST_PREDICTION_MARKET_BOOK");
        let chart = bounds(cx, "TEST_PREDICTION_MARKET_CHART");
        let splitter = bounds(cx, "TEST_PREDICTION_MARKET_SPLITTER");
        let share = |book: Bounds<Pixels>, chart: Bounds<Pixels>, splitter: Bounds<Pixels>| {
            f32::from(book.size.height)
                / (f32::from(book.size.height)
                    + f32::from(chart.size.height)
                    + f32::from(splitter.size.height))
        };
        let smallest = share(book, chart, splitter);
        assert!(
            (smallest - 0.3).abs() < 0.02,
            "the book takes the configured share of the area: {smallest}"
        );

        cx.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.config.order_book_ratio = 0.7;
                cx.notify();
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let grown = bounds(cx, "TEST_PREDICTION_MARKET_BOOK");
        let squeezed = bounds(cx, "TEST_PREDICTION_MARKET_CHART");
        let splitter = bounds(cx, "TEST_PREDICTION_MARKET_SPLITTER");
        assert!(grown.size.height > book.size.height, "more of the area");
        assert!(squeezed.size.height < chart.size.height, "less chart");
        let largest = share(grown, squeezed, splitter);
        assert!(
            (largest - 0.7).abs() < 0.02,
            "the panel follows the stored ratio: {largest}"
        );
    }

    /// One whole splitter drag: press at `start`, cross the drag threshold and
    /// release `travel` away from the press.
    ///
    /// The threshold move is the one that starts the drag; only the second move is
    /// a drag move, which is what a real pointer produces too.
    fn splitter_drag(cx: &mut VisualTestContext, start: Point<Pixels>, travel: Pixels) {
        let threshold = if travel < px(0.0) { px(-4.0) } else { px(4.0) };
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(start.x, start.y + threshold),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(start.x, start.y + travel),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(start.x, start.y + travel),
            MouseButton::Left,
            Modifiers::none(),
        );
    }

    /// Draws the fixture chart in a body window, and answers the splitter's bounds
    /// and the area the splitter divides.
    fn splitter_and_area(
        cx: &mut VisualTestContext,
        pane: &Entity<PredictionMarketWidget>,
    ) -> (Bounds<Pixels>, Pixels) {
        let splitter = cx
            .debug_bounds("TEST_PREDICTION_MARKET_SPLITTER")
            .expect("the splitter sits between the chart and the book");
        let area = cx
            .update(|_, cx| pane.read(cx).split_bounds.get())
            .expect("the body was laid out")
            .size
            .height;
        (splitter, area)
    }

    #[gpui_kit::test]
    fn a_splitter_drag_should_follow_the_pointer_and_report_once(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        let (splitter, area) = splitter_and_area(cx, &pane);

        splitter_drag(cx, splitter.center(), px(46.0));

        let expected = 0.5 - 46.0 / f32::from(area);
        cx.update(|_, cx| {
            let pane = pane.read(cx);
            let ratio = pane.config.order_book_ratio;
            assert!(
                (ratio - expected).abs() < 1e-3,
                "the notch follows the pointer: {ratio} vs {expected}"
            );
            assert!(
                (ratio - 0.4).abs() < 0.02,
                "46px in a {area:?} area is about two fifths, not the smallest share: {ratio}"
            );
            assert!(pane.split_drag.is_none(), "the gesture's origin is cleared");
            assert!(!pane.split_dragging, "and the gesture is no longer running");
        });
        assert_eq!(reports.get(), 1, "the released notch is written back once");
    }

    #[gpui_kit::test]
    fn a_splitter_drag_up_should_grow_the_book(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        let (splitter, area) = splitter_and_area(cx, &pane);

        splitter_drag(cx, splitter.center(), px(-46.0));

        let expected = 0.5 + 46.0 / f32::from(area);
        cx.update(|_, cx| {
            let ratio = pane.read(cx).config.order_book_ratio;
            assert!(
                (ratio - expected).abs() < 1e-3,
                "dragging up gives the book more: {ratio} vs {expected}"
            );
            assert!(ratio > 0.5, "the notch moved the other way: {ratio}");
        });
        assert_eq!(reports.get(), 1, "the released notch is written back once");
    }

    #[gpui_kit::test]
    fn a_splitter_drag_past_its_bounds_should_stop_at_the_clamp(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));

        let (splitter, _area) = splitter_and_area(cx, &pane);
        splitter_drag(cx, splitter.center(), px(1_000.0));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                pane.read(cx).config.order_book_ratio,
                0.2,
                "a drag past the smallest share stops at it"
            );
        });

        let (splitter, _area) = splitter_and_area(cx, &pane);
        splitter_drag(cx, splitter.center(), px(-1_000.0));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                pane.read(cx).config.order_book_ratio,
                0.8,
                "a drag past the largest share stops at it"
            );
        });
        assert_eq!(reports.get(), 2, "each released notch is written back once");
    }

    #[gpui_kit::test]
    fn a_click_outside_the_splitter_should_not_report_the_ratio(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        let chart = cx
            .debug_bounds("TEST_PREDICTION_MARKET_CHART")
            .expect("the chart is in the frame");

        cx.simulate_click(chart.center(), Modifiers::none());

        // The release listener on the splitter hears this click too: it is a
        // window-wide capture listener. A gesture that never moved the notch must
        // not write the ratio back.
        let (splitter, _area) = splitter_and_area(cx, &pane);
        let start = splitter.center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(
            point(start.x, start.y + px(120.0)),
            MouseButton::Left,
            Modifiers::none(),
        );

        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.config.order_book_ratio, 0.5);
            assert!(pane.split_drag.is_none(), "the press is cleared");
            assert!(!pane.split_dragging);
        });
        assert_eq!(
            reports.get(),
            0,
            "a click that is not a splitter drag reports nothing"
        );
    }

    #[gpui_kit::test]
    fn a_splitter_gesture_interrupted_by_hiding_should_not_report_its_notch(
        cx: &mut TestAppContext,
    ) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        let (splitter, _area) = splitter_and_area(cx, &pane);
        let start = splitter.center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(start.x, start.y + px(4.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(start.x, start.y + px(60.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.update(|_, cx| {
            let ratio = pane.read(cx).config.order_book_ratio;
            assert!(ratio < 0.5, "the drag moved the notch: {ratio}");
            // The book is hidden while the pointer is still down and the drag's
            // preview is running.
            pane.update(cx, |pane, cx| pane.toggle_book(cx));
        });
        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(
                pane.split_drag.is_none(),
                "hiding the book aborts the interrupted gesture"
            );
            assert!(!pane.split_dragging);
            assert!(!pane.config.order_book_visible);
        });
        assert_eq!(
            reports.get(),
            1,
            "hiding reports the toggle, not the notch the gesture had reached"
        );

        // Showing it again leaves a splitter whose window-wide release listener
        // hears the next click: a gesture with no press armed is not a gesture.
        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let chart = cx
            .debug_bounds("TEST_PREDICTION_MARKET_CHART")
            .expect("the chart is in the frame");
        cx.simulate_click(chart.center(), Modifiers::none());

        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(pane.split_drag.is_none(), "the later click arms nothing");
            assert!(!pane.split_dragging);
        });
        assert_eq!(
            reports.get(),
            2,
            "showing the book reports once more, and the later click reports nothing"
        );
    }

    #[gpui_kit::test]
    fn a_splitter_drag_back_to_its_start_should_not_report(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        let (splitter, _area) = splitter_and_area(cx, &pane);
        let start = splitter.center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(start.x, start.y + px(4.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(start.x, start.y + px(80.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        // The pointer comes back to where it went down before it is released.
        cx.simulate_mouse_move(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(start, MouseButton::Left, Modifiers::none());

        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.config.order_book_ratio, 0.5,
                "the notch is where the gesture started"
            );
            assert!(pane.split_drag.is_none());
            assert!(!pane.split_dragging);
        });
        assert_eq!(
            reports.get(),
            0,
            "a drag that ended where it started chose nothing and reports nothing"
        );
    }

    #[gpui_kit::test]
    fn a_short_pane_should_not_store_a_notch_below_the_drawn_minimum(cx: &mut TestAppContext) {
        let reports = reports();
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports.clone());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000, 2_000]));
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(160.0)));
        let (splitter, area) = splitter_and_area(cx, &pane);

        splitter_drag(cx, splitter.center(), px(1_000.0));

        let expected = MIN_BOOK_RATIO.max(f32::from(MIN_BOOK_HEIGHT) / f32::from(area));
        cx.update(|_, cx| {
            let ratio = pane.read(cx).config.order_book_ratio;
            assert!(
                (ratio - expected).abs() < 1e-3,
                "the stored notch is the smallest share the panel is drawn at: {ratio} vs {expected}"
            );
            assert!(
                ratio > MIN_BOOK_RATIO,
                "a short pane's minimum height is more than the tall pane's constant: {ratio}"
            );
            assert!(ratio < MAX_BOOK_RATIO);
        });
        assert_eq!(reports.get(), 1, "the released notch is written back once");
    }

    #[gpui_kit::test]
    fn a_venue_reload_should_start_the_book_over(cx: &mut TestAppContext) {
        let (pane, source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| select_event(&pane, cx));
        settle_until(cx, "the first book reading", |cx| {
            pane.read(cx).book_snapshot.is_some()
        });
        assert_eq!(source.book_targets().len(), 1);

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.on_venues_updated(
                    &VenuesUpdated {
                        venue: VenueId::Polymarket,
                        revision: 2,
                    },
                    cx,
                );
                assert!(
                    pane.book_key.is_none(),
                    "the book of the replaced catalog is dropped"
                );
                assert!(
                    pane.book_panel.grouped.is_none(),
                    "and so is the ladder drawn from it"
                );
            });
        });

        settle_until(cx, "the book the reload asked for", |_| {
            source.book_targets().len() >= 2
        });
        assert_eq!(
            source.book_targets().len(),
            2,
            "the next chart reading points the book at its leg again"
        );
    }

    #[gpui_kit::test]
    fn a_book_error_with_a_reading_should_draw_a_strip_above_the_panel(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        cx.update(|cx| {
            assert!(report_book_status(
                &pane,
                book_stream_key(1),
                PredictionOrderBookStatus::Failed {
                    error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                },
                cx,
            ));
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(520.0)));

        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_ERROR")
                .is_some(),
            "the failure is one line above the ladder it interrupted"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_HEADER")
                .is_some(),
            "the last reading is still the ladder the reader was reading"
        );
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_ROW").is_some());
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_RETRY")
                .is_some(),
            "and it offers a way to read the book again"
        );
    }

    #[gpui_kit::test]
    fn the_book_region_should_say_loading_and_then_no_data(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                pane.status.report(WidgetState::Ready, cx);
            });
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(520.0)));
        assert!(cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_some());
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_HEADER")
                .is_none(),
            "a book that has not arrived has no ladder"
        );
        assert!(
            cx.debug_bounds("EMPTY_STATE").is_none(),
            "loading is not an empty book"
        );

        cx.update(|_, cx| draw_book(&pane, empty_book(), cx));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("EMPTY_STATE").is_some(),
            "a book with no resting level says there is no data"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_HEADER")
                .is_some(),
            "the column labels stay above the empty ladder"
        );
    }

    #[gpui_kit::test]
    fn a_hyperliquid_event_should_not_ask_for_an_order_book(cx: &mut TestAppContext) {
        let (pane, source) = pane_with_market(
            cx,
            PredictionMarketConfig::default(),
            reports(),
            &[VenueId::Hyperliquid],
            &[(VenueId::Hyperliquid, hyperliquid_prediction_scopes())],
        );

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(hyperliquid_event_ref());
                assert!(pane.apply_latest(
                    hyperliquid_stream_key(),
                    Arc::new(PredictionMarketStreamMessage::Snapshot(
                        hyperliquid_reading()
                    )),
                    cx,
                ));
            });
        });

        assert!(
            source.book_targets().is_empty(),
            "Hyperliquid publishes prediction events but no order book"
        );
        cx.update(|cx| {
            {
                let pane = pane.read(cx);
                assert!(pane.book_key.is_none(), "no book stream is followed");
                assert!(pane.book_snapshot.is_none());
                assert!(
                    !PredictionMarketWidget::book_supported(&hyperliquid_event_ref()),
                    "this build cannot read a Hyperliquid book"
                );
                assert!(
                    PredictionMarketWidget::book_supported(&event_ref()),
                    "Polymarket is the venue that answers one"
                );
            }
            // A retry must not resurrect a request the venue cannot answer.
            pane.update(cx, |pane, cx| pane.retry_book(cx));
            assert!(pane.read(cx).book_key.is_none());
        });
        assert!(
            source.book_targets().is_empty(),
            "a retry of an unsupported book asks for nothing"
        );

        // The header still draws the event and range controls: only the book's
        // own controls are absent.
        {
            let (_, header_cx): (Entity<HeaderHarness>, &mut VisualTestContext) =
                cx.add_window_view(|_, _| HeaderHarness { pane: pane.clone() });
            header_cx.simulate_resize(size(px(900.0), px(300.0)));
            header_cx.update(|window, _| window.refresh());
            header_cx.run_until_parked();
            assert!(
                header_cx
                    .debug_bounds("TEST_PREDICTION_MARKET_RANGE")
                    .is_some(),
                "the header is drawn for a reading with no book"
            );
            assert!(
                header_cx
                    .debug_bounds("TEST_PREDICTION_MARKET_BOOK_TOGGLE")
                    .is_none(),
                "a venue with no book offers no toggle"
            );
            assert!(
                header_cx
                    .debug_bounds("TEST_PREDICTION_MARKET_BOOK_GROUPING")
                    .is_none()
            );
        }

        let cx = body_window(cx, &pane, size(px(900.0), px(600.0)));
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK").is_none(),
            "no panel is drawn for a venue with no book"
        );
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_SPLITTER").is_none(),
            "and no splitter either"
        );
        let chart = cx
            .debug_bounds("TEST_PREDICTION_MARKET_CHART")
            .expect("the chart is drawn");
        assert!(
            chart.size.height > px(500.0),
            "the chart keeps the whole body: {:?}",
            chart.size.height
        );
    }

    #[gpui_kit::test]
    fn a_hidden_book_should_not_read_until_it_is_shown(cx: &mut TestAppContext) {
        let config = PredictionMarketConfig {
            order_book_visible: false,
            ..PredictionMarketConfig::default()
        };
        let reports = reports();
        let (pane, source) = pane_with(cx, config, reports.clone());
        cx.update(|cx| select_event(&pane, cx));
        settle_until(cx, "the chart reading", |cx| pane.read(cx).has_content());
        assert!(
            source.book_targets().is_empty(),
            "a hidden book asks for nothing while the chart loads"
        );
        cx.update(|cx| {
            assert!(pane.read(cx).book_key.is_none());
            assert!(!pane.read(cx).config.order_book_visible);
        });

        // Showing it starts the reading the chart already points at.
        cx.update(|cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        settle_until(cx, "the book request the shown panel asks for", |_| {
            !source.book_targets().is_empty()
        });
        settle_until(cx, "the first book reading", |cx| {
            pane.read(cx).book_snapshot.is_some()
        });
        assert_eq!(source.book_targets().len(), 1);
        assert_eq!(reports.get(), 1);

        // Hiding retires the stream but keeps the ladder the reader had.
        let closed_before = source.book_streams_closed();
        cx.update(|cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.config.order_book_visible);
            assert!(pane.book_key.is_none(), "a hidden book follows no stream");
            assert!(pane.book_snapshot.is_some(), "its last ladder is kept");
        });
        settle_until(cx, "the stopped book stream", |_| {
            source.book_streams_closed() > closed_before
        });
        assert_eq!(
            source.book_targets().len(),
            1,
            "hiding asks for nothing new"
        );
        assert_eq!(reports.get(), 2);

        // A reading that lands while the panel is hidden is stored, not drawn.
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.config.order_book_visible = false;
                pane.book_key = Some(book_stream_key(1));
                let accepted = pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(long_book(2, 2))),
                    cx,
                );
                assert!(
                    !accepted,
                    "a hidden panel stores the book but asks for no repaint"
                );
                assert!(pane.book_snapshot.is_some());
                pane.book_key = None;
            });
        });

        // Showing it again is instant: the kept ladder is drawn while the new
        // request runs.
        cx.update(|cx| pane.update(cx, |pane, cx| pane.toggle_book(cx)));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.config.order_book_visible);
            assert!(pane.book_key.is_some(), "showing starts a new request");
            assert!(
                pane.book_snapshot.is_some(),
                "the ladder kept in memory is drawn meanwhile"
            );
        });
        settle_until(cx, "the second book request", |_| {
            source.book_targets().len() == 2
        });
        assert_eq!(reports.get(), 3);

        let cx = body_window(cx, &pane, size(px(900.0), px(520.0)));
        assert!(
            cx.debug_bounds("TEST_PREDICTION_MARKET_BOOK_ROW").is_some(),
            "the ladder kept while the panel was hidden is what it draws"
        );
    }

    #[gpui_kit::test]
    fn a_late_snapshot_after_a_book_failure_should_be_refused(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                pane.status.report(WidgetState::Ready, cx);
            });
            assert!(report_book_status(
                &pane,
                book_stream_key(1),
                PredictionOrderBookStatus::Failed {
                    error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                },
                cx,
            ));
            assert!(
                !report_book_snapshot(&pane, book_stream_key(1), book_reading(), cx),
                "a snapshot of the failed generation is not an answer"
            );
            let pane = pane.read(cx);
            assert!(pane.book_error.is_some(), "the failure remains the answer");
            assert!(pane.book_snapshot.is_none(), "no stale ladder is drawn");
            assert!(pane.book_panel.grouped.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_retry_should_keep_the_last_ladder(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        cx.update(|cx| {
            assert!(report_book_status(
                &pane,
                book_stream_key(1),
                PredictionOrderBookStatus::Failed {
                    error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                },
                cx,
            ));
            pane.update(cx, |pane, cx| pane.retry_book(cx));

            let pane = pane.read(cx);
            assert!(pane.book_error.is_none(), "a retry forgets the failure");
            assert!(
                pane.book_snapshot.is_some(),
                "the ladder the reader was reading stays on screen"
            );
            assert!(pane.book_panel.grouped.is_some());
            assert_eq!(
                pane.book_key.as_ref().map(|key| key.epoch),
                Some(2),
                "the retry is a new generation of the same book"
            );
            assert!(
                !pane.book_loading(),
                "the panel shows the kept ladder rather than loading"
            );
        });
    }

    #[gpui_kit::test]
    fn a_book_message_should_be_refused_once_the_pane_failed_or_waits(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, _| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
            });

            // A pane waiting for its market has no book to report.
            pane.update(cx, |pane, cx| {
                pane.awaiting_market = true;
                cx.notify();
            });
            assert!(!report_book_snapshot(
                &pane,
                book_stream_key(1),
                book_reading(),
                cx
            ));
            assert!(!report_book_status(
                &pane,
                book_stream_key(1),
                PredictionOrderBookStatus::Failed {
                    error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                },
                cx,
            ));
            assert!(pane.read(cx).book_snapshot.is_none());
            assert!(pane.read(cx).book_error.is_none());

            // A failed pane is terminal: neither channel revives it.
            pane.update(cx, |pane, cx| {
                pane.awaiting_market = false;
                pane.status.report(
                    WidgetState::failed(
                        Arc::new(PredictionMarketError::Fetch(
                            PredictionMarketFetchError::NoMarkets,
                        )),
                        WidgetAction::new("common.retry", |_| {}),
                    ),
                    cx,
                );
            });
            assert!(
                !report_book_snapshot(&pane, book_stream_key(1), book_reading(), cx),
                "a failed pane takes no book reading"
            );
            assert!(
                !report_book_status(
                    &pane,
                    book_stream_key(1),
                    PredictionOrderBookStatus::Failed {
                        error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                    },
                    cx,
                ),
                "nor a book status"
            );
            let pane = pane.read(cx);
            assert!(pane.book_snapshot.is_none());
            assert!(pane.book_error.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_terminal_chart_failure_should_release_the_book_lease(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        cx.update(|cx| {
            report_status(
                &pane,
                stream_key(),
                PredictionMarketStatus::Failed {
                    error: PredictionMarketFetchError::NoMarkets,
                },
                cx,
            );
            let pane = pane.read(cx);
            assert!(pane.status.is_failed(cx));
            assert!(
                pane.book_key.is_none(),
                "the book's stream is retired with the chart's terminal failure"
            );
            assert!(
                pane.book_snapshot.is_some(),
                "the last ladder the reader was reading stays on screen"
            );
            assert!(pane.book_panel.grouped.is_some());
        });
    }

    #[gpui_kit::test]
    fn a_chart_whose_market_is_not_loaded_should_clear_the_book(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        cx.update(|cx| {
            let error = PredictionMarketFetchError::Market(MarketError::Catalog(Box::new(
                CatalogError::CatalogNotLoaded {
                    route: event_ref().route,
                },
            )));
            report_status(
                &pane,
                stream_key(),
                PredictionMarketStatus::Failed { error },
                cx,
            );
            let pane = pane.read(cx);
            assert!(pane.awaiting_market, "the chart waits for its market");
            assert!(pane.book_key.is_none(), "the book's stream is retired");
            assert!(
                pane.book_snapshot.is_none(),
                "the book of a market not in memory is dropped"
            );
            assert!(pane.book_panel.grouped.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_load_without_a_loaded_market_should_clear_the_book(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| draw_book(&pane, book_reading(), cx));
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(cut_event_ref());
                pane.load(cx);
            });
            let pane = pane.read(cx);
            assert!(pane.awaiting_market);
            assert!(pane.book_key.is_none());
            assert!(pane.book_snapshot.is_none());
            assert!(pane.book_panel.grouped.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_venue_unload_should_drop_the_book(cx: &mut TestAppContext) {
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(cut_event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                assert!(pane.apply_book_latest(
                    book_stream_key(1),
                    Arc::new(PredictionOrderBookStreamMessage::Snapshot(book_reading())),
                    cx,
                ));
                pane.status.report(WidgetState::Ready, cx);
            });
            assert!(pane.read(cx).book_snapshot.is_some());

            // A commit on another venue shrinks the loaded view under this pane:
            // its market is gone, and the book read for it cannot be vouched for.
            pane.update(cx, |pane, cx| {
                pane.on_venues_updated(
                    &VenuesUpdated {
                        venue: VenueId::Hyperliquid,
                        revision: 2,
                    },
                    cx,
                );
                assert!(pane.awaiting_market);
                assert!(pane.book_key.is_none());
                assert!(
                    pane.book_snapshot.is_none(),
                    "the book of a market nothing holds is dropped"
                );
                assert!(pane.book_panel.grouped.is_none());
            });
        });
    }

    #[gpui_kit::test]
    fn a_failed_book_should_fit_its_retry_in_a_short_pane(cx: &mut TestAppContext) {
        let config = PredictionMarketConfig {
            order_book_ratio: 0.2,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports());
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.selection = Some(event_ref());
                pane.snapshot = Some(reading(1, PredictionRange::All, &[1_000]));
                pane.book_key = Some(book_stream_key(1));
                pane.book_epoch = 1;
                pane.status.report(WidgetState::Ready, cx);
            });
            assert!(report_book_status(
                &pane,
                book_stream_key(1),
                PredictionOrderBookStatus::Failed {
                    error: PredictionOrderBookError::Timeout(Duration::from_secs(10)),
                },
                cx,
            ));
        });
        let cx = body_window(cx, &pane, size(px(900.0), px(160.0)));

        let book = cx
            .debug_bounds("TEST_PREDICTION_MARKET_BOOK")
            .expect("the panel is drawn");
        let retry = cx
            .debug_bounds("TEST_PREDICTION_MARKET_BOOK_RETRY")
            .expect("the failure offers a retry");
        assert!(
            book.size.height >= MIN_BOOK_HEIGHT,
            "a short pane still gives the panel a usable height: {:?}",
            book.size.height
        );
        assert!(
            retry.origin.y >= book.origin.y,
            "the retry is not clipped at the top: {retry:?} vs {book:?}"
        );
        assert!(
            retry.origin.y + retry.size.height <= book.origin.y + book.size.height,
            "the retry bottom is inside the panel: {retry:?} vs {book:?}"
        );
    }

    #[gpui_kit::test]
    fn a_stored_book_choice_this_build_does_not_offer_should_be_reset_and_reported(
        cx: &mut TestAppContext,
    ) {
        let reports = reports();
        let config = PredictionMarketConfig {
            order_book_ratio: 5.0,
            order_book_grouping: 7,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports.clone());

        cx.update(|cx| pane.update(cx, |pane, cx| pane.load(cx)));

        cx.update(|cx| {
            let config = pane.read(cx).config;
            assert_eq!(config.order_book_ratio, 0.5, "an impossible notch is reset");
            assert_eq!(
                config.order_book_grouping, 1,
                "an unknown grouping is reset"
            );
        });
        assert_eq!(
            reports.get(),
            1,
            "the reset is reported once so the corrected config is written back"
        );
    }

    #[gpui_kit::test]
    fn a_supported_book_choice_should_be_kept_and_not_reported(cx: &mut TestAppContext) {
        let reports = reports();
        let config = PredictionMarketConfig {
            order_book_ratio: 0.3,
            order_book_grouping: 10,
            ..PredictionMarketConfig::default()
        };
        let (pane, _source) = pane_with(cx, config, reports.clone());

        cx.update(|cx| pane.update(cx, |pane, cx| pane.load(cx)));

        cx.update(|cx| {
            let config = pane.read(cx).config;
            assert_eq!(config.order_book_ratio, 0.3);
            assert_eq!(config.order_book_grouping, 10);
        });
        assert_eq!(reports.get(), 0, "a supported choice is left alone");
    }

    #[test]
    fn the_config_should_round_trip() {
        let config = PredictionMarketConfig {
            range: PredictionRange::Day1,
            order_book_visible: false,
            order_book_ratio: 0.3,
            order_book_grouping: 100,
        };

        let encoded = serde_json::to_string(&config).expect("the config encodes");
        let decoded: PredictionMarketConfig =
            serde_json::from_str(&encoded).expect("the config decodes");

        assert_eq!(decoded, config);
        assert_eq!(
            encoded,
            r#"{"range":"day_1","order_book_visible":false,"order_book_ratio":0.3,"order_book_grouping":100}"#
        );
    }
}
