//! The Pentagon Pizza Index pane: the reading it draws, the ladder it colours it
//! with, and the refresh it counts down to.
//!
//! # What the pane is scoped to
//!
//! Nothing. The feed is global — no account, no venue, no selected instrument — so
//! the pane has one scope for every reader, and its persisted config carries no
//! options at all. What a pane shows is what the feed answered last.
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
//! bring it forward — so they are one control. The feed publishes one point an
//! hour, so the countdown is the only thing on screen that moves on its own.

use std::{sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
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
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task,
    Window, base::Disableable, div, px,
};
use i18n::t;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::data_time_zone,
    util::{format_countdown, unix_millis},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    model::{PizzaLevel, PizzaReading},
    service::{
        PizzaFetchError, PizzaProtocol, PizzaRequest, PizzaScope, PizzaService, PizzaServiceError,
        PizzaSource, PizzaStatus, PizzaStreamMessage, UrlSource,
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

/// The pane persists nothing: the feed is the same feed for every reader.
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

/// One pane: the reading it holds, and the countdown to the next one.
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
    _countdown_task: Task<()>,
}

impl WidgetView for PentagonPizzaIndexWidget {
    type Config = PentagonPizzaIndexConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.monitoring.pentagon_pizza_index");
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
        // The feed is read by the pane rather than by a service of the application's:
        // it is a public document on someone else's host, so the transport is the
        // pane's own and the route it takes is the one the settings give the
        // capability. The feed is served over HTTPS on a public origin, and nothing
        // this pane reads is ever plaintext, so the transport refuses plaintext
        // whatever the build is for.
        let connector =
            cx.global::<kytrix_service_provider::service::connector::KytrixServiceConnector>();
        let transport = Arc::new(network::transport::hub::TransportHub::new(
            network::transport::hub::TransportPolicy::default(),
        ));
        let runtime = global::handle(cx);
        let source = UrlSource::live(transport, connector.network_notification(), runtime);
        Self::new_with_source(config, source, status, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let retry = self.status.is_failed(cx);
        self.start(retry, cx);
    }

