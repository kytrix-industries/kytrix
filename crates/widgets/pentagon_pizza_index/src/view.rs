//! The Pentagon Pizza Index pane: the account it is scoped to, the reading it draws,
//! the ladder it colours it with, and the refresh it counts down to.
//!
//! # What the pane is scoped to
//!
//! The account. The index is the same for every reader, but the answer is served over
//! an authenticated connection, so it is not one a pane may keep showing after the
//! account behind it changed. A reader with no account behind them stops the read that
//! belonged to the account that left and says so: that is a *waiting* state rather
//! than a failure, because nothing has gone wrong — the pane is just not for an
//! anonymous reader. Nothing is retried on a timer either; the account event
//! subscription re-scopes the pane the moment the user signs in.
//!
//! # What it reports, and what it does not
//!
//! It reports the poll through the framework's status cell and nothing else: a
//! failure is a failure, and the framework decides where that is shown and what
//! the recovery looks like. The pane keeps no banner and no "unexpected response"
//! state of its own; a response that does not match the request cannot happen,
//! because both are typed.
//!
//! # Why the colour is a status role
//!
//! The ladder is a severity, so it is drawn from the theme's status colors rather
//! than from a palette of its own: `success` for the calmest rung, `info`, `warning`,
//! `elevated` and `danger` for the most severe. That is what keeps the pane legible
//! in both appearances and keeps a red a reader has learned from an error banner
//! meaning the same thing here.
//!
//! # Where the countdown lives
//!
//! It is the refresh button's own label rather than a word beside it. The two are
//! one control's worth of information — when the next read is due, and the way to
//! bring it forward — so they are one control. The index publishes one point an
//! hour, so the countdown is the only thing on screen that moves on its own.

use std::{sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, base::Disableable, div, px,
};
use i18n::t;
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use time::{OffsetDateTime, UtcOffset};
use widget::{
    account::account_of,
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::data_time_zone,
    util::{format_countdown, unix_millis},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    model::{PizzaLevel, PizzaReading},
    service::{
        PizzaFetchError, PizzaProtocol, PizzaRequest, PizzaService, PizzaServiceError, PizzaSource,
        PizzaStatus, PizzaStreamKey, PizzaStreamMessage,
    },
};

/// How often the pane re-reads the clock behind the countdown.
///
/// It is not a fetch: it only re-checks the deadline the service published. A
/// quarter of a second is faster than the label's own resolution and cheaper than
/// a frame.
pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// How tall the colour bar under the reading is.
const LEVEL_BAR_HEIGHT: f32 = 6.0;

/// The pane persists nothing: the index is the same index for every reader.
///
/// The type exists because every widget carries the schema version its config is
/// stored under, and because a widget that later gains an option has somewhere to
/// put it without a second type.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PentagonPizzaIndexConfig;

impl WidgetConfig for PentagonPizzaIndexConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PizzaFailure {
    /// The stream reported the failure itself.
    Fetch(PizzaFetchError),
    /// The request the pane sent was refused.
    Rpc(RpcCallError<PizzaServiceError>),
}

impl LocalizedError for PizzaFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// One pane: the account it follows, the reading it holds, and the countdown to the
/// next one.
pub struct PentagonPizzaIndexWidget {
    binding: WidgetBinding<PizzaProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    /// The reading the pane is showing, once a round has published one.
    reading: Option<Arc<PizzaReading>>,
    /// The revision of that reading, which is what readiness is measured against.
    accepted: Option<u64>,
    /// The revision the service has stated complete.
    ///
    /// It arrives on the other channel, so it can arrive before the reading it
    /// describes; claiming readiness on the status alone would claim a reading the
    /// pane does not have.
    ready: Option<u64>,
    /// When the next automatic read is due, as the service's own deadline.
    next_refresh_at: Option<i64>,
    /// The text the refresh button carries while a countdown is running.
    countdown: Option<SharedString>,
    /// Whether a refresh command this pane sent has not yet been reported on.
    refresh_pending: bool,
    /// Whether the load this pane sent has not been reported on.
    ///
    /// A load that answers after the reader behind it signed out is ignored: its
    /// answer is about a stream the pane no longer follows, and reporting it would
    /// turn the pane's waiting state into a failure.
    load_pending: bool,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    /// Keeps the pane following the account the settings hold.
    _account: Subscription,
    _countdown_task: Task<()>,
}

