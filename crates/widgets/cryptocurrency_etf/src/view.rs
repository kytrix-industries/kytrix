//! The ETF flow pane: which funds it follows, what it draws, and what it offers.
//!
//! # What this pane is
//!
//! ETF flow is published once a trading day, so an answer is a *history* rather
//! than a stream: the service reads the whole thing every eight hours, and the pane
//! counts down to the next read so a number that has not moved can be told apart
//! from a pane that has stopped. A manual refresh resets that schedule rather than
//! running alongside it.
//!
//! The asset, the window and the fund are **views of one answer**, not scopes of
//! their own: one response carries every asset and the whole history, so each of
//! the three re-resolves what is on screen without asking the service for anything.
//! Only the account is a scope — it is what the answer was served to.
//!
//! # What it does with a failure
//!
//! A failed read is a failure the user sees: the pane reports it through the
//! framework, which puts a banner over the chart that is still on screen. The
//! service stops its schedule at that point, so the banner's recovery is what starts
//! it again — a service that is briefly unreachable does not turn into a request
//! every eight hours until someone notices.
//!
//! # Who may see it
//!
//! The service serves this data to an account, so a pane with no account behind it
//! fetches nothing and says so. That is a *waiting* state rather than a failure:
//! nothing has gone wrong, the pane is just not for an anonymous reader. Nothing is
//! retried on a timer either — the account event subscription re-scopes the pane the
//! moment the user signs in.

use std::{sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    Window, base::Disableable, div, px,
};
use i18n::t;
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use widget::{
    account::account_of,
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    util::{format_countdown, unix_millis},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    chart::{CryptocurrencyEtfChart, flow},
    model::{
        CryptocurrencyEtfAsset, CryptocurrencyEtfFundSelection, CryptocurrencyEtfParseError,
        CryptocurrencyEtfWindow,
    },
    service::{
        CryptocurrencyEtfFetchError, CryptocurrencyEtfProtocol, CryptocurrencyEtfRequest,
        CryptocurrencyEtfResponse, CryptocurrencyEtfService, CryptocurrencyEtfServiceError,
        CryptocurrencyEtfSource, CryptocurrencyEtfStatus, CryptocurrencyEtfStreamKey,
        CryptocurrencyEtfStreamMessage,
    },
};

/// How often the pane re-reads the clock behind its countdown.
///
/// It is not a fetch: it only re-checks how long is left. A quarter of a second is
/// faster than the label's own resolution and cheaper than a frame.
pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// The height of the pane's own row of readings.
const METRICS_HEIGHT: f32 = 58.0;

/// The asset, the window and the fund a pane is showing.
///
/// None of the three is persisted: the pane opens on the deepest ETF complex, its most
/// recent month, and every fund in it, and what the user picks lives only as long as the
/// pane does. Nothing here is part of a stream's scope either, so the widget persists no
/// options at all and carries this type for its schema version alone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CryptocurrencyEtfConfig;

impl WidgetConfig for CryptocurrencyEtfConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CryptocurrencyEtfFailure {
    Fetch(CryptocurrencyEtfFetchError),
    Rpc(RpcCallError<CryptocurrencyEtfServiceError>),
}

impl LocalizedError for CryptocurrencyEtfFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for CryptocurrencyEtfFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Parse(error) => error.localized_message(cx),
            Self::Timeout(timeout) => {
                t!(
                    cx,
                    "error.cryptocurrency_etf.timeout",
                    seconds = timeout.as_secs()
                )
            }
        }
    }
}

impl LocalizedError for CryptocurrencyEtfServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t!(cx, "error.cryptocurrency_etf.stream_unavailable")
            }
        }
    }
}

impl LocalizedError for CryptocurrencyEtfParseError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::EmptyAssetMap { .. } => t!(cx, "error.cryptocurrency_etf.no_data"),
            Self::InvalidDate { value, .. } => {
                t!(cx, "error.cryptocurrency_etf.invalid_date", value = value)
            }
            Self::DuplicateDate { date, .. } => {
                t!(cx, "error.cryptocurrency_etf.duplicate_date", value = date)
            }
            Self::TooManyRows { maximum, .. } => t!(
                cx,
                "error.cryptocurrency_etf.too_many_rows",
                maximum = maximum,
                value = maximum.saturating_add(1)
            ),
            Self::InvalidNumber { value, .. } => {
                t!(cx, "error.cryptocurrency_etf.invalid_number", value = value)
            }
            Self::NonFiniteTotal { .. } | Self::NonFiniteAssets { .. } => {
                t!(cx, "error.cryptocurrency_etf.non_finite")
            }
        }
    }
}

/// One ETF flow pane: the account it follows, the chart it draws, and its controls.
pub struct CryptocurrencyEtfWidget {
    /// The asset, window and fund this pane is showing, which it starts at the defaults.
    asset: CryptocurrencyEtfAsset,
    window: CryptocurrencyEtfWindow,
    fund: CryptocurrencyEtfFundSelection,
    binding: WidgetBinding<CryptocurrencyEtfProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    /// The chart, which exists once an answer has been read and is dropped with the
    /// account it belonged to.
    chart: Option<CryptocurrencyEtfChart>,
    snapshot: Option<Arc<crate::model::CryptocurrencyEtfSnapshot>>,
    /// The revision of the answer the chart is showing, which is what makes the pane
    /// content rather than a place waiting for something.
    accepted: Option<u64>,
    /// The revision the stream has reported ready. Readiness is published at the
    /// protocol's own boundary, so a pane that accepted an older answer than the one
    /// a status described stays busy.
    ready: Option<u64>,
    /// When the next automatic read is due, as the service's own deadline.
    next_refresh_at: Option<i64>,
    countdown: Option<SharedString>,
    /// Whether a refresh command this pane sent has been answered *and its round
    /// reported*.
    ///
    /// It is set by the click and cleared when the service says what it did with the
    /// command, which is what closes the window between the two: the service holds
    /// its gate for the whole round, and the round is announced after the command is
    /// acknowledged — so a control that trusted the acknowledgement alone would be
    /// live again for the moment in between.
    refresh_pending: bool,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    _countdown_task: Task<()>,
    /// Keeps the pane following the account the settings hold.
    _account: Subscription,
}

impl WidgetView for CryptocurrencyEtfWidget {
    type Config = CryptocurrencyEtfConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.cryptocurrency_etf");
    const DISPLAY_NAME: &'static str = "widget.names.cryptocurrency_etf";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str =
        "Daily net flows of the cryptocurrency spot ETF complexes, and the assets they hold";
    const ICON: Option<&'static str> = Some("widgets/cryptocurrency_etf.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let retry = self.status.is_failed(cx);
        self.account_id = account_of(&cx.global::<ConfigManager>().read());
        self.start(retry, cx);
    }

    /// A pane is content once it has accepted an answer for the account it follows.
    ///
    /// Having a chart is not the same thing: a chart is drawn for whatever the
    /// config names as soon as an answer arrives, and an empty chart is not content
    /// — it is a pane that has asked for something and is waiting for it.
    fn has_content(&self) -> bool {
        self.chart.is_some() && self.accepted.is_some()
    }

    /// The refresh, the asset, the fund and the window — the whole header, in the
    /// right group, because the left one is the pane drag area.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().right(self.drawing_controls(cx))
    }
}

