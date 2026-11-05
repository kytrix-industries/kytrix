//! The prediction probability chart pane.
//!
//! The pane follows one event and draws one line per option. It loads a bounded
//! history and then folds a latest reading into it every ten seconds; the event
//! selector is the header control, and an event that is no longer published puts
//! the selection back to "nothing chosen" rather than leaving a chart that cannot
//! be recovered.

use std::sync::Arc;

use app_config_provider::{events::LanguageChanged, manager::ConfigManager};
use base_ui::{
    empty_state::EmptyState,
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    App, Context, ElementId, Entity, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
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
        PredictionEventRef, PredictionMarketGroup, PredictionMarketRef, PredictionWindow,
    },
    routing::CapabilityId,
};
use smol_str::{SmolStr, format_smolstr};
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    chart::chart_element,
    model::{PredictionChartSnapshot, PredictionMarketChoice},
    service::{
        PredictionChartFetchError, PredictionChartProtocol, PredictionChartRequest,
        PredictionChartResponse, PredictionChartService, PredictionChartServiceError,
        PredictionChartSource, PredictionChartStatus, PredictionChartStreamKey,
        PredictionChartStreamMessage,
    },
};

/// A failure the pane reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionChartError {
    Fetch(PredictionChartFetchError),
    Rpc(RpcCallError<PredictionChartServiceError>),
}

impl LocalizedError for PredictionChartError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for PredictionChartFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Market(error) => error.localized_message(cx),
            Self::NoMarkets => t!(cx, "error.prediction_chart.no_markets"),
            Self::Timeout(timeout) => t!(
                cx,
                "error.prediction_chart.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl LocalizedError for PredictionChartServiceError {
    fn localized_message(&self, _cx: &App) -> SharedString {
        match *self {}
    }
}

/// The prediction probability chart.
pub struct PredictionChartWidget {
    selection: Option<PredictionEventRef>,
    selection_title: Option<SmolStr>,
    market: Option<PredictionMarketRef>,
    binding: WidgetBinding<PredictionChartProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    snapshot: Option<Arc<PredictionChartSnapshot>>,
    locale: Option<SmolStr>,
    awaiting_market: bool,
    _venues: Subscription,
    _language: Option<Subscription>,
}

impl WidgetView for PredictionChartWidget {
    type Config = ();

    const ID: WidgetId = WidgetId::new_static("kytrix.prediction_chart");
    const DISPLAY_NAME: &'static str = "widget.names.prediction_chart";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &str = "Prediction event probabilities over time";
    const ICON: Option<&'static str> = Some("widgets/prediction_chart.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<MarketParticipantsConnector>().market();
        Self::new_with_source(source, status, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if !self.market_ready(cx) {
            self.awaiting_market = true;
            self.snapshot = None;
            self.report_notice(cx);
            return;
        }
        self.awaiting_market = false;
        let Some(selection) = self.selection.clone() else {
            self.status.report(
                WidgetState::waiting(Notice::message("widget.prediction_chart.select_event")),
                cx,
            );
            return;
        };
        self.locale = cx
            .try_global::<ConfigManager>()
            .and_then(|manager| polymarket_locale(manager.read().language));
        let retry = self.status.is_failed(cx);
        if let Some(snapshot) = self.snapshot.as_mut() {
            // A replaced stream numbers its revisions from one. The reading kept
            // across a retry or a language change belongs to the stream being
            // replaced, so it must not reject the new stream's first snapshot.
            Arc::make_mut(snapshot).revision = 0;
        }
        self.status.report(WidgetState::Connecting, cx);
        let request = if retry {
            PredictionChartRequest::Retry {
                event: selection,
                market: self.market.clone(),
                window: PredictionWindow::default(),
                locale: self.locale.clone(),
            }
        } else {
            PredictionChartRequest::Load {
                event: selection,
                market: self.market.clone(),
                window: PredictionWindow::default(),
                locale: self.locale.clone(),
            }
        };
        self.binding.call(request, cx, Self::apply_load_result);
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some() && !self.awaiting_market
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
            "PredictionChartEvent",
            self.selection.clone(),
            label,
            "widget.prediction_chart.select_event",
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
        WidgetHeader::empty().left(left)
    }
}