impl WidgetView for PentagonPizzaIndexWidget {
    type Config = PentagonPizzaIndexConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.pentagon_pizza_index");
    const DISPLAY_NAME: &'static str = "widget.names.pentagon_pizza_index";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str =
        "The Pentagon Pizza Index, and the readiness level it reports";
    const ICON: Option<&'static str> = Some("widgets/pentagon_pizza_index.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        // The index is read by the Kytrix service and handed to this pane over the
        // connector's own authenticated connection, so the route it takes is the one
        // the settings give the capability and this widget opens no socket of its
        // own.
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(config, source, status, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let account = account_of(&cx.global::<ConfigManager>().read());
        // A retry keeps what is on screen only for the account that is showing it:
        // an account the settings replaced while the pane was failed is another
        // scope, and the reading belongs to the one that left.
        let retry = self.status.is_failed(cx) && self.account_id == account;
        self.account_id = account;
        self.start(retry, cx);
    }

    /// The reading is the pane's whole content, and once one has arrived the pane
    /// has something to draw whatever that reading says.
    ///
    /// It is the reading alone rather than the revision beside it: a retry drops the
    /// revision its new stream does not share, and a pane that answered "no content"
    /// for that would blank the reading it deliberately kept.
    fn has_content(&self) -> bool {
        self.reading.is_some()
    }

    /// The refresh control on the right, and the countdown it carries.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().right(self.refresh_button(cx))
    }
}