impl CryptocurrencyEtfWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        _config: CryptocurrencyEtfConfig,
        source: S,
        status: Entity<WidgetStatus>,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: CryptocurrencyEtfSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            CryptocurrencyEtfService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let countdown_task = cx.spawn(async move |this, cx| {
            loop {
                if this
                    .update(cx, |this, cx| {
                        this.tick_countdown(cx);
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(COUNTDOWN_TICK).await;
            }
        });
        // The account is the pane's scope, and the user can change it while the pane
        // is open: signing out has to take the data with it, and signing in has to
        // fetch. Following the account event is what makes that happen without the
        // pane having to be reopened.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });

        Self {
            asset: CryptocurrencyEtfAsset::default(),
            window: CryptocurrencyEtfWindow::default(),
            fund: CryptocurrencyEtfFundSelection::default(),
            binding,
            status,
            recovery,
            chart: None,
            snapshot: None,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown: None,
            refresh_pending: false,
            // The pane records the account it was built for, but it does not start
            // anything: the framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            _countdown_task: countdown_task,
            _account: account,
        }
    }

    /// The account this pane would accept an answer from.
    fn current_key(&self) -> Option<CryptocurrencyEtfStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CryptocurrencyEtfStreamKey { account_id })
    }

    /// Whether this pane has anything to read.
    ///
    /// Both header controls read this rather than deciding for themselves: a window
    /// and a fund only say which part of an answer to show, so with no answer coming
    /// they change nothing, and a pane that cannot refresh must not present itself as
    /// able to re-scope either.
    fn has_scope(&self) -> bool {
        self.current_key().is_some()
    }

    /// The deadline this pane counts down to, if it has one to count down to.
    ///
    /// A countdown belongs to a pane that is *ready*: it counts down the time left
    /// until the next automatic read of the answer that is on screen. While a read is
    /// in flight there is nothing to count down to — the deadline the service
    /// announced belongs to the read after the one being waited for — so a number
    /// that started moving then would be counting down to an answer the pane has not
    /// received. A pane with no account has no deadline either: it is not reading.
    fn countdown_deadline(&self, cx: &impl AppContext) -> Option<i64> {
        let status = self.status.state(cx);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the countdown is: a pane that is connecting, reading
    /// or failed has nothing to bring forward — the first two are already waiting on
    /// data, and a failure is a state the framework offers its own recovery over,
    /// above the chart. So the control is an icon alone and refuses input the rest of
    /// the time, and carries the countdown only while there is one.
    ///
    /// The pending half is not decoration: the service refuses a second read while
    /// one is in flight and the framework shows a refusal as a failure, so a control
    /// that forwarded the click while its own command was still unanswered would
    /// answer a fast double click with a banner.
    fn refresh_is_disabled(&self, cx: &impl AppContext) -> bool {
        self.refresh_pending || self.countdown_deadline(cx).is_none()
    }

    /// Whether a pane would enable its drawing controls, which is what a test
    /// asserts instead of reaching into a control it cannot read back.
    #[cfg(test)]
    pub(crate) fn has_scope_for_test(&self) -> bool {
        self.has_scope()
    }

    /// Follows the account the settings now hold.
    ///
    /// Another account is another scope: the answer the pane is showing was served
    /// to the previous one, so it is dropped rather than kept under a new name.
    fn reconcile_account(&mut self, account: Option<SmolStr>, cx: &mut Context<Self>) {
        if self.account_id == account {
            return;
        }
        self.account_id = account;
        self.start(false, cx);
    }

    /// Starts reading, or restarts for an account the user is retrying.
    ///
    /// A retry keeps the chart the user is still reading — blanking it to reconnect
    /// would flash a screen of data away — while an explicit load starts over,
    /// because the pane cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        // The command that was in flight belongs to the generation this call is
        // replacing, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for.
            self.clear_answer(cx);
            self.status.report(
                WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
                cx,
            );
            cx.notify();
            return;
        };
        if !retry {
            self.clear_answer(cx);
        }
        self.status.report(WidgetState::Connecting, cx);
        self.binding.call(
            if retry {
                CryptocurrencyEtfRequest::Retry(key)
            } else {
                CryptocurrencyEtfRequest::Load(key)
            },
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    /// Drops the answer and the chart drawn from it.
    fn clear_answer(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.accepted = None;
        self.chart = None;
        cx.notify();
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut Context<Self>,
    ) {
        self.refresh_pending = false;
        self.apply_rpc_result(result, cx);
    }

    fn apply_refresh_result(
        &mut self,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut Context<Self>,
    ) {
        // An accepted command stays pending until the round it started is reported:
        // the acknowledgement and the round are two different messages, and the gate
        // the service holds spans both.
        if result.is_err() {
            self.refresh_pending = false;
        }
        self.apply_rpc_result(result, cx);
    }

    /// Reported one way, whichever request it answers: the two differ in what they
    /// asked for, not in what a failure means to the reader.
    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(
                CryptocurrencyEtfResponse::StreamStarted | CryptocurrencyEtfResponse::RefreshQueued,
            ) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(CryptocurrencyEtfFailure::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a status is drawn by the
    /// framework around the body, so publishing it is enough.
    fn apply_reliable(
        &mut self,
        key: CryptocurrencyEtfStreamKey,
        message: Arc<CryptocurrencyEtfStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let CryptocurrencyEtfStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            CryptocurrencyEtfStatus::Connecting => self.status.report(WidgetState::Connecting, cx),
            CryptocurrencyEtfStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(WidgetState::Loading, cx);
            }
            CryptocurrencyEtfStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                // The reliable channel and the latest one have no order between
                // them, so this can arrive before the answer it describes: claiming
                // readiness now would be claiming a chart the pane does not have.
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            CryptocurrencyEtfStatus::Failed { error } => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(
                        Arc::new(CryptocurrencyEtfFailure::Fetch(error.clone())),
                        recovery,
                    ),
                    cx,
                );
            }
        }
        false
    }

    /// Applies an answer the stream published.
    ///
    /// Returns whether anything a frame draws changed, which is what the framework
    /// turns into a repaint: the body is cached, and an answer that folds into the
    /// same pixels is not worth a frame.
    fn apply_latest(
        &mut self,
        key: CryptocurrencyEtfStreamKey,
        message: Arc<CryptocurrencyEtfStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let CryptocurrencyEtfStreamMessage::Snapshot { revision, snapshot } = message.as_ref()
        else {
            return false;
        };
        // An answer the pane has already overtaken would only make the chart jump
        // backwards: within one account the service's revision only moves forwards.
        if self.accepted.is_some_and(|held| *revision < held) {
            return false;
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
        {
            return false;
        }
        self.snapshot = Some(snapshot.clone());
        self.accepted = Some(*revision);
        self.sync_chart(cx);
        // Readiness is published at the protocol's own boundary: a complete answer
        // is not the same thing as the answer a status said was complete, and this
        // is where the two meet.
        if self.ready.is_some_and(|ready| *revision >= ready) {
            self.status.report(WidgetState::Ready, cx);
        }
        true
    }

    /// Points the chart at what the config names, out of the answer the pane holds.
    fn sync_chart(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let dataset = snapshot.dataset(self.asset);
        let selection = self.fund.clone();
        let window = self.window;
        match self.chart.as_mut() {
            Some(chart) => {
                chart.set_view(cx, dataset, selection, window);
            }
            None => {
                self.chart = Some(CryptocurrencyEtfChart::new(cx, dataset, selection, window));
            }
        }
        cx.notify();
    }

    /// Drops a fund the asset the pane now shows does not have.
    ///
    /// A fund belongs to one asset's complex, so a choice made under BTC is not a
    /// choice under SOL: it would draw an empty chart for a fund that does not
    /// exist rather than the total the user can read.
    fn normalize_fund(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let dataset = snapshot.dataset(self.asset);
        let CryptocurrencyEtfFundSelection::Named(name) = &self.fund else {
            return;
        };
        if dataset.fund_index(name.as_str()).is_none() {
            self.fund = CryptocurrencyEtfFundSelection::All;
        }
    }

    fn select_asset(&mut self, asset: CryptocurrencyEtfAsset, cx: &mut Context<Self>) {
        if self.asset == asset {
            return;
        }
        self.asset = asset;
        self.normalize_fund();
        self.sync_chart(cx);
    }

    fn select_window(&mut self, window: CryptocurrencyEtfWindow, cx: &mut Context<Self>) {
        if self.window == window {
            return;
        }
        self.window = window;
        self.sync_chart(cx);
    }

    fn select_fund(&mut self, fund: CryptocurrencyEtfFundSelection, cx: &mut Context<Self>) {
        if self.fund == fund {
            return;
        }
        self.fund = fund;
        self.sync_chart(cx);
    }

    /// Reads the account's answer again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, so the command it sends is one the loop answers:
    /// a failure is recovered through the framework's own control, above the chart.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_is_disabled(cx) {
            return;
        }
        self.refresh_pending = true;
        self.binding.call(
            CryptocurrencyEtfRequest::Refresh,
            cx,
            Self::apply_refresh_result,
        );
        cx.notify();
    }

    /// Rebuilds the countdown, asking for the frame that draws it.
    ///
    /// The countdown is the only thing this pane draws that moves on its own, and it
    /// moves once a second: the task wakes four times a second so a second is never
    /// missed, and only the write that changed it asks for a frame. The header is
    /// not cached, so nothing else here has to announce itself.
    ///
    /// The label is the deadline of a ready pane and nothing else, so leaving the
    /// ready state — a read starting, a failure, a reload, an account signing out —
    /// takes the countdown off the button in the same tick that greys it.
    fn tick_countdown(&mut self, cx: &mut Context<Self>) {
        let label = self.countdown_deadline(cx).map(|due| {
            SharedString::new(format_countdown(due.saturating_sub(unix_millis())).as_str())
        });
        if self.countdown == label {
            return;
        }
        self.countdown = label;
        cx.notify();
    }

    /// Runs one countdown tick, which a test drives instead of the clock.
    #[cfg(test)]
    pub(crate) fn tick_countdown_for_test(&mut self, cx: &mut Context<Self>) {
        self.tick_countdown(cx);
    }

    /// The asset picker: which ETF complex the chart is about.
    ///
    /// It is disabled without an account, exactly like the fund and the window: the
    /// asset only says which part of an answer to draw, so with no answer coming it
    /// changes nothing — and a row of controls where one is live for no reason reads
    /// as if the others were broken.
    fn asset_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let selected = SharedString::new_static(self.asset.label());
        CryptocurrencyEtfAsset::ALL
            .iter()
            .fold(
                Select::new(ElementId::Name(SharedString::new_static(
                    "CryptocurrencyEtfAsset",
                )))
                .placeholder(selected.clone())
                .ghost()
                .small()
                .disabled(disabled)
                .selected_item(Some(selected))
                .aria_label(t!(cx, "widget.cryptocurrency_etf.asset")),
                |select, asset| {
                    // A fund's ticker is its name and its id at once: `HYPE` reads the
                    // same in every locale, so there is nothing to keep apart the way the
                    // window's stable ids are kept apart from its labels.
                    select.menu_item(MenuItem::Action(MenuItemAction::new(
                        SharedString::new_static(asset.label()),
                        SharedString::new_static(asset.label()),
                    )))
                },
            )
            .on_change({
                let widget = cx.entity();
                move |selected, _, app| {
                    let Some(asset) = selected.as_deref().and_then(|selected| {
                        CryptocurrencyEtfAsset::ALL
                            .into_iter()
                            .find(|asset| asset.label() == selected)
                    }) else {
                        return;
                    };
                    widget.update(app, |this, cx| this.select_asset(asset, cx));
                }
            })
    }

    /// The header's right group: the refresh, the asset, the fund and the window.
    ///
    /// That order is the order a reader narrows the chart down — how fresh the answer
    /// is, which complex, which fund inside it, and how much of its history — so the
    /// control that changes the most is the one nearest the hand that reaches for the
    /// header.
    ///
    /// There is no separate countdown: it is the refresh button's own label, because
    /// the two are one control's worth of information — when the next read is due,
    /// and the way to bring it forward.
    fn drawing_controls(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = !self.has_scope();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(control("TEST_ETF_REFRESH", self.refresh_button(cx)))
            .child(control("TEST_ETF_ASSET", self.asset_select(cx, disabled)))
            .child(control("TEST_ETF_FUND", self.fund_select(cx, disabled)))
            .child(control("TEST_ETF_WINDOW", self.window_select(cx, disabled)))
            .into_any_element()
    }

    /// The window picker: how much of the history the chart shows.
    ///
    /// It is disabled without an account, exactly like the refresh: the window only
    /// says which part of an answer to draw, so with no answer coming it changes
    /// nothing.
    fn window_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let selected = window_id(self.window);
        CryptocurrencyEtfWindow::ALL
            .iter()
            .fold(
                Select::new(ElementId::Name(SharedString::new_static(
                    "CryptocurrencyEtfWindow",
                )))
                .placeholder(SharedString::new(window_label(cx, self.window)))
                .ghost()
                .small()
                .disabled(disabled)
                .selected_item(Some(SharedString::new_static(selected)))
                .aria_label(t!(cx, "widget.cryptocurrency_etf.time_window")),
                |select, window| {
                    select.menu_item(MenuItem::Action(MenuItemAction::new(
                        SharedString::new_static(window_id(*window)),
                        SharedString::new(window_label(cx, *window)),
                    )))
                },
            )
            .on_change({
                let widget = cx.entity();
                move |selected, _, app| {
                    // The picker identifies a window by its stable id rather than by
                    // the label it draws, so the choice survives a change of
                    // language.
                    let Some(window) = selected.as_deref().and_then(window_from_id) else {
                        return;
                    };
                    widget.update(app, |this, cx| this.select_window(window, cx));
                }
            })
    }

    /// The fund picker: the whole complex, or one fund inside it.
    ///
    /// The funds come from the answer rather than from a list of their own: which
    /// funds an asset has is a property of the data, and a list compiled into the
    /// pane would go stale the first time a fund launched. Before the first answer
    /// there is only the total, which is what a pane shows anyway.
    fn fund_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let funds = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.dataset(self.asset).funds.clone());
        let all_label = t!(cx, "widget.cryptocurrency_etf.all");
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "CryptocurrencyEtfFund",
        )))
        .placeholder(all_label.clone())
        .ghost()
        .small()
        .disabled(disabled)
        .selected_item(Some(fund_id(&self.fund)))
        .aria_label(t!(cx, "widget.cryptocurrency_etf.fund"))
        .menu_item(MenuItem::Action(MenuItemAction::new(
            SharedString::new_static(ALL_FUNDS_ID),
            all_label,
        )));
        if let Some(funds) = funds {
            for fund in funds.iter() {
                select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(format_smolstr!("{FUND_ID_PREFIX}{fund}").as_str()),
                    SharedString::new(fund.as_str()),
                )));
            }
        }
        select.on_change({
            let widget = cx.entity();
            move |selected, _, app| {
                let Some(fund) = selected.as_deref().map(fund_from_id) else {
                    return;
                };
                widget.update(app, |this, cx| this.select_fund(fund, cx));
            }
        })
    }

    /// The control that reads the account's answer again, and the time left until
    /// the next automatic read.
    ///
    /// The countdown is the button's own label rather than a word beside it: it is
    /// the number that says the pane is still alive, and the control that resets it
    /// is the one place a reader would look for it. A pane with no account has no
    /// schedule to count down to, so the button is bare and disabled.
    fn refresh_button(&self, cx: &Context<Self>) -> Button {
        let disabled = self.refresh_is_disabled(cx);
        Button::new(ElementId::Name(SharedString::new_static(
            "CryptocurrencyEtfRefresh",
        )))
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(t!(cx, "widget.cryptocurrency_etf.refresh_now"))
        .child(Icon::new(IconKind::ArrowsClockwise).size_3())
        .children(self.countdown.clone())
        .on_click({
            let widget = cx.entity();
            move |_, _, app| {
                widget.update(app, |this, cx| this.refresh(cx));
            }
        })
    }

    /// The row of readings above the chart: the hovered day, and what it says.
    ///
    /// It shows the last day when the pointer is elsewhere, so the row is never a
    /// gap: a pane that has an answer always has a reading to show, and which one it
    /// is only follows the pointer.
    fn metrics_row(&self, metrics: crate::chart::Metrics, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.roles.fg.muted.hsla;
        let foreground = theme.roles.fg.default.hsla;
        let mut row = div()
            .debug_selector(|| "TEST_ETF_ROW".into())
            .h(px(METRICS_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .border_b_1()
            .border_color(theme.roles.line.divider.hsla);
        row = row.child(metric(
            t!(cx, "widget.cryptocurrency_etf.date"),
            metrics.date,
            muted,
            foreground,
        ));
        if let Some(net_assets) = metrics.net_assets {
            row = row
                .child(metric(
                    metrics.caption,
                    metrics.flow,
                    muted,
                    flow(theme, metrics.flow_value, muted),
                ))
                .child(metric(
                    t!(cx, "widget.cryptocurrency_etf.total_net_assets"),
                    net_assets,
                    muted,
                    foreground,
                ));
        } else {
            row = row.child(metric(
                metrics.caption,
                metrics.flow,
                muted,
                flow(theme, metrics.flow_value, muted),
            ));
        }
        row.into_any_element()
    }
}

impl Render for CryptocurrencyEtfWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the ready
        // state and nothing else. Everything the pane is *doing* is drawn by the
        // framework around this.
        if let Some(chart) = self.chart.as_mut() {
            // Two things in one frame are drawn from the pointer, so the queued
            // position is applied here rather than during the paint pass: a commit in
            // paint would leave the row above the chart a frame behind the chart
            // itself. The labels are re-resolved here too, because an answer arrives
            // every eight hours and a chart that kept the old language until then
            // would be wrong for most of a working day.
            chart.sync_locale(cx);
            chart.commit_pointer();
        }
        let Some(chart) = self.chart.as_ref() else {
            return div().size_full().min_h_0().into_any_element();
        };
        let metrics = chart.metrics();
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .children(metrics.map(|metrics| self.metrics_row(metrics, cx)))
            // The chart area is what is left after the row above it. The chart itself
            // owns everything below this line — its minimum size, its two scroll
            // containers and the axes they make room for — so the pane does not
            // impose a width of its own here: a pane narrower than the axes need
            // scrolls inside the chart rather than clipping an axis nobody can read.
            .child(div().flex_1().min_h_0().child(chart.entity(cx)))
            .into_any_element()
    }
}

