//! The prediction market pane.
//!
//! The pane follows one event and draws one line per option. It loads a bounded
//! history and then follows the venue's live probability stream; the event
//! selector is the header control, and a venue reload drops the chart it can no
//! longer vouch for. The range the history is read over is the pane's one persisted
//! option: the header's timeframe picker states the six ranges the venue publishes,
//! and a switch drops the chart while the new reading lands under the framework's
//! connecting spinner.

use std::{
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::manager::ConfigManager;
use base_ui::{
    empty_state::EmptyState,
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, Context, CursorStyle, ElementId, Entity, InteractiveElement, IntoElement,
    MouseMoveEvent, ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, div, prelude::FluentBuilder, px,
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
    prediction::{PredictionEventRef, PredictionMarketGroup, PredictionMarketRef, PredictionRange},
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
    chart::canvas::{ChartCanvasOptions, ChartPaintCaches, chart_element},
    model::{PredictionMarketChoice, PredictionMarketSnapshot},
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
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PredictionMarketConfig {
    /// The range the history is read over.
    #[serde(rename = "range")]
    pub range: PredictionRange,
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

/// The prediction market.
pub struct PredictionMarketWidget {
    config: PredictionMarketConfig,
    config_change: WidgetConfigChange,
    selection: Option<PredictionEventRef>,
    selection_title: Option<SmolStr>,
    market: Option<PredictionMarketRef>,
    binding: WidgetBinding<PredictionMarketProtocol>,
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
        if !self.market_ready(cx) {
            self.awaiting_market = true;
            self.snapshot = None;
            self.forget_chart_state();
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
        WidgetHeader::empty()
            .left(left)
            .right(self.range_select(cx))
    }
}

impl PredictionMarketWidget {
    /// The seam a test answers through.
    pub fn new_with_source<S>(
        config: PredictionMarketConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: PredictionMarketSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            PredictionMarketService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
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

    fn select(&mut self, event: PredictionEventRef, title: SmolStr, cx: &mut Context<Self>) {
        self.selection = Some(event);
        self.selection_title = Some(title);
        self.market = None;
        self.snapshot = None;
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
                    self.report_notice(cx);
                    cx.notify();
                    return false;
                }
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
            // A refresh replaces the catalog generation the stream was opened
            // against. The old chart cannot be vouched for any more.
            self.snapshot = None;
            self.forget_chart_state();
            self.load(cx);
            cx.notify();
            return;
        }
        if !self.market_ready(cx) {
            self.awaiting_market = true;
            self.snapshot = None;
            self.forget_chart_state();
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

impl Render for PredictionMarketWidget {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is cached, and a cached subtree is only repainted when its own
        // view says so. A running pulse is exactly that case, so it asks for one frame
        // at a time and stops asking as soon as the effect has ended.
        let reduce_motion = cx.reduce_motion();
        let pulse_phase = self
            .pulse
            .and_then(|pulse| pulse.phase(Instant::now()))
            .filter(|_| !reduce_motion);
        if pulse_phase.is_some() {
            window.request_animation_frame();
        } else {
            self.pulse = None;
        }

        let Some(snapshot) = self.snapshot.clone() else {
            return div().size_full().min_h_0().into_any_element();
        };
        let has_points = snapshot.series.iter().any(|line| !line.points.is_empty());
        let mut body = div().size_full().min_h_0().flex().flex_col();
        // A reading with no points still carries one series per side: the legend is
        // what names them, so it stays exactly as long as the snapshot does.
        if !snapshot.series.is_empty() {
            body = body.child(self.legend(&snapshot, cx));
        }
        body.child(self.chart_area(&snapshot, has_points, pulse_phase, cx))
            .into_any_element()
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
        AppContext, Entity, IntoElement, ParentElement, Render, TestAppContext, VisualTestContext,
        Window, div, point, px, size,
    };
    use market_participants_connector::{
        connector::MarketParticipantsConnector, market::error::MarketError,
    };
    use market_participants_model::{
        decimal::{Fraction, Ratio},
        prediction::{PredictionEventView, PredictionLeg, PredictionPricePoint, PredictionSeries},
    };
    use widget_tokio_runtime::global;

    use super::*;
    use crate::{
        chart::paint::{reset_log, take_log},
        model::PredictionMarketSnapshot,
        service::{
            PredictionMarketSource, PredictionMarketStreamKey, PredictionMarketStreamMessage,
            PredictionSeriesEventSubscription,
        },
        test_support::{charted_market, event_ref, event_with},
    };

    /// A source that answers one event and one two-point history, and records the range
    /// every history request asked for.
    ///
    /// The request itself is not observable from the pane: it crosses the service, so
    /// the source is the only place a test can see which range was read.
    #[derive(Clone, Default)]
    struct FakeSource {
        ranges: Arc<Mutex<Vec<PredictionRange>>>,
    }

    impl FakeSource {
        fn ranges(&self) -> Vec<PredictionRange> {
            self.ranges
                .lock()
                .expect("the fake's range log is not poisoned")
                .clone()
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

    /// A live stream that publishes nothing: every state a test asserts is reported to
    /// the pane directly, so a source that spoke would race the assertion.
    struct PendingSubscription;

    impl PredictionSeriesEventSubscription for PendingSubscription {
        async fn recv(&mut self) -> Option<Result<Vec<PredictionSeries>, MarketError>> {
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

    /// Installs the application state a pane is built on, and commits the one loaded
    /// venue that makes the pane's market gate pass.
    ///
    /// A pane only reads a market that is in the loaded-venues view, and the fixture
    /// seam is how a test commits one without a venue answering.
    fn install(cx: &mut App) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let manager = ConfigManager::in_memory(Config::default(), cx);
        cx.set_global(manager);
        MarketParticipantsConnector::install(cx)
            .expect("the market participants root should install");
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(&[VenueId::Polymarket]),
                &[],
                &[],
            )
            .expect("the prediction venue should load");
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
        cx.executor().allow_parking();
        cx.update(install);
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
        let (pane, _source) = pane_with(cx, PredictionMarketConfig::default(), reports());
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
}