impl PentagonPizzaIndexWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        _config: PentagonPizzaIndexConfig,
        source: S,
        status: Entity<WidgetStatus>,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: PizzaSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            PizzaService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let countdown_task = cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.tick_countdown(cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(COUNTDOWN_TICK).await;
            }
        });
        // The account is the pane's scope, and the user can change it while the
        // pane is open: signing out has to take the reading with it, and signing in
        // has to fetch. Following the account event is what makes that happen
        // without the pane having to be reopened.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });

        Self {
            binding,
            status,
            recovery,
            reading: None,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown: None,
            refresh_pending: false,
            load_pending: false,
            // The pane records the account it was built for, but it does not start
            // anything: the framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            _account: account,
            _countdown_task: countdown_task,
        }
    }

    /// Messages are reported through the binding, which a test drives by hand: it
    /// is cancelled first so the service behind it cannot answer at the same time.
    #[cfg(test)]
    pub(crate) fn cancel_binding(&self) {
        self.binding.cancel();
    }

    /// Whether the reading on screen is showing `revision`.
    #[cfg(test)]
    pub(crate) const fn accepted_revision(&self) -> Option<u64> {
        self.accepted
    }

    /// The account this pane would accept an answer from.
    fn current_key(&self) -> Option<PizzaStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| PizzaStreamKey { account_id })
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

    /// Starts reading, or restarts for a reading the user is retrying.
    ///
    /// A retry keeps the reading the user is still looking at — blanking it to
    /// reconnect would flash the answer away — while an explicit load starts over,
    /// because the pane cannot vouch for what it was showing.
    ///
    /// The revision is dropped either way. It numbers the readings of **one**
    /// generation, and a load replaces the generation: the stream that answers next
    /// counts from one again, so a revision kept from the one it replaced would
    /// refuse every reading the new one publishes until it happened to catch up.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        self.accepted = None;
        // The command that was in flight belongs to the generation this call
        // replaces, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        self.load_pending = false;
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for. The
            // stream the pane was reading belongs to the account that left, so it is
            // stopped rather than left polling for an answer nobody can draw.
            self.reading = None;
            self.status.report(
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
                cx,
            );
            // The answer to a stop is nothing the pane reports: it is already
            // waiting for the account that would start the next read.
            self.binding.call(PizzaRequest::Unload, cx, |_, _, _| {});
            cx.notify();
            return;
        };
        if !retry {
            self.reading = None;
        }
        self.status.report(WidgetState::Connecting, cx);
        self.load_pending = true;
        self.binding
            .call(PizzaRequest::Load(key), cx, Self::apply_load_result);
        cx.notify();
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<PizzaProtocol>,
        cx: &mut Context<Self>,
    ) {
        if !self.load_pending {
            return;
        }
        self.load_pending = false;
        if let Err(error) = result {
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(PizzaFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    fn apply_refresh_result(
        &mut self,
        result: WidgetRpcResult<PizzaProtocol>,
        cx: &mut Context<Self>,
    ) {
        if !self.refresh_pending {
            return;
        }
        // An accepted command stays pending until the round it started is reported:
        // the acknowledgement and the round are two different messages.
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(PizzaFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a status is drawn by the
    /// framework around the body, so publishing it is enough.
    pub(crate) fn apply_reliable(
        &mut self,
        key: PizzaStreamKey,
        message: Arc<PizzaStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let PizzaStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        self.next_refresh_at = status.next_refresh_at();

        match status {
            PizzaStatus::Connecting => {
                self.status.report(WidgetState::Connecting, cx);
            }
            PizzaStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(WidgetState::Loading, cx);
            }
            PizzaStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            PizzaStatus::Failed(error) => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(PizzaFailure::Fetch(error.clone())), recovery),
                    cx,
                );
            }
        }

        false
    }

    /// Applies a reading the stream published.
    ///
    /// Returns whether anything a frame draws changed, which is what the framework
    /// turns into a repaint: the body is cached, and a reading identical to the one
    /// on screen is not worth a frame.
    pub(crate) fn apply_latest(
        &mut self,
        key: PizzaStreamKey,
        message: Arc<PizzaStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let PizzaStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        // A reading the pane has already overtaken would only make the body jump
        // backwards: within one generation the revision only moves forwards, so a
        // smaller one can only be a reading a newer one has replaced.
        if self.accepted.is_some_and(|held| snapshot.revision() < held) {
            return false;
        }
        let changed = self.reading.as_deref() != Some(&snapshot.reading());
        self.reading = Some(Arc::new(snapshot.reading()));
        self.accepted = Some(snapshot.revision());
        // Readiness is published at the protocol's own boundary: a complete reading
        // is not the same thing as a reading a status said was complete, and this is
        // where the two meet.
        if self.ready.is_some_and(|ready| snapshot.revision() >= ready) {
            self.status.report(WidgetState::Ready, cx);
        }
        changed
    }

    /// The deadline this pane counts down to, if it has one.
    ///
    /// A countdown belongs to a ready pane: it counts down the time left until the
    /// next automatic read of the reading that is on screen. While a read is in
    /// flight there is nothing to count down to — the deadline the service
    /// announced belongs to the read after the one being waited for.
    fn countdown_deadline(&self, cx: &impl AppContext) -> Option<i64> {
        let status = self.status.state(cx);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the countdown is: a pane that is connecting, fetching
    /// or failed has nothing to bring forward — the first two are already waiting on
    /// data, and a failure is a state the framework offers its own recovery over.
    fn refresh_is_disabled(&self, cx: &impl AppContext) -> bool {
        self.refresh_pending || self.countdown_deadline(cx).is_none()
    }

    /// The control that reads the index again, and the time left until the next
    /// automatic read.
    fn refresh_button(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.refresh_is_disabled(cx);
        let widget = cx.entity();
        let button = Button::new(ElementId::Name(SharedString::new_static(
            "PentagonPizzaIndexRefresh",
        )))
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(t!(cx, "widget.pentagon_pizza_index.refresh_now"))
        .child(Icon::new(IconKind::ArrowsClockwise).size_3())
        .children(self.countdown.clone());
        // A disabled button in this workspace is only *drawn* disabled, so the
        // handler is not attached at all while there is nothing to refresh.
        let button = if disabled {
            button
        } else {
            button.on_click(move |_, _, app| {
                widget.update(app, |this, cx| this.refresh(cx));
            })
        };

        div()
            .debug_selector(|| "TEST_PENTAGON_PIZZA_INDEX_REFRESH".into())
            .flex_none()
            .occlude()
            .child(button)
            .into_any_element()
    }

    /// Reads the index again without waiting for the next interval.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_is_disabled(cx) {
            return;
        }
        self.refresh_pending = true;
        self.binding
            .call(PizzaRequest::Refresh, cx, Self::apply_refresh_result);
        cx.notify();
    }

    /// Rebuilds the countdown, asking for the frame that draws it.
    ///
    /// The countdown is the only thing this pane draws that moves on its own, and
    /// it moves once a second: the task wakes four times a second so a second is
    /// never missed, and only the write that changed it asks for a frame. The header
    /// is not cached, so nothing else here has to announce itself.
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
}