/// One reading of the row: what it is, then the number.
fn metric(
    title: impl Into<SharedString>,
    value: impl Into<SharedString>,
    label_color: gpui_kit::Hsla,
    color: gpui_kit::Hsla,
) -> AnyElement {
    div()
        .min_w_0()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(label_color)
                .truncate()
                .child(title.into()),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(color)
                .truncate()
                .child(value.into()),
        )
        .into_any_element()
}

/// One header control, in a box of its own.
///
/// The controls themselves are `base_ui` elements and carry no debug selector, and
/// the order they sit in is part of what this pane promises: a box per control is
/// what lets a test measure that order.
fn control(selector: &'static str, control: impl IntoElement) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .child(control)
        .into_any_element()
}

/// The stable id of the fund choice that means every fund.
const ALL_FUNDS_ID: &str = "all";

/// What a fund's id is built from, so a fund called `all` could not collide with the
/// choice above.
const FUND_ID_PREFIX: &str = "fund:";

/// The id a window is chosen by, which is persisted in the picker rather than in the
/// config: it is what keeps a choice readable across a change of language.
fn window_id(window: CryptocurrencyEtfWindow) -> &'static str {
    match window {
        CryptocurrencyEtfWindow::Days30 => "days_30",
        CryptocurrencyEtfWindow::Days90 => "days_90",
        CryptocurrencyEtfWindow::Days180 => "days_180",
        CryptocurrencyEtfWindow::Days360 => "days_360",
        CryptocurrencyEtfWindow::All => "all",
    }
}

