use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconKind,
        select::{Select, SelectItem, SelectOptions},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::role::Role,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::{
    connector::MarketParticipantsConnector,
    venue::definition::{ScopeEntry, venues_supporting},
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    event::{Event, InputEvent},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::{TranslatedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    util::{format_countdown, unix_millis},
    venue::{VenueDisplay, route_display_string},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

#[cfg(test)]
use crate::service::VolumeHeatMapSource;
use crate::{
    chart::VolumeHeatMapChart,
    model::HeatMapModelError,
    service::{
        VolumeHeatMapFetchError, VolumeHeatMapProtocol, VolumeHeatMapRequest, VolumeHeatMapService,
        VolumeHeatMapServiceError, VolumeHeatMapStatus, VolumeHeatMapStreamKey,
        VolumeHeatMapStreamMessage,
    },
};

pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

const REFRESH_KEY_SUFFIX: &str = "volume_heatmap.header.refresh";
const VENUE_KEY_SUFFIX: &str = "volume_heatmap.header.venue";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeHeatMapConfig {
    #[serde(rename = "venue")]
    pub venue: Option<VenueId>,
}

impl WidgetConfig for VolumeHeatMapConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Reading {
    venue: VenueId,
    routes: Arc<[VenueRoute]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VolumeHeatMapFailure {
    Fetch(VolumeHeatMapFetchError),
    Rpc(RpcCallError<VolumeHeatMapServiceError>),
}

impl TranslatedError for VolumeHeatMapFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for VolumeHeatMapFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::MarketParticipant(error) => error.translated_message(translate),
            Self::Model(error) => error.translated_message(translate),
            Self::TooManyTickers {
                route,
                count,
                limit,
            } => t_smol!(
                translate,
                "error.volume_heatmap.too_many_tickers",
                route = route_display_string(route, translate),
                count = count,
                limit = limit
            ),
            Self::Timeout(timeout) => t_smol!(
                translate,
                "error.volume_heatmap.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl TranslatedError for HeatMapModelError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::NotionalOverflow { .. } => {
                t_smol!(translate, "error.volume_heatmap.notional_overflow")
            }
            Self::AssetNotNamed { .. } => {
                t_smol!(translate, "error.volume_heatmap.asset_not_named")
            }
        }
    }
}

impl TranslatedError for VolumeHeatMapServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t_smol!(translate, "error.volume_heatmap.stream_unavailable")
            }
        }
    }
}

fn is_waiting_for_a_catalog(failure: &VolumeHeatMapFailure) -> bool {
    matches!(
        failure,
        VolumeHeatMapFailure::Fetch(VolumeHeatMapFetchError::MarketParticipant(error))
            if market_is_catalog_not_loaded(error)
    )
}

fn reads_the_whole_market(entry: &ScopeEntry) -> bool {
    entry.catalog_source.is_some()
        && entry
            .capabilities
            .iter()
            .any(|support| support.capability == CapabilityId::AllTickers)
}

fn readable_venues(
    supported: &[VenueId],
    entries: &[(VenueRoute, Arc<ScopeEntry>)],
) -> Vec<VenueId> {
    supported
        .iter()
        .copied()
        .filter(|venue| {
            entries
                .iter()
                .any(|(route, entry)| route.venue == *venue && reads_the_whole_market(entry))
        })
        .collect()
}

fn routes_of(entries: &[(VenueRoute, Arc<ScopeEntry>)], venue: VenueId) -> Vec<VenueRoute> {
    entries
        .iter()
        .filter(|(route, entry)| route.venue == venue && reads_the_whole_market(entry))
        .map(|(route, _)| route.clone())
        .collect()
}

fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::AllTickers)
}

trait MarketDirectory {
    fn readable_venues(&self) -> Vec<VenueId>;

    fn routes_of(&self, venue: VenueId) -> Vec<VenueRoute>;

    fn is_loaded(&self, route: &VenueRoute) -> bool;
}

struct ConnectorDirectory(MarketParticipantsConnector);