impl Render for PentagonPizzaIndexWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the ready
        // state and nothing else. Everything the pane is *doing* is drawn by the
        // framework around this.
        let Some(reading) = self.reading.clone() else {
            return EmptyState::new("PentagonPizzaIndexEmpty", t!(cx, "common.no_data"))
                .into_any_element();
        };
        let level = reading.level();
        let theme = cx.theme();
        let status_role = level_role(theme, level);
        let zone = data_time_zone(cx);

        // The level is the pane's one big statement, so it is drawn at the size of a
        // heading and coloured by its rung; the number the index publishes sits beside
        // it, because that is what the index's own page reports and what a reader
        // comparing the two will look for.
        div()
            .id(ElementId::Name(SharedString::new_static(
                "PentagonPizzaIndexBody",
            )))
            .debug_selector(|| "TEST_PENTAGON_PIZZA_INDEX".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .child(
                div()
                    .debug_selector(|| "TEST_PENTAGON_PIZZA_INDEX_LEVEL".into())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .debug_selector(|| "TEST_PENTAGON_PIZZA_INDEX_NUMBER".into())
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(status_role)
                            .child(t!(
                                cx,
                                "widget.pentagon_pizza_index.level_number",
                                level = level.number()
                            )),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(status_role)
                            .child(level.name(cx)),
                    ),
            )
            .child(
                // The bar is the rung's colour written across the pane: a reader who
                // has scrolled past everything else still sees which rung it is.
                div()
                    .debug_selector(|| "TEST_PENTAGON_PIZZA_INDEX_BAR".into())
                    .w_full()
                    .max_w(px(220.0))
                    .h(px(LEVEL_BAR_HEIGHT))
                    .rounded_full()
                    .bg(status_role),
            )
            .child(
                div()
                    .debug_selector(|| "TEST_PENTAGON_PIZZA_INDEX_OBSERVED".into())
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "widget.pentagon_pizza_index.observed_at",
                        time = format_observed_at(reading.observed_at(), zone)
                    )),
            )
            .into_any_element()
    }
}

/// The colour one rung is drawn in.
///
/// The ladder is a severity, so it reads the status roles rather than a palette of
/// its own: that is what keeps two appearances legible and keeps a red a reader has
/// learned from an error banner meaning the same thing here.
fn level_role(theme: &base_ui::style::theme::Theme, level: PizzaLevel) -> gpui_kit::Hsla {
    match level {
        PizzaLevel::FadeOut => theme.roles.status.success.base.hsla,
        PizzaLevel::DoubleTake => theme.roles.status.info.base.hsla,
        PizzaLevel::RoundHouse => theme.roles.status.warning.base.hsla,
        PizzaLevel::FastPace => theme.roles.status.elevated.base.hsla,
        PizzaLevel::ExerciseTerm => theme.roles.status.danger.base.hsla,
    }
}