fn window_from_id(id: &str) -> Option<CryptocurrencyEtfWindow> {
    CryptocurrencyEtfWindow::ALL
        .into_iter()
        .find(|window| window_id(*window) == id)
}

/// How a window is named: a number of trading days, or all of them.
fn window_label(cx: &App, window: CryptocurrencyEtfWindow) -> SharedString {
    match window {
        CryptocurrencyEtfWindow::Days30 => t!(cx, "widget.cryptocurrency_etf.days", count = 30),
        CryptocurrencyEtfWindow::Days90 => t!(cx, "widget.cryptocurrency_etf.days", count = 90),
        CryptocurrencyEtfWindow::Days180 => t!(cx, "widget.cryptocurrency_etf.days", count = 180),
        CryptocurrencyEtfWindow::Days360 => t!(cx, "widget.cryptocurrency_etf.days", count = 360),
        CryptocurrencyEtfWindow::All => t!(cx, "widget.cryptocurrency_etf.all"),
    }
}

fn fund_id(fund: &CryptocurrencyEtfFundSelection) -> SharedString {
    match fund {
        CryptocurrencyEtfFundSelection::All => SharedString::new_static(ALL_FUNDS_ID),
        CryptocurrencyEtfFundSelection::Named(name) => {
            SharedString::new(format_smolstr!("{FUND_ID_PREFIX}{name}").as_str())
        }
    }
}