impl MarketDirectory for ConnectorDirectory {
    fn readable_venues(&self) -> Vec<VenueId> {
        readable_venues(&self.0.supported_venues(), &self.0.venues().entries())
    }

    fn routes_of(&self, venue: VenueId) -> Vec<VenueRoute> {
        routes_of(&self.0.venues().entries(), venue)
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Start,
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Refresh,
    Venue(VenueId),
}

struct Countdown {
    window: WindowId,
    scope: Cell<Option<NodeId>>,
    deadline: Cell<Option<i64>>,
    label: Signal<Option<SmolStr>>,
    now: Rc<dyn Fn() -> i64>,
    timer: std::cell::RefCell<Option<TimerId>>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            scope: Cell::new(None),
            deadline: Cell::new(None),
            label: cx.signal(None),
            now,
            timer: std::cell::RefCell::new(None),
        }
    }

    fn set_scope(self: &Rc<Self>, app: &mut App, scope: NodeId) {
        self.scope.set(Some(scope));
        if self.deadline.get().is_some() && self.timer.borrow().is_none() {
            self.arm(app);
        }
    }

    fn clear_scope(&self) {
        self.scope.set(None);
    }

    fn deadline(&self) -> Option<i64> {
        self.deadline.get()
    }

    fn update(self: &Rc<Self>, app: &mut App, deadline: Option<i64>) {
        self.deadline.set(deadline);
        if let Some(id) = self.timer.borrow_mut().take() {
            app.cancel_timer(&id);
        }
        self.write_label(app);
        if deadline.is_some() {
            self.arm(app);
        }
    }

    fn write_label(&self, app: &mut App) -> bool {
        app.write(
            &self.label,
            countdown_label(self.deadline.get(), (self.now)()),
        )
    }

    fn arm(self: &Rc<Self>, app: &mut App) {
        if self.deadline.get().is_none() {
            return;
        }
        let Some(scope) = self.scope.get() else {
            return;
        };
        let this = Rc::clone(self);
        match app.after_scoped(self.window, scope, COUNTDOWN_TICK, move |app| {
            this.tick(app)
        }) {
            Ok(id) => *self.timer.borrow_mut() = Some(id),
            Err(error) => {
                log::error!("the volume heat map countdown could not arm its next tick: {error}");
            }
        }
    }

    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

struct VenueControl {
    parent: NodeId,
    root: NodeId,
    select: Select,
    language: LanguageCode,
    venues: Vec<VenueId>,
}

struct RefreshControl {
    root: NodeId,
    slot: NodeId,
    button: Button,
    language: LanguageCode,
}

pub struct VolumeHeatMapWidget {
    pub config: VolumeHeatMapConfig,
    config_change: WidgetConfigChange,
    binding: Option<WidgetBinding<VolumeHeatMapProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    pub recovery: WidgetAction,
    directory: Option<Rc<dyn MarketDirectory>>,
    chart: VolumeHeatMapChart,
    body_root: Option<NodeId>,
    empty: Option<NodeId>,
    empty_language: Option<LanguageCode>,
    venues: Vec<VenueId>,
    reading: Option<Reading>,
    accepted: Option<u64>,
    ready: Option<u64>,
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
    refresh_pending: bool,
    awaiting_market: bool,
    seen_generation: u64,
    generation: Signal<u64>,
    requests: Signal<Option<HeaderRequest>>,
    venue_control: Option<VenueControl>,
    refresh_control: Option<RefreshControl>,
}