    /// The reading is the pane's whole content, and once one has arrived the pane
    /// has something to draw whatever that reading says.
    fn has_content(&self) -> bool {
        self.accepted.is_some() && self.reading.is_some()
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

    /// Starts reading, or restarts for a reading the user is retrying.
    ///
    /// A retry keeps the reading the user is still looking at — blanking it to
    /// reconnect would flash the answer away — while an explicit load starts over,
    /// because the pane cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        // The command that was in flight belongs to the generation this call
        // replaces, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        if !retry {
            self.accepted = None;
            self.reading = None;
        }
        self.status.report(WidgetState::Connecting, cx);
        self.binding
            .call(PizzaRequest::Load, cx, Self::apply_load_result);
        cx.notify();
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<PizzaProtocol>,
        cx: &mut Context<Self>,
    ) {
        self.refresh_pending = false;
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
        key: PizzaScope,
        message: Arc<PizzaStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if key != PizzaScope || self.status.is_failed(cx) {
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
        key: PizzaScope,
        message: Arc<PizzaStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if key != PizzaScope || self.status.is_failed(cx) {
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

    /// The control that reads the feed again, and the time left until the next
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

    /// Reads the feed again without waiting for the next interval.
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
        // heading and coloured by its rung; the number the feed publishes sits beside
        // it, because that is what the feed's own page reports and what a reader
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
    // instant the feed could not have published is written as a placeholder rather
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
    use app_config::settings::Config;
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{App, TestAppContext};
    use widget::{protocol::RequestId, status::WidgetState};
    use widget_tokio_runtime::global;

    use super::*;
    use crate::{
        model::PizzaLevel,
        service::PizzaSnapshot,
        test_support::{FakeSource, OBSERVED_AT, TIMESTAMP, reading},
    };

    /// The pane a test drives, built the way the framework builds one.
    ///
    /// It is deliberately *not* loaded: these tests are the only thing that reports
    /// to the pane, and a service running its own rounds on the application runtime
    /// would race them on another thread. The binding the construction still holds
    /// is cancelled, so nothing else can answer either.
    fn pane(source: FakeSource, cx: &mut TestAppContext) -> Entity<PentagonPizzaIndexWidget> {
        // The countdown task this pane spawns runs on the test's executor and wakes on a
        // timer, and the service's own runtime is a Tokio one: both are work the test does
        // not drive tick by tick, so the executor is told it may park while they settle.
        // Without it the scheduler reports activity from a thread it is not running on
        // and fails the test as non-deterministic.
        cx.executor().allow_parking();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);

            let status = cx.new(|_| WidgetStatus::new());
            let widget = cx.new(move |cx| {
                let weak: gpui_kit::WeakEntity<PentagonPizzaIndexWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                PentagonPizzaIndexWidget::new_with_source(
                    PentagonPizzaIndexConfig,
                    source,
                    status,
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, _| widget.cancel_binding());

            widget
        })
    }

    /// A fake that never answers, so what the pane holds is only what a test reports.
    fn pending(cx: &mut TestAppContext) -> Entity<PentagonPizzaIndexWidget> {
        pane(FakeSource::default(), cx)
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
        pane.update(cx, |pane, context| {
            pane.apply_reliable(
                PizzaScope,
                Arc::new(PizzaStreamMessage::Status(status)),
                context,
            )
        })
    }

    /// Reports a reading as the stream would, at `revision`.
    fn report_reading(
        pane: &Entity<PentagonPizzaIndexWidget>,
        revision: u64,
        reading: PizzaReading,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| {
            pane.apply_latest(
                PizzaScope,
                Arc::new(PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(
                    revision, reading,
                )))),
                context,
            )
        })
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
        let pane = pending(cx);

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
        let pane = pending(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Failed(PizzaFetchError::Data(
                    crate::model::PizzaDataError::Unreadable,
                )),
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
        let pane = pending(cx);

        cx.update(|cx| {
            report_status(
                &pane,
                PizzaStatus::Failed(PizzaFetchError::Data(crate::model::PizzaDataError::Refused)),
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

    /// A reading the pane has already overtaken is not one it may draw: within one
    /// generation the revision only moves forwards.
    #[gpui_kit::test]
    fn an_older_revision_should_not_replace_the_reading_on_screen(cx: &mut TestAppContext) {
        let pane = pending(cx);

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

    /// A reading identical to the one on screen is not worth a frame, which is what
    /// keeps a quiet hour from scheduling a repaint.
    #[gpui_kit::test]
    fn a_reading_that_did_not_change_should_not_ask_for_a_frame(cx: &mut TestAppContext) {
        let pane = pending(cx);

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
        let pane = pending(cx);

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
        let pane = pending(cx);
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
                PizzaStatus::Failed(PizzaFetchError::Timeout(Duration::from_secs(15))),
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
        use crate::{model::PizzaDataError, service::PizzaServiceError};

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for language in [
                i18n::language::codes::LanguageCode::En,
                i18n::language::codes::LanguageCode::ZhCn,
            ] {
                cx.set_global(language);

                for error in [
                    PizzaDataError::Refused,
                    PizzaDataError::MissingPoint,
                    PizzaDataError::UnknownLevel { level: 9 },
                    PizzaDataError::Timestamp,
                    PizzaDataError::Unreadable,
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
                for error in [
                    PizzaFetchError::Timeout(Duration::from_secs(15)),
                    PizzaFetchError::Data(PizzaDataError::Refused),
                ] {
                    assert!(
                        !error.localized_message(cx).is_empty(),
                        "{error:?} renders empty in {language}"
                    );
                }
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
            "{TIMESTAMP} read as a wall clock in UTC"
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

    /// A request id nobody asked for is not one the pane may act on, which is what
    /// the binding's own filtering is for; this pins the type the pane reports with.
    #[test]
    fn a_request_id_should_identify_one_call() {
        assert_eq!(RequestId::new(7).get(), 7);
    }
}