fn fund_from_id(id: &str) -> CryptocurrencyEtfFundSelection {
    match id.strip_prefix(FUND_ID_PREFIX) {
        Some(name) => CryptocurrencyEtfFundSelection::Named(SmolStr::new(name)),
        None => CryptocurrencyEtfFundSelection::All,
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use gpui_kit::{
        AppContext, BorrowAppContext, Bounds, Entity, IntoElement, Modifiers, Pixels, Render,
        TestAppContext, VisualTestContext, Window, div, point, px,
    };
    use i18n::language::codes::LanguageCode;

    use super::*;
    use crate::{
        model::CryptocurrencyEtfSnapshot,
        service::test_support::{DEFAULT_FUNDS, FakeSource, FetchPlan, asset, snapshot_of},
    };

    /// The account every fixture pane is scoped to.
    const ACCOUNT: &str = "test-account";

    /// Settings holding `account`, or holding nothing when it is `None`.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    /// Installs the application state a pane is built on: the theme, the translator
    /// and the runtime.
    ///
    /// Each is an application-wide global an application installs exactly once, so a
    /// test that needs two panes in one application installs them once and scopes the
    /// panes with [`sign_in`] rather than installing them again.
    fn install(cx: &mut App) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
    }

    /// Scopes the panes built from here on to `account`, or to nobody.
    fn sign_in(cx: &mut App, account: Option<&str>) {
        let manager = ConfigManager::in_memory(settings(account), cx);
        cx.set_global(manager);
    }

    /// Lets the pane and its service run until what the test is waiting for happens.
    ///
    /// Two executors are involved: the pane sends its commands from GPUI's own, and the
    /// service answers on the application runtime. So both are given a turn — the pane
    /// first, because a command that has not been sent is a service with nothing to answer.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A pane the framework would have built, driven by a fake that never answers.
    ///
    /// The fake never answers because the tests below are the only thing that reports
    /// to the pane: a source that answered would race them, since its service
    /// publishes on a task of its own.
    fn pane_with_source<S>(
        source: S,
        cx: &mut Context<CryptocurrencyEtfWidget>,
    ) -> CryptocurrencyEtfWidget
    where
        S: CryptocurrencyEtfSource,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        CryptocurrencyEtfWidget::new_with_source(
            CryptocurrencyEtfConfig,
            source,
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    }

    fn pane_with(
        cx: &mut TestAppContext,
        account: Option<&str>,
    ) -> Entity<CryptocurrencyEtfWidget> {
        cx.update(install);
        pane_over(cx, account)
    }

    /// The same pane over the state [`install`] already put in place: what a test
    /// building two panes for two accounts builds its second one with.
    fn pane_over(
        cx: &mut TestAppContext,
        account: Option<&str>,
    ) -> Entity<CryptocurrencyEtfWidget> {
        cx.executor().allow_parking();
        cx.update(|cx| {
            sign_in(cx, account);
            cx.new(|cx| {
                let mut pane = pane_with_source(FakeSource::new([FetchPlan::Pending]), cx);
                pane.load(cx);
                pane
            })
        })
    }

    fn pane(cx: &mut TestAppContext) -> Entity<CryptocurrencyEtfWidget> {
        pane_with(cx, Some(ACCOUNT))
    }

    fn key() -> CryptocurrencyEtfStreamKey {
        CryptocurrencyEtfStreamKey {
            account_id: SmolStr::new_static(ACCOUNT),
        }
    }

    /// Reports a status as the stream would.
    ///
    /// Cancelling the binding first is what makes the reported message the only input
    /// the pane has: the load it was still holding is dropped with the runtime.
    fn report_status(
        pane: &Entity<CryptocurrencyEtfWidget>,
        status: CryptocurrencyEtfStatus,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| {
            pane.binding.cancel();
            pane.apply_reliable(
                key(),
                Arc::new(CryptocurrencyEtfStreamMessage::Status(status)),
                context,
            )
        })
    }

    /// Reports an answer as the stream would.
    fn report_snapshot(
        pane: &Entity<CryptocurrencyEtfWidget>,
        revision: u64,
        snapshot: Arc<CryptocurrencyEtfSnapshot>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| {
            pane.binding.cancel();
            pane.apply_latest(
                key(),
                Arc::new(CryptocurrencyEtfStreamMessage::Snapshot { revision, snapshot }),
                context,
            )
        })
    }

    fn ready(revision: u64) -> CryptocurrencyEtfStatus {
        CryptocurrencyEtfStatus::Ready {
            revision,
            next_refresh_at: i64::MAX,
        }
    }

    fn failed() -> CryptocurrencyEtfStatus {
        CryptocurrencyEtfStatus::Failed {
            error: CryptocurrencyEtfFetchError::Service(
                kytrix_service_provider::service::engine::KytrixServiceError::Disconnected,
            ),
        }
    }

    fn refreshing() -> CryptocurrencyEtfStatus {
        CryptocurrencyEtfStatus::Refreshing {
            next_refresh_at: i64::MAX,
        }
    }

    /// An answer whose first asset has the funds a test names.
    fn snapshot_with(funds: &[&str]) -> Arc<CryptocurrencyEtfSnapshot> {
        snapshot_of([
            asset(CryptocurrencyEtfAsset::Btc, &[1.0, 2.0], funds),
            asset(CryptocurrencyEtfAsset::Eth, &[1.0, 2.0], DEFAULT_FUNDS),
            asset(CryptocurrencyEtfAsset::Sol, &[1.0, 2.0], DEFAULT_FUNDS),
            asset(CryptocurrencyEtfAsset::Hyp, &[1.0, 2.0], DEFAULT_FUNDS),
        ])
    }

    #[gpui_kit::test]
    fn a_pane_without_an_account_should_ask_for_one(cx: &mut TestAppContext) {
        let pane = pane_with(cx, None);

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
                "a pane nobody is signed in for says what it is waiting for"
            );
            assert!(!pane.read(cx).has_content());
            assert!(
                !pane.read(cx).has_scope_for_test(),
                "there is nothing to read without an account"
            );
        });
    }

    /// A pane nobody is signed in for waits in the language the interface shows: the notice is
    /// a reference into the catalogue, resolved on the frame that draws it, so the pane reports
    /// nothing again — no reload, and nothing asked of the service.
    #[gpui_kit::test]
    fn a_language_change_should_resolve_the_signed_out_notice_where_it_is_drawn(
        cx: &mut TestAppContext,
    ) {
        let pane = pane_with(cx, None);
        let english = cx.update(|cx| t!(cx, "widget.cryptocurrency_etf.logged_out"));

        let notice = cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("a pane nobody signed in for waits");
            assert_eq!(
                notice,
                Notice::message("widget.cryptocurrency_etf.logged_out")
            );
            assert_eq!(
                notice.resolve(cx),
                english,
                "the pane waits in the language it is being read in"
            );

            notice
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));
        cx.run_until_parked();

        cx.update(|cx| {
            let chinese = t!(cx, "widget.cryptocurrency_etf.logged_out");
            assert_ne!(chinese, english, "the two locales have to differ");
            assert_eq!(
                notice.resolve(cx),
                chinese,
                "and the same reference resolves in the language showing now, without a reload"
            );
        });
    }

    #[gpui_kit::test]
    fn an_account_should_be_what_enables_the_controls(cx: &mut TestAppContext) {
        cx.update(install);
        let signed_out = pane_over(cx, None);
        let pane = pane_over(cx, Some(ACCOUNT));

        cx.update(|cx| {
            assert!(!signed_out.read(cx).has_scope_for_test());
            assert!(
                signed_out.read(cx).refresh_is_disabled(cx),
                "there is nothing to read without an account"
            );
            assert!(pane.read(cx).has_scope_for_test());
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "a pane already waiting for its first answer has nothing to bring forward"
            );
        });

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(&pane, ready(1), cx);

            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "an idle pane with an answer can read again on demand"
            );
        });
    }

    /// The double-click the refresh control exists to absorb.
    ///
    /// A refresh is refused by the service while one is in flight, and the framework
    /// shows a refusal as a failure — so a control that forwarded a second click
    /// inside that window would answer a fast double click with a banner.
    #[gpui_kit::test]
    fn a_refresh_should_not_be_forwarded_while_an_answer_is_being_read(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(&pane, ready(1), cx);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "an idle pane with an answer can read again on demand"
            );
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(
                    pane.refresh_is_disabled(cx),
                    "the click closes the window itself, before anything has answered"
                );
                // The service acknowledges the command before it runs the round it
                // started, so the acknowledgement is not the end of the window.
                pane.apply_refresh_result(Ok(CryptocurrencyEtfResponse::RefreshQueued), cx);
                assert!(
                    pane.refresh_is_disabled(cx),
                    "a queued read is still a read in flight"
                );
            });
        });

        cx.update(|cx| {
            report_status(&pane, refreshing(), cx);
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "a read in flight has nothing to bring forward"
            );
        });

        cx.update(|cx| {
            report_status(&pane, ready(2), cx);
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "readiness without the answer it describes is not a reason to read again"
            );
            report_snapshot(&pane, 2, snapshot_with(DEFAULT_FUNDS), cx);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "and with the answer in hand there is nothing left to wait for"
            );
        });
    }

    /// A failure is the framework's to recover from, not the header's.
    ///
    /// A failed pane takes no more of its stream's messages, so a refresh command
    /// sent from here would be accepted by the loop and its answer dropped by the
    /// pane. The control is therefore greyed with the rest of the states that are not
    /// ready, and the way out is the recovery the banner above the chart offers.
    #[gpui_kit::test]
    fn a_failed_pane_should_offer_its_recovery_rather_than_a_refresh(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(&pane, failed(), cx);

            assert!(pane.read(cx).status.is_failed(cx));
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "the header has nothing to bring forward, so it offers nothing"
            );
            let failure = pane
                .read(cx)
                .status
                .state(cx)
                .failure()
                .cloned()
                .expect("a failure should be published");
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "and the framework's own recovery is the way out of it"
            );
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.refresh(cx));

            assert!(
                pane.read(cx).status.is_failed(cx),
                "a disabled control changes nothing when it is pressed"
            );
        });
    }

    /// The countdown belongs to a ready pane, and only to a ready pane.
    ///
    /// A read in flight has a deadline too — the one it will arm when it lands — but
    /// counting down to it before the answer is on screen would be counting down to an
    /// answer the pane does not have yet. So the label appears with readiness and is
    /// taken away by anything that is not it, and the control is greyed with a bare
    /// icon for the same stretch.
    #[gpui_kit::test]
    fn the_countdown_should_only_run_while_the_pane_is_ready(cx: &mut TestAppContext) {
        let pane = pane(cx);
        let tick = |pane: &Entity<CryptocurrencyEtfWidget>, cx: &mut App| {
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
        };

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(
                &pane,
                CryptocurrencyEtfStatus::Ready {
                    revision: 1,
                    next_refresh_at: unix_millis() + 8 * 60 * 60 * 1_000,
                },
                cx,
            );
            tick(&pane, cx);

            assert_eq!(
                pane.read(cx).countdown.clone(),
                Some(SharedString::new_static("08:00:00")),
                "a ready pane counts down to the deadline the service published"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));
        });

        cx.update(|cx| {
            report_status(&pane, refreshing(), cx);
            tick(&pane, cx);

            assert!(
                pane.read(cx).countdown.is_none(),
                "a read in flight has no countdown to show"
            );
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "and nothing for the control to bring forward"
            );
        });

        cx.update(|cx| {
            report_status(
                &pane,
                CryptocurrencyEtfStatus::Ready {
                    revision: 2,
                    next_refresh_at: unix_millis() + 60_000,
                },
                cx,
            );
            tick(&pane, cx);

            assert!(
                pane.read(cx).countdown.is_none(),
                "readiness without the answer it describes is not something to count down from"
            );
            assert!(pane.read(cx).refresh_is_disabled(cx));

            report_snapshot(&pane, 2, snapshot_with(DEFAULT_FUNDS), cx);
            tick(&pane, cx);

            assert_eq!(
                pane.read(cx).countdown.clone(),
                Some(SharedString::new_static("01:00")),
                "and with the answer in hand the countdown starts on the new deadline"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));
        });
    }

    /// A failure takes the loop off the clock, so there is no countdown to show.
    #[gpui_kit::test]
    fn a_failed_pane_should_stop_counting_down(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(&pane, ready(1), cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(
                pane.read(cx).countdown.is_some(),
                "a ready pane counts down to its next read"
            );

            report_status(&pane, failed(), cx);
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert!(
                pane.read(cx).countdown.is_none(),
                "a pane whose schedule stopped has nothing to count down to"
            );
        });
    }

    #[gpui_kit::test]
    fn a_pane_with_an_account_should_draw_what_arrives(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            assert!(
                !pane.read(cx).has_content(),
                "a pane is not content until it has an answer"
            );
        });

        cx.update(|cx| {
            report_status(&pane, ready(1), cx);
            assert!(
                !pane.read(cx).has_content(),
                "readiness before the answer it describes is not content"
            );
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Loading);
        });

        cx.update(|cx| {
            let changed = report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);

            assert!(changed, "accepting an answer is a change worth a frame");
            assert!(pane.read(cx).has_content());
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
            assert!(!pane.read(cx).status.is_failed(cx));
        });
    }

    #[gpui_kit::test]
    fn an_answer_arriving_before_its_readiness_should_still_be_drawn(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(&pane, ready(1), cx);
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);

            assert!(pane.read(cx).has_content());
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        });
    }

    #[gpui_kit::test]
    fn readiness_for_a_newer_answer_should_not_claim_the_older_one(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(&pane, ready(2), cx);
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);

            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Loading,
                "an answer older than the readiness that arrived is not what was reported ready"
            );
            assert!(!pane.read(cx).status.is_failed(cx));
        });
    }

    #[gpui_kit::test]
    fn an_answer_the_pane_has_overtaken_should_be_ignored(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 2, snapshot_with(DEFAULT_FUNDS), cx);
            let changed = report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);

            assert!(
                !changed,
                "an answer older than the one on screen would only make the chart jump back"
            );
            assert_eq!(pane.read(cx).accepted, Some(2));
        });
    }

    #[gpui_kit::test]
    fn an_answer_from_another_account_should_be_ignored(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            let changed = pane.update(cx, |pane, context| {
                pane.binding.cancel();
                pane.apply_latest(
                    CryptocurrencyEtfStreamKey {
                        account_id: SmolStr::new_static("another-account"),
                    },
                    Arc::new(CryptocurrencyEtfStreamMessage::Snapshot {
                        revision: 1,
                        snapshot: snapshot_with(DEFAULT_FUNDS),
                    }),
                    context,
                )
            });

            assert!(!changed, "another account's answer is not this pane's");
            assert!(!pane.read(cx).has_content());
        });
    }

    #[gpui_kit::test]
    fn a_failure_should_keep_the_answer_on_screen_and_offer_a_recovery(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(&pane, failed(), cx);

            assert!(pane.read(cx).status.is_failed(cx));
            let failure = pane
                .read(cx)
                .status
                .state(cx)
                .failure()
                .cloned()
                .expect("a failure should be published");
            assert!(
                !failure.text(cx).is_empty(),
                "a failure has to say what went wrong"
            );
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "a failure always offers a way out"
            );
            assert!(
                pane.read(cx).has_content(),
                "the chart the user was reading stays on screen"
            );
            assert!(
                pane.read(cx).countdown.is_none(),
                "a failure is off the clock, so there is nothing to count down to"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_should_be_terminal_until_the_user_recovers(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(&pane, failed(), cx);
            assert!(pane.read(cx).status.is_failed(cx));

            let status_changed = report_status(&pane, CryptocurrencyEtfStatus::Connecting, cx);
            let answer_changed = report_snapshot(&pane, 7, snapshot_with(DEFAULT_FUNDS), cx);

            assert!(!status_changed, "a failed pane takes no more statuses");
            assert!(!answer_changed, "a failed pane takes no more answers");
            assert!(pane.read(cx).status.is_failed(cx));

            // The recovery the failure offered is what starts it again.
            pane.update(cx, |pane, cx| pane.load(cx));

            assert!(!pane.read(cx).status.is_failed(cx));
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
        });
    }

    #[gpui_kit::test]
    fn the_countdown_should_be_the_refresh_buttons_own_label(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            report_status(
                &pane,
                CryptocurrencyEtfStatus::Ready {
                    revision: 1,
                    next_refresh_at: unix_millis() + 8 * 60 * 60 * 1_000,
                },
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown_for_test(cx));

            let countdown = pane
                .read(cx)
                .countdown
                .clone()
                .expect("a ready pane counts down to its next read");
            assert!(
                countdown
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == ':'),
                "the label is the time left on its own: {countdown}"
            );
            assert_eq!(
                countdown.as_ref().len(),
                8,
                "an eight-hour interval is read as a clock, hours included: {countdown}"
            );
            assert!(
                pane.read(cx)
                    .next_refresh_at
                    .is_some_and(|due| due > unix_millis()),
                "the pane counts down to a read that is still ahead of it"
            );
        });
    }

    #[gpui_kit::test]
    fn the_window_and_the_fund_should_redraw_without_reading_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (pane, reads) = cx.update(|cx| {
            install(cx);
            sign_in(cx, Some(ACCOUNT));
            let source = FakeSource::new([FetchPlan::Pending]);
            let reads = source.clone();
            let pane = cx.new(|cx| {
                let mut pane = pane_with_source(source, cx);
                pane.load(cx);
                pane
            });
            (pane, reads)
        });

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            let reads_before = reads.fetch_count();

            pane.update(cx, |pane, cx| {
                pane.select_window(CryptocurrencyEtfWindow::Days90, cx);
                pane.select_fund(CryptocurrencyEtfFundSelection::All, cx);
            });
            pane.update(cx, |pane, cx| {
                pane.select_window(CryptocurrencyEtfWindow::Days90, cx)
            });

            assert_eq!(pane.read(cx).window, CryptocurrencyEtfWindow::Days90);
            assert!(
                pane.read(cx).has_content(),
                "another window is the same answer drawn differently"
            );
            assert_eq!(
                reads.fetch_count(),
                reads_before,
                "a view of the answer that is already here is not worth another read"
            );
        });
    }

    #[gpui_kit::test]
    fn a_fund_the_new_asset_does_not_have_should_fall_back_to_the_whole_complex(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(&["IBIT", "FBTC"]), cx);
            pane.update(cx, |pane, cx| {
                pane.select_fund(
                    CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("IBIT")),
                    cx,
                );
            });
            assert_eq!(
                pane.read(cx).fund,
                CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("IBIT"))
            );

            pane.update(cx, |pane, cx| {
                pane.select_asset(CryptocurrencyEtfAsset::Eth, cx)
            });

            assert_eq!(pane.read(cx).asset, CryptocurrencyEtfAsset::Eth);
            assert_eq!(
                pane.read(cx).fund,
                CryptocurrencyEtfFundSelection::All,
                "a fund belongs to one asset's complex, so a choice under BTC is not one under ETH"
            );
            assert!(pane.read(cx).has_content());
        });
    }

    #[gpui_kit::test]
    fn another_account_should_take_the_answer_with_it(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            assert!(pane.read(cx).has_content());

            pane.update(cx, |pane, cx| {
                pane.reconcile_account(Some(SmolStr::new_static("second-account")), cx);
            });

            assert!(
                !pane.read(cx).has_content(),
                "the answer was served to the account that just left"
            );
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
            assert_eq!(
                pane.read(cx).asset,
                CryptocurrencyEtfAsset::Btc,
                "the view is kept"
            );
        });
    }

    #[gpui_kit::test]
    fn signing_out_should_take_the_answer_with_it(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            assert!(!pane.read(cx).has_content());
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
                "and the pane asks for an account rather than reporting a failure"
            );
            assert!(!pane.read(cx).has_scope_for_test());
        });
    }

    /// Another account is another read, and the pane hears about it from the settings
    /// rather than from a reload: what the new account is owed is fetched for it.
    #[gpui_kit::test]
    fn an_account_change_should_read_the_new_account(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (pane, reads) = cx.update(|cx| {
            install(cx);
            sign_in(cx, Some(ACCOUNT));
            let source = FakeSource::new([FetchPlan::Pending]);
            let reads = source.clone();
            let pane = cx.new(|cx| {
                let mut pane = pane_with_source(source, cx);
                pane.load(cx);
                pane
            });
            (pane, reads)
        });
        settle_until(cx, "the pane to read", |_| reads.fetch_count() == 1);

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.account = Some(AccountConfig {
                            user_id: "someone-else".to_owned(),
                            wallet_address: "0x0000000000000000000000000000000000000002".to_owned(),
                            wallet_kind: AccountWalletKind::Ethereum,
                        });
                    },
                    cx,
                );
            });
        });

        settle_until(cx, "the replacement read", |_| reads.fetch_count() == 2);
        cx.update(|cx| {
            assert_eq!(pane.read(cx).account_id.as_deref(), Some("someone-else"));
        });
    }

    /// One pane mounted in a window of its own, which is what paints it.
    struct Harness {
        pane: Entity<CryptocurrencyEtfWidget>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.pane.clone())
        }
    }

    #[gpui_kit::test]
    fn a_pane_with_content_should_paint_its_row_of_readings(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(install);
        cx.update(|cx| sign_in(cx, Some(ACCOUNT)));
        let (harness, cx): (Entity<Harness>, &mut VisualTestContext) =
            cx.add_window_view(|_, cx| {
                let pane = cx.new(|cx| {
                    let mut pane = pane_with_source(FakeSource::new([FetchPlan::Pending]), cx);
                    pane.load(cx);
                    pane
                });
                Harness { pane }
            });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("TEST_ETF_ROW").is_none(),
            "a pane with nothing to show has no reading to paint"
        );

        cx.update(|_, cx| {
            let pane = harness.read(cx).pane.clone();
            report_snapshot(&pane, 1, snapshot_with(DEFAULT_FUNDS), cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let row = cx
            .debug_bounds("TEST_ETF_ROW")
            .expect("a pane with an answer paints the day, the flow and what the funds hold");
        cx.simulate_mouse_move(
            point(row.center().x, row.center().y + px(80.0)),
            None,
            Modifiers::default(),
        );
        cx.run_until_parked();
    }

    /// One pane's header row, mounted in a window of its own.
    ///
    /// The pane chrome is what lays a header out in the application, and it is not
    /// part of this crate; the harness stands in for it, so the controls are built
    /// and painted exactly the way a frame builds them.
    struct HeaderHarness {
        pane: Entity<CryptocurrencyEtfWidget>,
    }

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let header = self.pane.update(cx, |pane, cx| pane.header(window, cx));
            div()
                .size_full()
                .flex()
                .child(
                    div()
                        .debug_selector(|| "TEST_ETF_HEADER_LEFT".into())
                        .children(header.left),
                )
                .child(
                    div()
                        .flex_1()
                        .debug_selector(|| "TEST_ETF_HEADER_DRAG".into()),
                )
                .child(
                    div()
                        .debug_selector(|| "TEST_ETF_HEADER_RIGHT".into())
                        .children(header.right),
                )
        }
    }

    /// Mounts one pane's header row in a window of its own, for `account` or for
    /// nobody.
    fn header_window<'a>(
        cx: &'a mut TestAppContext,
        account: Option<&str>,
    ) -> (Entity<HeaderHarness>, &'a mut VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx);
            sign_in(cx, account);
        });
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let pane = cx.new(|cx| {
                let mut pane = pane_with_source(FakeSource::new([FetchPlan::Pending]), cx);
                pane.load(cx);
                pane
            });
            HeaderHarness { pane }
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        (harness, cx)
    }

    /// The boxes of the header's controls, in the order they are drawn.
    fn control_bounds(cx: &mut VisualTestContext) -> [Bounds<Pixels>; 4] {
        [
            "TEST_ETF_REFRESH",
            "TEST_ETF_ASSET",
            "TEST_ETF_FUND",
            "TEST_ETF_WINDOW",
        ]
        .map(|selector| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} should be shown"))
        })
    }

    /// The whole header is one group, and the order of it is the order a reader
    /// narrows the chart down: how fresh the answer is, which complex, which fund
    /// inside it, and how much of its history.
    #[gpui_kit::test]
    fn the_header_should_offer_its_controls_in_the_order_they_narrow_the_chart_down(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = header_window(cx, Some(ACCOUNT));

        let bounds = control_bounds(cx);
        assert!(
            bounds
                .windows(2)
                .all(|pair| pair[0].right() <= pair[1].left()),
            "the controls run from the refresh to the window, left to right: {bounds:?}"
        );
        assert!(
            cx.debug_bounds("TEST_ETF_HEADER_RIGHT").is_some(),
            "and they are the header's right group"
        );

        // The controls are rebuilt every frame, so a second one is what proves the
        // asset picker keeps its own state rather than being built once.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert_eq!(
            cx.debug_bounds("TEST_ETF_ASSET")
                .expect("the asset picker is shown again"),
            bounds[1],
            "a header does not move between frames"
        );
        assert!(
            harness.read_with(cx, |harness, cx| harness.pane.read(cx).has_scope_for_test()),
            "the pane behind it is still the one the header belongs to"
        );

        let header = cx.update(|window, cx| {
            harness.update(cx, |harness, cx| {
                harness.pane.update(cx, |pane, cx| pane.header(window, cx))
            })
        });
        assert!(
            header.left.is_none(),
            "the assets are a picker in the right group now, not a tab row on the left"
        );
    }

    /// A pane nobody is signed in for draws the same row, greyed rather than gone.
    ///
    /// The controls are disabled, not missing, so the header does not reflow when an
    /// account signs in or out. What a disabled control *looks* like is `base_ui`'s;
    /// that it is still there, and still in the same order, is this pane's.
    #[gpui_kit::test]
    fn the_header_should_keep_its_controls_without_an_account(cx: &mut TestAppContext) {
        let (_, cx) = header_window(cx, None);

        let bounds = control_bounds(cx);
        assert!(
            bounds.iter().all(|bounds| bounds.size.width > px(0.0)),
            "every control is still drawn: {bounds:?}"
        );
        assert!(
            bounds
                .windows(2)
                .all(|pair| pair[0].right() <= pair[1].left()),
            "and in the same order"
        );
    }

    #[test]
    fn a_countdown_should_never_read_as_a_smaller_number_than_it_is() {
        assert_eq!(
            format_countdown(8 * 60 * 60 * 1_000).as_str(),
            "08:00:00",
            "an eight-hour interval counts down in hours"
        );
        assert_eq!(
            format_countdown(59_001).as_str(),
            "01:00",
            "and stops showing them once the last hour is all that is left"
        );
    }

    #[test]
    fn a_window_should_round_trip_through_the_id_its_picker_uses() {
        for window in CryptocurrencyEtfWindow::ALL {
            assert_eq!(window_from_id(window_id(window)), Some(window));
        }
        assert_eq!(window_from_id("not a window"), None);
    }

    #[test]
    fn a_fund_choice_should_round_trip_through_the_id_its_picker_uses() {
        let all = CryptocurrencyEtfFundSelection::All;
        let named = CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("IBIT"));

        assert_eq!(fund_from_id(&fund_id(&all)), all);
        assert_eq!(fund_from_id(&fund_id(&named)), named);
        assert_ne!(fund_id(&all), fund_id(&named));
        assert_eq!(
            fund_from_id(ALL_FUNDS_ID),
            CryptocurrencyEtfFundSelection::All,
            "the picker's own id for the whole complex is what it opens with"
        );
    }

    /// The pane persists nothing, so a fresh one opens on the deepest complex, its most
    /// recent month and every fund in it.
    #[gpui_kit::test]
    fn a_fresh_pane_should_open_on_the_deepest_complex_its_recent_month_and_every_fund(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx);

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.asset, CryptocurrencyEtfAsset::Btc);
            assert_eq!(pane.window, CryptocurrencyEtfWindow::Days30);
            assert_eq!(pane.fund, CryptocurrencyEtfFundSelection::All);
        });
        assert_eq!(
            <CryptocurrencyEtfConfig as WidgetConfig>::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
        assert_eq!(
            CryptocurrencyEtfWidget::ID.as_str(),
            "kytrix.cryptocurrency_etf"
        );
    }
}