impl WidgetView for VolumeHeatMapWidget {
    type Config = VolumeHeatMapConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.volume_heatmap");
    const DISPLAY_NAME: &'static str = "widget.names.volume_heatmap";
    const CATEGORY: WidgetCategory = WidgetCategory::Statistics;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.volume_heatmap");
    const ICON: Option<&'static str> = Some("widgets/volume_heatmap.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(
                config,
                None,
                None,
                status,
                config_change,
                recovery,
                now,
                cx,
            ));
        };
        let service = VolumeHeatMapService::new(connector.market());
        let binding = WidgetBinding::spawn(cx, service);
        let directory: Rc<dyn MarketDirectory> = Rc::new(ConnectorDirectory(connector));
        Ok(Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            now,
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        let venues = directory.readable_venues();
        if venues.is_empty() {
            self.venues = venues;
            self.wait_for_market(cx);
            return;
        }
        self.awaiting_market = false;
        self.venues = venues;
        if self
            .config
            .venue
            .is_some_and(|venue| !self.venues.contains(&venue))
        {
            self.config.venue = None;
            log::warn!(
                "the volume heat map's venue is not one this map can read; the pane picks a readable one"
            );
            self.config_change.publish(cx.app_mut());
        }
        let venue = match self.config.venue {
            Some(venue) => venue,
            None => {
                let venue = self.venues[0];
                self.config.venue = Some(venue);
                self.config_change.publish(cx.app_mut());
                venue
            }
        };
        let routes = directory.routes_of(venue);
        if routes.is_empty() {
            self.reading = None;
            self.wait_for_market(cx);
            return;
        }

        let retry = self.status.is_failed(cx.app());
        self.reading = Some(Reading {
            venue,
            routes: routes.into(),
        });
        self.start(retry, cx);
    }

    fn has_content(&self) -> bool {
        self.reading.is_some() && self.accepted.is_some()
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.body_root.is_some() {
            return Ok(());
        }
        let root = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_BODY")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.chart.build(cx, root)?;
        self.body_root = Some(root);
        self.countdown.set_scope(cx.app_mut(), root);
        self.sync_empty_overlay(cx)?;
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.reload_market(cx);
            }
        }

        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        let request = *cx.read(&self.requests);
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        if self.has_content() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        self.chart.sync(cx)?;
        self.sync_empty_overlay(cx)?;

        let deadline = self.countdown_deadline(cx.app());
        if deadline != self.countdown.deadline() {
            Countdown::update(&self.countdown, cx.app_mut(), deadline);
        }
        self.sync_header(cx)?;

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        match group {
            HeaderGroup::Left => {
                self.release_venue_control(cx, true);
                let control = self.build_venue_control(cx, parent)?;
                self.venue_control = Some(control);
            }
            HeaderGroup::Right => {
                self.release_refresh_control(cx, true);
                let control = self.build_refresh_control(cx, parent)?;
                self.refresh_control = Some(control);
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        let moved = match input {
            InputEvent::PointerMove { position, .. }
            | InputEvent::PointerEnter { position, .. } => self.chart.point_at(*position),
            InputEvent::PointerLeave { .. } => self.chart.clear_hover(),
            _ => return false,
        };
        if moved {
            let _ = cx.request_frame();
        }
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.release_venue_control(cx, false);
        self.release_refresh_control(cx, false);
        self.chart.release(cx);
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.countdown.clear_scope();
        self.empty = None;
        self.empty_language = None;
        self.body_root = None;
        self.reading = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
        self.pending.clear();
    }
}

impl VolumeHeatMapWidget {
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        config: VolumeHeatMapConfig,
        binding: Option<WidgetBinding<VolumeHeatMapProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        now: Rc<dyn Fn() -> i64>,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let requests = cx.signal(None);
        let countdown = Rc::new(Countdown::new(cx, Rc::clone(&now)));