/// The moment a reading was observed at, read in the display zone.
///
/// It is formatted on the frame that draws it rather than when the reading arrived:
/// the display zone can move while the pane is open, and a clock formatted once is a
/// clock frozen into the snapshot.
fn format_observed_at(observed_at: i64, zone: UtcOffset) -> SharedString {
    // The offset can move an instant past the last date the calendar holds, and an
    // instant the index could not have published is written as a placeholder rather
    // than taken to a date that is not one.
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(observed_at) * 1_000_000)
        .ok()
        .and_then(|time| time.checked_to_offset(zone))
        .map_or_else(
            || SharedString::new_static("--"),
            |time| {
                SharedString::new(
                    smol_str::format_smolstr!(
                        "{:04}-{:02}-{:02} {:02}:{:02}",
                        time.year(),
                        u8::from(time.month()),
                        time.day(),
                        time.hour(),
                        time.minute()
                    )
                    .as_str(),
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{App, BorrowAppContext, TestAppContext};
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use widget::{protocol::RequestId, status::WidgetState};
    use widget_tokio_runtime::global;

    use super::*;
    use crate::{
        model::{PizzaDataError, PizzaLevel},
        service::PizzaSnapshot,
        test_support::{ACCOUNT, FakeSource, OBSERVED_AT, reading},
    };

    /// Settings that hold `account` as the signed-in account, or hold nobody.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

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

    /// The pane a test drives, built the way the framework builds one.
    ///
    /// It is deliberately *not* loaded: these tests are the only thing that reports
    /// to the pane, and a service running its own rounds on the application runtime
    /// would race them on another thread. The binding the construction still holds
    /// is cancelled, so nothing else can answer either.
    fn pane_with(
        cx: &mut TestAppContext,
        account: Option<&str>,
    ) -> Entity<PentagonPizzaIndexWidget> {
        // The countdown task this pane spawns runs on the test's executor and wakes on a
        // timer, and the service's own runtime is a Tokio one: both are work the test does
        // not drive tick by tick, so the executor is told it may park while they settle.
        // Without it the scheduler reports activity from a thread it is not running on
        // and fails the test as non-deterministic.
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx);
            sign_in(cx, account);

            let status = cx.new(|_| WidgetStatus::new());
            let widget = cx.new(move |cx| {
                let weak: gpui_kit::WeakEntity<PentagonPizzaIndexWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                PentagonPizzaIndexWidget::new_with_source(
                    PentagonPizzaIndexConfig,
                    FakeSource::default(),
                    status,
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, _| widget.cancel_binding());

            widget
        })
    }

    /// A pane scoped to the fixture account, holding nothing it was not told.
    fn pane(cx: &mut TestAppContext) -> Entity<PentagonPizzaIndexWidget> {
        pane_with(cx, Some(ACCOUNT))
    }

    fn key() -> PizzaStreamKey {
        PizzaStreamKey {
            account_id: SmolStr::new_static(ACCOUNT),
        }
    }

    fn other_key() -> PizzaStreamKey {
        PizzaStreamKey {
            account_id: SmolStr::new_static("second-account"),
        }
    }

    /// Reports a status as the stream would.
    ///
    /// The pane's binding was cancelled where it was built, so the reported messages
    /// are its only input: the load it was still holding was dropped with the
    /// runtime, and nothing else can answer.
    fn report_status(
        pane: &Entity<PentagonPizzaIndexWidget>,
        status: PizzaStatus,
        cx: &mut App,
    ) -> bool {
        report_from(pane, key(), PizzaStreamMessage::Status(status), cx)
    }

    /// Reports a status the stream addressed to `key`.
    fn report_from(
        pane: &Entity<PentagonPizzaIndexWidget>,
        key: PizzaStreamKey,
        message: PizzaStreamMessage,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| match message {
            PizzaStreamMessage::Status(_) => pane.apply_reliable(key, Arc::new(message), context),
            PizzaStreamMessage::Snapshot(_) => pane.apply_latest(key, Arc::new(message), context),
        })
    }

    /// Reports a reading as the stream would, at `revision`.
    fn report_reading(
        pane: &Entity<PentagonPizzaIndexWidget>,
        revision: u64,
        reading: PizzaReading,
        cx: &mut App,
    ) -> bool {
        report_from(
            pane,
            key(),
            PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(revision, reading))),
            cx,
        )
    }

    /// The state the pane is reporting.
    fn reported(pane: &Entity<PentagonPizzaIndexWidget>, cx: &App) -> WidgetState {
        pane.read(cx).status.state(cx)
    }

    /// The reading the pane holds, by level.
    fn level_of(pane: &Entity<PentagonPizzaIndexWidget>, cx: &App) -> Option<PizzaLevel> {
        pane.read(cx)
            .reading
            .as_ref()
            .map(|reading| reading.level())
    }

    /// A ready pane is content once a reading is published for it, and the reading is
    /// what it draws.
    #[gpui_kit::test]
    fn a_reading_should_be_the_panes_content(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 1,
                    next_refresh_at: 0,
                },
                cx,
            );
            report_reading(&pane, 1, reading(3), cx);

            assert!(pane.read(cx).has_content());
            assert_eq!(reported(&pane, cx), WidgetState::Ready);
        });
        cx.read(|cx| assert_eq!(level_of(&pane, cx), Some(PizzaLevel::RoundHouse)));
    }

    /// A failure is reported as a failure, holds no reading, and leaves the pane with
    /// nothing the framework could draw as content.
    #[gpui_kit::test]
    fn a_failed_round_should_report_a_failure_and_hold_no_reading(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Failed(PizzaFetchError::Data(PizzaDataError::UnknownLevel {
                    level: 9,
                })),
                cx,
            );

            assert!(pane.read(cx).status.is_failed(cx));
            assert!(
                !pane.read(cx).has_content(),
                "a failed read is not content: the framework draws the failure placeholder"
            );
            assert!(pane.read(cx).reading.is_none());
        });
    }

    /// A message about a scope this pane is not following is not one it may act on,
    /// and a pane that has failed is finished until the user recovers it.
    #[gpui_kit::test]
    fn a_late_message_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Closed)),
                cx,
            );
            report_reading(&pane, 1, reading(1), cx);

            assert!(pane.read(cx).status.is_failed(cx));
            assert!(
                pane.read(cx).reading.is_none(),
                "a reading that arrives after a failure is about a stream that is gone"
            );
        });
    }

    /// Another account is another scope: an answer served to the one that just left
    /// is not one this pane may draw.
    #[gpui_kit::test]
    fn a_message_about_another_account_should_not_be_drawn(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            // The pane's own account is accepted first, so the assertion below is
            // about the key rather than about a pane that never took anything.
            assert!(report_reading(&pane, 1, reading(3), cx));
            assert_eq!(level_of(&pane, cx), Some(PizzaLevel::RoundHouse));

            report_from(
                &pane,
                other_key(),
                PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(2, reading(1)))),
                cx,
            );

            assert_eq!(
                level_of(&pane, cx),
                Some(PizzaLevel::RoundHouse),
                "the reading served to another account did not replace the one on screen"
            );
            assert_eq!(
                pane.read(cx).accepted_revision(),
                Some(1),
                "and it did not take the revision with it either"
            );
        });
    }

    /// A reading the pane has already overtaken is not one it may draw: within one
    /// generation the revision only moves forwards.
    #[gpui_kit::test]
    fn an_older_revision_should_not_replace_the_reading_on_screen(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 2,
                    next_refresh_at: 0,
                },
                cx,
            );
            report_reading(&pane, 2, reading(1), cx);
            report_reading(&pane, 1, reading(5), cx);

            assert_eq!(
                pane.read(cx).accepted_revision(),
                Some(2),
                "the reading on screen is still revision two"
            );
            assert_eq!(
                pane.read(cx)
                    .reading
                    .as_ref()
                    .map(|reading| reading.level()),
                Some(PizzaLevel::ExerciseTerm),
                "the older reading did not replace the one on screen"
            );
        });
    }

    /// A retry keeps the reading the user is looking at but not the revision it was
    /// counted at: a load replaces the stream, and the one that replaces it numbers
    /// its readings from one again.
    ///
    /// A revision kept across that boundary would refuse every reading the new
    /// stream publishes until its count caught up — the pane would keep a failed
    /// hour's level on screen and report readiness over it.
    #[gpui_kit::test]
    fn a_retry_should_keep_the_reading_and_drop_its_revision(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_reading(&pane, 2, reading(1), cx);
            report_status(
                &pane,
                PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Closed)),
                cx,
            );
            pane.update(cx, |pane, cx| pane.start(true, cx));

            assert_eq!(
                pane.read(cx).accepted_revision(),
                None,
                "the revision numbers one stream, and this is another one"
            );
            assert_eq!(
                level_of(&pane, cx),
                Some(PizzaLevel::ExerciseTerm),
                "the reading the user was looking at is still there"
            );
            assert!(
                pane.read(cx).has_content(),
                "a pane that kept its reading still has something to draw"
            );

            assert!(
                report_reading(&pane, 1, reading(5), cx),
                "the first reading of the new stream is a change"
            );
            assert_eq!(
                level_of(&pane, cx),
                Some(PizzaLevel::FadeOut),
                "and it is what the pane now draws"
            );
        });
    }

    /// A loading reader who signs out is waiting for an account, not for the load it
    /// no longer follows: the answer belongs to the stream that left with the
    /// account, and reporting it as a failure would hide the account request.
    #[gpui_kit::test]
    fn a_load_answer_for_a_reader_that_signed_out_should_not_fail_the_pane(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx);

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.load(cx);
                pane.reconcile_account(None, cx);
                pane.apply_load_result(
                    Err(RpcCallError::ServiceClosed {
                        request_id: RequestId::new(1),
                    }),
                    cx,
                );
            });

            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
                "the load answered for the account that left"
            );
        });
    }

    /// The same for a refresh: the command belonged to the stream the sign-out
    /// replaced, so its answer is not one this pane may report.
    #[gpui_kit::test]
    fn a_refresh_answer_for_a_reader_that_signed_out_should_not_fail_the_pane(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx);
        let due = unix_millis().saturating_add(600_000);

        cx.update(|cx| {
            report_reading(&pane, 1, reading(3), cx);
            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 1,
                    next_refresh_at: due,
                },
                cx,
            );
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(pane.refresh_pending, "the click puts a refresh in flight");
                pane.reconcile_account(None, cx);
                pane.apply_refresh_result(
                    Err(RpcCallError::ServiceClosed {
                        request_id: RequestId::new(2),
                    }),
                    cx,
                );
            });

            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
                "the refresh answered for the account that left"
            );
        });
    }

    /// A reader the service does not know is told what it is waiting for rather than
    /// being sent after an answer nobody will serve it.
    #[gpui_kit::test]
    fn a_signed_out_load_should_wait_for_an_account(cx: &mut TestAppContext) {
        let pane = pane_with(cx, None);

        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.load(cx));

            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out"))
            );
            assert!(!pane.read(cx).has_content());
            assert!(pane.read(cx).current_key().is_none());
        });
    }

    /// A reading identical to the one on screen is not worth a frame, which is what
    /// keeps a quiet hour from scheduling a repaint.
    #[gpui_kit::test]
    fn a_reading_that_did_not_change_should_not_ask_for_a_frame(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 1,
                    next_refresh_at: 0,
                },
                cx,
            );
            assert!(report_reading(&pane, 1, reading(3), cx));
            assert!(
                !report_reading(&pane, 1, reading(3), cx),
                "the same reading twice is one change"
            );
        });
    }

    /// Readiness is published at the protocol's own boundary: a reading that arrived
    /// before the status describing it leaves the pane loading, and the status then
    /// makes it ready.
    #[gpui_kit::test]
    fn the_two_channels_should_meet_at_the_readings_revision(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_reading(&pane, 1, reading(2), cx);
            assert_eq!(
                reported(&pane, cx),
                WidgetState::Connecting,
                "a reading without its status is a pane that is still connecting"
            );

            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 1,
                    next_refresh_at: 0,
                },
                cx,
            );
            assert_eq!(reported(&pane, cx), WidgetState::Ready);
        });
    }

    /// The countdown belongs to a ready pane: it counts down the time left until the
    /// next automatic read of the reading on screen.
    #[gpui_kit::test]
    fn the_countdown_should_belong_to_a_ready_pane(cx: &mut TestAppContext) {
        let pane = pane(cx);
        let due = unix_millis().saturating_add(600_000);

        cx.update(|cx| {
            report_reading(&pane, 1, reading(2), cx);
            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 1,
                    next_refresh_at: due,
                },
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert!(
                pane.read(cx).countdown.is_some(),
                "a ready pane counts down to its next read"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));

            // A pane that fails has nothing to bring forward.
            report_status(
                &pane,
                PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Timeout)),
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(
                pane.read(cx).countdown.is_none(),
                "a failed pane is not counting down to anything"
            );
            assert!(pane.read(cx).refresh_is_disabled(cx));
        });
    }

    /// Signing out takes the reading with it — and the revision, the deadline and the
    /// countdown counted against it — and asks for an account rather than reporting a
    /// failure: nothing has gone wrong, the pane is just not for an anonymous reader.
    #[gpui_kit::test]
    fn signing_out_should_take_the_reading_with_it(cx: &mut TestAppContext) {
        let pane = pane(cx);
        let due = unix_millis().saturating_add(600_000);

        cx.update(|cx| {
            report_reading(&pane, 1, reading(3), cx);
            report_status(
                &pane,
                PizzaStatus::Ready {
                    revision: 1,
                    next_refresh_at: due,
                },
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(pane.read(cx).has_content());

            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            assert!(!pane.read(cx).has_content());
            assert!(
                pane.read(cx).reading.is_none(),
                "the reading was served to an account that just left"
            );
            assert_eq!(pane.read(cx).accepted_revision(), None);
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
                "a signed-out pane waits for an account"
            );
            assert!(pane.read(cx).current_key().is_none());
            assert!(
                pane.read(cx).countdown.is_none(),
                "a pane waiting for an account is not counting down to a read"
            );

            // The task that rebuilds the label must not revive it out of a deadline
            // the sign-out was supposed to drop.
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(pane.read(cx).countdown.is_none());
        });
    }

    /// Another account is another read, and the reading of the one that left goes
    /// with it.
    #[gpui_kit::test]
    fn another_account_should_take_the_reading_with_it(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_reading(&pane, 1, reading(3), cx);
            assert!(pane.read(cx).has_content());

            pane.update(cx, |pane, cx| {
                pane.reconcile_account(Some(SmolStr::new_static("second-account")), cx);
            });

            assert!(
                !pane.read(cx).has_content(),
                "the reading was served to the account that just left"
            );
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
            assert_eq!(
                pane.read(cx).current_key(),
                Some(other_key()),
                "the pane follows the account the settings now hold"
            );
        });
    }

    /// The pane follows the account through the settings event, not only through a
    /// reload: a reader who signs in is re-scoped, and one who signs out is asked for
    /// an account rather than left with the reading of the account that left.
    ///
    /// The fixture's binding is cancelled, so the load the new account starts is
    /// answered by the closed service instead of by a source. That answer is not what
    /// this test is about: the scope the pane took from the event is.
    #[gpui_kit::test]
    fn an_account_change_should_re_scope_the_pane(cx: &mut TestAppContext) {
        let pane = pane(cx);

        cx.update(|cx| {
            report_reading(&pane, 1, reading(3), cx);
            assert!(pane.read(cx).has_content());
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| *config = settings(Some("second-account")), cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).account_id.as_deref(),
                Some("second-account"),
                "the pane hears the account change from the settings"
            );
            assert!(
                !pane.read(cx).has_content(),
                "the reading was served to the account that left"
            );
            assert_eq!(pane.read(cx).current_key(), Some(other_key()));

            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| *config = settings(None), cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
                "signing out asks for an account"
            );
        });
    }

    /// Each rung is drawn in its own colour: a scale that shared a colour between two
    /// rungs would make two different readings look the same.
    #[gpui_kit::test]
    fn every_rung_should_be_drawn_in_its_own_colour(cx: &mut TestAppContext) {
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            let theme = cx.theme();
            let colours = [
                PizzaLevel::FadeOut,
                PizzaLevel::DoubleTake,
                PizzaLevel::RoundHouse,
                PizzaLevel::FastPace,
                PizzaLevel::ExerciseTerm,
            ]
            .map(|level| level_role(theme, level));

            for (index, colour) in colours.iter().enumerate() {
                assert!(
                    !colours[..index].contains(colour),
                    "two rungs share a colour, so two readings would look the same"
                );
            }
        });
    }

    /// The message keys this pane draws are all named in both locales, which is what
    /// keeps a missing translation from being a blank line rather than a failure.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for language in [
                i18n::language::codes::LanguageCode::En,
                i18n::language::codes::LanguageCode::ZhCn,
            ] {
                cx.set_global(language);

                for level in [
                    PizzaLevel::FadeOut,
                    PizzaLevel::DoubleTake,
                    PizzaLevel::RoundHouse,
                    PizzaLevel::FastPace,
                    PizzaLevel::ExerciseTerm,
                ] {
                    assert!(
                        !level.name(cx).is_empty(),
                        "{} has no copy in {language}",
                        level.name_key()
                    );
                }
                for key in [
                    "widget.names.pentagon_pizza_index",
                    "widget.pentagon_pizza_index.refresh_now",
                    "widget.pentagon_pizza_index.observed_at",
                    "widget.pentagon_pizza_index.level_number",
                    "widget.pentagon_pizza_index.logged_out",
                ] {
                    assert!(!t!(cx, key).is_empty(), "{key} has no copy in {language}");
                }
            }
        });
    }

    /// Every failure this pane can report resolves to a sentence in both locales: a
    /// failure is the one thing a reader has to act on, so a blank one is the worst
    /// place for a missing key.
    #[gpui_kit::test]
    fn every_error_this_pane_reports_should_resolve(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for language in [
                i18n::language::codes::LanguageCode::En,
                i18n::language::codes::LanguageCode::ZhCn,
            ] {
                cx.set_global(language);

                // One of each failure rather than one per value: every variant is a
                // key of its own, and a level that renders is the same key whatever
                // number it names.
                for error in [
                    PizzaDataError::MissingObservation,
                    PizzaDataError::UnknownLevel { level: 9 },
                ] {
                    assert!(
                        !error.localized_message(cx).is_empty(),
                        "{error:?} renders empty in {language}"
                    );
                }
                for error in [
                    PizzaServiceError::StreamClosed,
                    PizzaServiceError::StreamBusy,
                ] {
                    assert!(
                        !error.localized_message(cx).is_empty(),
                        "{error:?} renders empty in {language}"
                    );
                }
                // The failures the connector answers with: a closed connection, a
                // deadline, and the two an answer of the wrong shape produces.
                for error in [
                    KytrixServiceError::Closed,
                    KytrixServiceError::Timeout,
                    KytrixServiceError::Authentication("expired".into()),
                    KytrixServiceError::Protocol("missing data".into()),
                ] {
                    assert!(
                        !PizzaFetchError::Service(error.clone())
                            .localized_message(cx)
                            .is_empty(),
                        "{error:?} renders empty in {language}"
                    );
                }
                assert!(
                    !PizzaFetchError::Data(PizzaDataError::MissingObservation)
                        .localized_message(cx)
                        .is_empty(),
                    "a data failure renders empty in {language}"
                );
            }
        });
    }

    /// A reading draws the moment it was observed at, read in the display zone rather
    /// than in UTC: the clock is resolved on the frame, so the zone can move.
    #[test]
    fn a_reading_should_draw_the_moment_it_was_observed_at() {
        assert_eq!(
            format_observed_at(OBSERVED_AT, UtcOffset::UTC).as_ref(),
            "2026-10-24 06:00",
            "the fixture reading read as a wall clock in UTC"
        );
        assert_eq!(
            format_observed_at(OBSERVED_AT, UtcOffset::from_hms(8, 0, 0).unwrap()).as_ref(),
            "2026-10-24 14:00",
            "the display zone moves the clock without moving the instant"
        );
        assert_eq!(
            format_observed_at(i64::MAX, UtcOffset::UTC).as_ref(),
            "--",
            "an instant past the calendar is a placeholder, not a made-up date"
        );
    }
}