impl PredictionChartWidget {
    /// The seam a test answers through.
    pub fn new_with_source<S>(
        source: S,
        status: Entity<WidgetStatus>,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: PredictionChartSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            PredictionChartService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let venues = cx.global::<MarketParticipantsConnector>().venues().events();
        let subscription = cx.subscribe(&venues, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        });
        let language_subscription = match cx.try_global::<ConfigManager>() {
            Some(manager) => {
                let events = manager.events().clone();
                let subscription = cx.subscribe(&events, |this, _, _: &LanguageChanged, cx| {
                    this.load(cx);
                    cx.notify();
                });
                Some(subscription)
            }
            None => None,
        };
        Self {
            selection: None,
            selection_title: None,
            market: None,
            binding,
            status,
            recovery,
            snapshot: None,
            locale: None,
            awaiting_market: false,
            _venues: subscription,
            _language: language_subscription,
        }
    }

    fn market_group_select(
        &self,
        snapshot: &PredictionChartSnapshot,
        groups: &[&PredictionMarketGroup],
        selected: &PredictionMarketGroup,
        cx: &Context<Self>,
    ) -> Select {
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "PredictionChartMarketGroup",
        )))
        .placeholder(SharedString::new(selected.label.clone()))
        .ghost()
        .small()
        .max_w(px(200.0))
        .menu_max_h(px(600.0))
        .selected_item(Some(SharedString::new(selected.key.as_str())))
        .aria_label(t!(cx, "widget.prediction_chart.select_market_group"));
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
        snapshot: &PredictionChartSnapshot,
        selected: &PredictionMarketChoice,
        cx: &Context<Self>,
    ) -> Select {
        let selected_id = SharedString::new(selected.market.native_market.as_str());
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "PredictionChartMarket",
        )))
        .placeholder(SharedString::new(selected.label.clone()))
        .ghost()
        .small()
        .max_w(px(320.0))
        .menu_max_h(px(600.0))
        .selected_item(Some(selected_id))
        .aria_label(t!(cx, "widget.prediction_chart.select_market"));
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

    fn select(&mut self, event: PredictionEventRef, title: SmolStr, cx: &mut Context<Self>) {
        self.selection = Some(event);
        self.selection_title = Some(title);
        self.market = None;
        self.snapshot = None;
        self.load(cx);
        cx.notify();
    }

    fn select_market(&mut self, market: PredictionMarketRef, cx: &mut Context<Self>) {
        if self.market.as_ref() == Some(&market) && !self.status.is_failed(cx) {
            return;
        }
        self.market = Some(market);
        self.snapshot = None;
        self.load(cx);
        cx.notify();
    }

    fn current_key(&self) -> Option<PredictionChartStreamKey> {
        self.selection
            .as_ref()
            .map(|event| PredictionChartStreamKey {
                event: event.clone(),
                market: self.market.clone(),
                window: PredictionWindow::default(),
                locale: self.locale.clone(),
            })
    }

    fn market_ready(&self, cx: &App) -> bool {
        let venues = cx.global::<MarketParticipantsConnector>().venues();
        if !Self::listed_venues()
            .iter()
            .any(|venue| venues.loaded(venue).is_some())
        {
            return false;
        }
        let Some(event) = &self.selection else {
            return true;
        };
        venues.scope(&event.route).is_some()
    }

    fn report_notice(&self, cx: &mut Context<Self>) {
        if self.awaiting_market {
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
        } else if self.selection.is_none() {
            self.status.report(
                WidgetState::waiting(Notice::message("widget.prediction_chart.select_event")),
                cx,
            );
        }
    }

    fn listed_venues() -> Vec<VenueId> {
        market_participants_connector::venue::definition::venues_supporting(
            CapabilityId::PredictionEvents,
        )
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<PredictionChartProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(PredictionChartResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(PredictionChartError::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    fn apply_reliable(
        &mut self,
        key: PredictionChartStreamKey,
        message: Arc<PredictionChartStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let PredictionChartStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        match status {
            PredictionChartStatus::Connecting => self.status.report(WidgetState::Connecting, cx),
            PredictionChartStatus::Ready { .. } => {
                if self.snapshot.is_some() {
                    self.status.report(WidgetState::Ready, cx);
                }
            }
            PredictionChartStatus::Failed { error } => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(
                        Arc::new(PredictionChartError::Fetch(error.clone())),
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
        key: PredictionChartStreamKey,
        message: Arc<PredictionChartStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let PredictionChartStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        if self
            .snapshot
            .as_ref()
            .is_some_and(|held| snapshot.revision < held.revision)
        {
            return false;
        }
        self.snapshot = Some(snapshot.clone());
        self.selection_title = Some(snapshot.title.clone());
        self.status.report(WidgetState::Ready, cx);
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
            self.load(cx);
            return;
        }
        if self.awaiting_market && self.market_ready(cx) {
            self.load(cx);
        }
    }
}

impl Render for PredictionChartWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(snapshot) = self.snapshot.clone() else {
            return div().size_full().min_h_0().into_any_element();
        };
        if snapshot.series.iter().all(|line| line.points.is_empty()) {
            return div()
                .size_full()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "PredictionChartEmpty",
                    t!(cx, "common.no_data"),
                ))
                .into_any_element();
        }
        let theme = cx.theme();
        let mut legend = div().flex().flex_wrap().gap_2().px_2().py_1();
        for line in snapshot.series.iter() {
            let color = theme.roles.chart.series[line.color % 6].hsla;
            let latest = line
                .latest
                .map(|value| format_smolstr!("{:.0}%", value.ratio().inner() as f64 / 1e16))
                .unwrap_or_default();
            legend = legend.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().size(px(7.0)).rounded_sm().bg(color))
                    .child(div().text_xs().child(SharedString::new(line.label.clone())))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.roles.fg.muted.hsla)
                            .child(SharedString::new(latest)),
                    ),
            );
        }
        if snapshot.omitted > 0 {
            legend = legend.child(
                div()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "widget.prediction_chart.more_lines",
                        count = snapshot.omitted
                    )),
            );
        }
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(legend)
            .child(div().flex_1().min_h_0().child(chart_element(snapshot)))
            .into_any_element()
    }
}