        Self {
            config,
            config_change,
            binding,
            pending: Pending::new(),
            status,
            recovery,
            directory,
            chart: VolumeHeatMapChart::new(),
            body_root: None,
            empty: None,
            empty_language: None,
            venues: Vec::new(),
            reading: None,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            awaiting_market: false,
            seen_generation,
            generation,
            requests,
            venue_control: None,
            refresh_control: None,
        }
    }

    #[cfg(test)]
    fn with_source<S: VolumeHeatMapSource>(
        config: VolumeHeatMapConfig,
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        now: Rc<dyn Fn() -> i64>,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = VolumeHeatMapService::new(source);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            now,
            cx,
        )
    }

    fn reload_market(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        let venues = directory.readable_venues();
        if venues.is_empty() {
            self.venues = venues;
            if self.config.venue.take().is_some() {
                self.config_change.publish(cx.app_mut());
            }
            self.reading = None;
            self.wait_for_market(cx);
            return;
        }
        self.venues = venues;
        let venue = self
            .config
            .venue
            .filter(|venue| self.venues.contains(venue))
            .unwrap_or(self.venues[0]);
        let routes = directory.routes_of(venue);
        let next = (!routes.is_empty()).then(|| Reading {
            venue,
            routes: routes.into(),
        });
        if next == self.reading {
            if !self.market_ready(directory.as_ref()) {
                self.wait_for_market(cx);
            }
            return;
        }
        self.load(cx);
    }

    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        self.reading.as_ref().is_some_and(|reading| {
            !reading.routes.is_empty()
                && reading
                    .routes
                    .iter()
                    .all(|route| directory.is_loaded(route))
        })
    }

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.reading = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
        self.refresh_pending = false;
        self.pending.clear();
        self.chart.set_entries(Arc::from([]));
        self.report_markets_not_loaded(cx);
        let _ = cx.request_frame();
    }

    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let venues = market_venues();
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded {
                supported: venues.clone(),
                venues,
            }),
        );
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.ready = None;
        self.next_refresh_at = None;
        self.refresh_pending = false;
        self.pending.clear();
        if !retry {
            self.accepted = None;
            self.chart.set_entries(Arc::from([]));
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let Some(reading) = self.reading.clone() else {
            return;
        };
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(
            cx,
            VolumeHeatMapRequest::Load {
                venue: reading.venue,
                routes: reading.routes,
            },
        );
        self.pending.track(request_id, PendingCommand::Start);
        let _ = cx.request_frame();
    }

    fn apply(&mut self, inbound: Inbound<VolumeHeatMapProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, message, cx),
            Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
        }
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<VolumeHeatMapProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_rpc_result(result, cx),
            Some(PendingCommand::Refresh) => self.apply_rpc_result(result, cx),
            None => {
                log::debug!(
                    "the volume heat map pane ignored reply {request_id}: it answers no command in flight"
                );
            }
        }
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<VolumeHeatMapProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(VolumeHeatMapFailure::Rpc(error)), recovery),
            );
        }
    }

    fn apply_reliable(
        &mut self,
        key: VolumeHeatMapStreamKey,
        message: Arc<VolumeHeatMapStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.is_about_the_map_on_screen(key, cx.app()) {
            return;
        }
        let VolumeHeatMapStreamMessage::Status(status) = message.as_ref() else {
            return;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            VolumeHeatMapStatus::Connecting { .. } => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            VolumeHeatMapStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            VolumeHeatMapStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            VolumeHeatMapStatus::Failed { error, .. } => {
                self.refresh_pending = false;
                let failure = VolumeHeatMapFailure::Fetch(error.clone());
                if is_waiting_for_a_catalog(&failure) {
                    self.reading = None;
                    self.wait_for_market(cx);
                    return;
                }
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(failure), recovery),
                );
            }
        }
    }

    fn apply_latest(
        &mut self,
        key: VolumeHeatMapStreamKey,
        message: Arc<VolumeHeatMapStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.is_about_the_map_on_screen(key, cx.app()) {
            return;
        }
        let VolumeHeatMapStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return;
        };
        if self.accepted.is_some_and(|held| snapshot.revision < held) {
            return;
        }
        let changed = self.chart.set_entries(Arc::clone(&snapshot.entries));
        self.accepted = Some(snapshot.revision);
        if self.ready.is_some_and(|ready| snapshot.revision >= ready) {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        if changed {
            let _ = cx.request_frame();
        }
    }

    fn is_about_the_map_on_screen(&self, key: VolumeHeatMapStreamKey, app: &App) -> bool {
        if self.status.is_failed(app) || self.awaiting_market {
            return false;
        }
        self.reading
            .as_ref()
            .is_some_and(|reading| reading.venue == key.venue())
    }

    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.awaiting_market || self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    fn select_venue(&mut self, venue: VenueId, cx: &mut WidgetCx<'_>) {
        if self.config.venue == Some(venue) {
            return;
        }
        self.config.venue = Some(venue);
        self.config_change.publish(cx.app_mut());
        self.reading = None;
        self.accepted = None;
        self.chart.set_entries(Arc::from([]));
        self.load(cx);
    }

    fn refresh(&mut self, cx: &mut WidgetCx<'_>) {
        if self.refresh_is_disabled(cx.app()) {
            return;
        }
        let Some(reading) = self.reading.as_ref() else {
            return;
        };
        let venue = reading.venue;
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        self.refresh_pending = true;
        let request_id = binding.call(cx, VolumeHeatMapRequest::Refresh { venue });
        self.pending.track(request_id, PendingCommand::Refresh);
        let _ = cx.request_frame();
    }

    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Refresh => self.refresh(cx),
            HeaderRequest::Venue(venue) => self.select_venue(venue, cx),
        }
    }

    fn build_venue_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<VenueControl, EngineError> {
        let language = *cx.read(&cx.language());
        let row = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_VENUE")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let select = self.build_venue_select(cx, row)?;
        Ok(VenueControl {
            parent,
            root: row,
            select,
            language,
            venues: self.venues.clone(),
        })
    }

    fn build_venue_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Select, EngineError> {
        let requests = self.requests;
        let translate = cx.translate().clone();
        let options = self.venue_select_options(&translate);
        let window = cx.window();
        let key = cx.unique_key(VENUE_KEY_SUFFIX);
        let theme = cx.theme().clone();
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                let Ok(venue) = VenueId::from_name(id.as_str()) else {
                    return;
                };
                app.write(&requests, Some(HeaderRequest::Venue(venue)));
                let _ = app.request_repaint(window);
            },
        );
        let items = self.venue_items(&translate);
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!("the volume heat map picker could not state its venues: {error}");
            });
        if let Some(venue) = self.config.venue {
            select.set_chosen(cx.app_mut(), [venue.as_str()], false);
        }
        let _ = select.set_disabled(cx.app_mut(), self.venue_picker_is_disabled());
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    fn venue_select_options(&self, translate: &Translate) -> SelectOptions {
        let label = t_smol!(translate, "common.market_participants");
        let placeholder = if self.config.venue.is_some() {
            SmolStr::default()
        } else {
            t_smol!(translate, "widget.volume_heatmap.choose_venue")
        };
        SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(label)
            .placeholder(placeholder)
    }

    fn venue_items(&self, translate: &Translate) -> Vec<SelectItem> {
        self.venues
            .iter()
            .map(|venue| {
                SelectItem::new(
                    SmolStr::new(venue.as_str()),
                    venue.display_string(translate),
                )
            })
            .collect()
    }

    fn venue_picker_is_disabled(&self) -> bool {
        self.awaiting_market || self.venues.len() < 2
    }

    fn build_refresh_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<RefreshControl, EngineError> {
        let language = *cx.read(&cx.language());
        let row = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_REFRESH_ROW")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let slot = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(cx.tree_mut(), row)?;
        let button = self.build_refresh_button(cx, slot)?;
        let countdown = self.countdown.label;
        let theme = cx.theme().clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_VOLUME_HEATMAP_COUNTDOWN")
        .bind_text(move |read| {
            let label: Option<SmolStr> = read.read(&countdown).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), slot)?;
        Ok(RefreshControl {
            root: row,
            slot,
            button,
            language,
        })
    }

    fn build_refresh_button(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Button, EngineError> {
        let requests = self.requests;
        let label = t_smol!(cx.translate(), "widget.volume_heatmap.refresh_now");
        let disabled = self.refresh_is_disabled(cx.app());
        let key = cx.unique_key(REFRESH_KEY_SUFFIX);
        let theme = cx.theme().clone();
        let window = cx.window();
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::ArrowsClockwise)
            .disabled(disabled)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                ctx.update(&requests, |request| *request = Some(HeaderRequest::Refresh));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.sync_venue_control(cx)?;
        self.sync_refresh_control(cx)?;
        Ok(())
    }

    fn sync_venue_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some((root, parent, language, venues)) = self.venue_control.as_ref().map(|control| {
            (
                control.root,
                control.parent,
                control.language,
                control.venues.clone(),
            )
        }) else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.release_venue_control(cx, false);
            return Ok(());
        }
        let language_now = *cx.read(&cx.language());
        if language != language_now {
            self.release_venue_control(cx, true);
            let control = self.build_venue_control(cx, parent)?;
            self.venue_control = Some(control);
        } else if venues != self.venues {
            let translate = cx.translate().clone();
            let items = self.venue_items(&translate);
            if let Some(control) = self.venue_control.as_mut() {
                control
                    .select
                    .set_items(cx.app_mut(), items)
                    .unwrap_or_else(|error| {
                        log::warn!(
                            "the volume heat map picker could not replace its venues: {error}"
                        );
                    });
                control.venues = self.venues.clone();
            }
        }
        let Some(control) = self.venue_control.as_ref() else {
            return Ok(());
        };
        match self.config.venue {
            Some(venue) => {
                let stated = control.select.chosen(cx.app());
                if stated.len() != 1 || stated.first().map(SmolStr::as_str) != Some(venue.as_str())
                {
                    control
                        .select
                        .set_chosen(cx.app_mut(), [venue.as_str()], false);
                }
            }
            None if !control.select.chosen(cx.app()).is_empty() => {
                control.select.set_chosen(cx.app_mut(), [], false);
            }
            None => {}
        }
        let _ = control
            .select
            .set_disabled(cx.app_mut(), self.venue_picker_is_disabled());
        control.select.sync(cx.app_mut())?;
        Ok(())
    }

    fn sync_refresh_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some((root, slot, language)) = self
            .refresh_control
            .as_ref()
            .map(|control| (control.root, control.slot, control.language))
        else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.release_refresh_control(cx, false);
            return Ok(());
        }
        let language_now = *cx.read(&cx.language());
        if language != language_now {
            if let Some(control) = self.refresh_control.as_mut() {
                if let Some(node) = control.button.node() {
                    remove_own_node(cx, node);
                }
                let _ = control.button.release(cx.app_mut());
            }
            let button = self.build_refresh_button(cx, slot)?;
            if let Some(control) = self.refresh_control.as_mut() {
                control.button = button;
                control.language = language_now;
            }
        }
        let disabled = self.refresh_is_disabled(cx.app());
        let Some(control) = self.refresh_control.as_ref() else {
            return Ok(());
        };
        control.button.sync(cx.app_mut())?;
        let _ = control.button.set_disabled(cx.app_mut(), disabled);
        Ok(())
    }

    fn sync_empty_overlay(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self.has_content() && self.chart.is_empty();
        if !empty {
            if let Some(node) = self.empty.take() {
                remove_own_node(cx, node);
                self.empty_language = None;
            }
            return Ok(());
        }
        let language = *cx.read(&cx.language());
        if self.empty.is_some() && self.empty_language == Some(language) {
            return Ok(());
        }
        if let Some(node) = self.empty.take() {
            remove_own_node(cx, node);
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.body_root = None;
            return Ok(());
        }
        let overlay = NodeBuilder::boxed()
            .id("TEST_VOLUME_HEATMAP_EMPTY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        let text = t_smol!(cx.translate(), "common.no_data");
        EmptyState::new(cx.theme().clone(), "volume_heatmap.empty")
            .text(text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);
        self.empty_language = Some(language);
        Ok(())
    }

    fn release_venue_control(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(control) = self.venue_control.take() else {
            return;
        };
        if remove_node {
            remove_own_node(cx, control.root);
        }
        let _ = control.select.release(cx.app_mut());
    }

    fn release_refresh_control(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(control) = self.refresh_control.take() else {
            return;
        };
        if remove_node {
            remove_own_node(cx, control.root);
        }
        let _ = control.button.release(cx.app_mut());
    }
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) {
    if cx.tree().get(node).is_err() {
        return;
    }
    let window = cx.window();
    let _ = cx.app_mut().remove_node(window, node);
}

fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

#[cfg(test)]
mod tests;
