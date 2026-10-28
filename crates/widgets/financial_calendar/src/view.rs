//! The financial calendar pane: the events it draws, and the schedule it keeps.
//!
//! # What this pane is
//!
//! A calendar is a whole answer rather than a stream: the service reads all of it,
//! every ten minutes, and one read replaces the last one entirely. So the pane never
//! asks for a part of the calendar — it asks for *the* calendar, and everything on
//! screen is a view of the one answer it holds. A read that answers with a different
//! calendar rebuilds the table from nothing rather than patching the rows that
//! moved: a revision can add an event in the middle of a day that is already drawn,
//! and a row-by-row merge would have to decide what "the same event" means.
//!
//! # What moves without an answer
//!
//! Two things, and neither of them is data: the day a row belongs to (which follows
//! the zone the settings display data in) and the header's countdown (which counts
//! down to the next read). The zone is read when the answer is grouped and again on
//! the frame that draws a time, so moving it regroups the rows and redraws the clock
//! without asking the service for anything.
//!
//! # What it does with a failure
//!
//! A refused read is a failure the user sees: the pane reports it through the
//! framework, which puts a banner over the calendar that is still on screen. The
//! service stops its schedule at that point, so the banner's recovery is what starts
//! it again — a service that is briefly unreachable does not turn into a request
//! every ten minutes until someone notices.
//!
//! # Who may see it
//!
//! The service serves this data to an account, so a pane with no account behind it
//! reads nothing and says so. That is a *waiting* state rather than a failure:
//! nothing has gone wrong, the pane is just not for an anonymous reader. Nothing is
//! retried on a timer either — the account event subscription re-scopes the pane the
//! moment the user signs in, and stops the read it was following the moment they sign
//! out.

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
    table::Table,
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    Window, base::Disableable, div, prelude::FluentBuilder,
};
use i18n::t;
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::Date;
use widget::{
    account::account_of,
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::{data_time_zone, on_data_time_zone_change},
    util::{format_countdown, unix_millis},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    model::{CalendarParseError, CalendarSnapshot, Importance},
    service::{
        CalendarFetchError, CalendarProtocol, CalendarRequest, CalendarResponse, CalendarService,
        CalendarServiceError, CalendarSource, CalendarStreamKey, CalendarStreamMessage,
    },
    table::{CalendarTable, day_label_text},
};

/// How often the pane re-reads the clock behind its countdown.
///
/// It is not a read: it only re-checks how long is left. A quarter of a second is
/// faster than the label's own resolution and cheaper than a frame.
pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// The pane has nothing to persist, so its config carries the schema version alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinancialCalendarConfig;

impl WidgetConfig for FinancialCalendarConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CalendarFailure {
    Fetch(CalendarFetchError),
    Rpc(RpcCallError<CalendarServiceError>),
}

impl LocalizedError for CalendarFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for CalendarFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Parse(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for CalendarParseError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InvalidDate { year, month, day } => t!(
                cx,
                "error.financial_calendar.invalid_date",
                year = year,
                month = month,
                day = day
            ),
            Self::InvalidTimeOfDay { seconds } => t!(
                cx,
                "error.financial_calendar.invalid_time_of_day",
                seconds = seconds
            ),
        }
    }
}

impl LocalizedError for CalendarServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::StreamClosed => t!(cx, "error.financial_calendar.stream_unavailable"),
            Self::StreamBusy => t!(cx, "error.financial_calendar.refresh_already_waiting"),
        }
    }
}

/// One financial calendar pane: the account it follows, the table it draws, and its
/// refresh control.
pub struct FinancialCalendarWidget {
    binding: WidgetBinding<CalendarProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    /// The table the answer is drawn as, which is an entity because that is what
    /// [`Table`] reads its rows from.
    table: Entity<CalendarTable>,
    /// The answer this pane accepted, or nothing before the first one arrives.
    snapshot: Option<Arc<CalendarSnapshot>>,
    /// When the next automatic read is due, as the service published it with the
    /// answer it belongs to.
    next_refresh_at: Option<i64>,
    /// The time left until that read, as the refresh control draws it.
    countdown: Option<SharedString>,
    /// Whether a refresh this pane sent has been answered *and its answer reported*.
    ///
    /// It is set by the click and cleared by the answer the read it started
    /// publishes, which is what closes the window between the two: the service
    /// refuses a second read while one is in flight, and the framework shows a
    /// refusal as a failure.
    refresh_pending: bool,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    _countdown_task: Task<()>,
    /// Keeps the pane following the account the settings hold.
    _account: Subscription,
    /// Wakes the pane — and the table — when the zone its days and times are read in
    /// moves.
    _time_zone: Option<Subscription>,
}

impl WidgetView for FinancialCalendarWidget {
    type Config = FinancialCalendarConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.financial_calendar");
    const DISPLAY_NAME: &'static str = "widget.names.financial_calendar";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str = "Scheduled economic events, grouped by the day they fall on";
    const ICON: Option<&'static str> = Some("widgets/financial_calendar.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(config, source, status, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // A retry keeps the calendar the user is still reading — blanking it to
        // reconnect would flash a screen of data away — while an explicit load starts
        // over, because the pane cannot vouch for what it was showing.
        let retry = self.status.is_failed(cx);
        self.account_id = account_of(&cx.global::<ConfigManager>().read());
        self.start(retry, cx);
    }

    /// A pane is content once an answer has arrived for the account it follows.
    ///
    /// Having the last answer is not the same thing: it may have been served to an
    /// account that has since signed out.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The filters that narrow the calendar, and the refresh control with the time
    /// left until the next read on it.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty()
            .left(self.filter_controls(cx))
            .right(self.refresh_control(cx))
    }
}

impl FinancialCalendarWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        _config: FinancialCalendarConfig,
        source: S,
        status: Entity<WidgetStatus>,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: CalendarSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            CalendarService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let table = cx.new(|_| CalendarTable::default());
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
        // is open: signing out has to take the calendar with it, and signing in has
        // to read. Following the account event is what makes that happen without the
        // pane having to be reopened.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });
        // A day and a time are read from the rows the table holds, and those rows are
        // grouped for one zone: moving the setting regroups them. The frame that reads
        // them is the pane's own — the table is a dataset rather than a view, and its
        // rows are read while the pane draws — so the pane is what has to be notified.
        let time_zone = on_data_time_zone_change(cx, |this, _, cx| this.regroup(cx));

        Self {
            binding,
            status,
            recovery,
            table,
            snapshot: None,
            next_refresh_at: None,
            countdown: None,
            refresh_pending: false,
            // The pane records the account it was built for, but it does not start
            // anything: the framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            _countdown_task: countdown_task,
            _account: account,
            _time_zone: time_zone,
        }
    }

    /// The account this pane would accept an answer from.
    fn current_key(&self) -> Option<CalendarStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CalendarStreamKey { account_id })
    }

    /// The deadline this pane counts down to, if it has one.
    ///
    /// A countdown belongs to a ready pane: while a read is in flight there is
    /// nothing to count down to — the deadline the answer will carry belongs to the
    /// read after the one being waited for — and a failed pane has no schedule left.
    fn countdown_deadline(&self, cx: &impl AppContext) -> Option<i64> {
        let status = self.status.state(cx);
        if self.refresh_pending || status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    fn refresh_is_disabled(&self, cx: &impl AppContext) -> bool {
        self.countdown_deadline(cx).is_none()
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
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        self.refresh_pending = false;
        // The countdown belongs to the schedule this call is replacing. Nothing is
        // drawing it while the pane is idle again, but the tick that notices is up to
        // a quarter of a second away, and the frame between the two must not show the
        // deadline of the schedule that has just been dropped.
        self.countdown = None;
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for. The
            // stream the previous account was reading is stopped rather than left
            // polling for a reader who is no longer there.
            self.clear_answer(cx);
            self.binding.call(CalendarRequest::Stop, cx, |_, _, _| {});
            self.status.report(
                WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
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
                CalendarRequest::Retry(key)
            } else {
                CalendarRequest::Load(key)
            },
            cx,
            Self::apply_rpc_result,
        );
        cx.notify();
    }

    /// Drops the answer and the rows drawn from it.
    fn clear_answer(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.next_refresh_at = None;
        self.countdown = None;
        self.table.update(cx, |table, _| table.clear());
        cx.notify();
    }

    /// Replaces every row the table draws with the answer it now holds.
    ///
    /// A read answers with the whole calendar, so this is a rebuild rather than a
    /// merge: a day may have gained an event, lost one, or moved to another day as
    /// the setting moved. The rows are read on the frame the pane draws, so the answer
    /// the binding accepts is what asks for that frame.
    fn rebuild_table(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let zone = data_time_zone(cx);
        self.table
            .update(cx, |table, _| table.rebuild(snapshot, zone));
    }

    /// Groups the rows the table already has for the zone the settings hold now.
    fn regroup(&mut self, cx: &mut Context<Self>) {
        let zone = data_time_zone(cx);
        self.table.update(cx, |table, _| table.regroup(zone));
        cx.notify();
    }

    /// Reads the account's calendar again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, and the control that calls it carries no click
    /// handler while it is disabled — this guard is the second one, at the action's
    /// entry.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_is_disabled(cx) {
            return;
        }
        self.refresh_pending = true;
        // The countdown belongs to the schedule this click is stepping ahead of; a
        // disabled control that kept drawing it would read as if the read were still
        // on its way at the old time.
        self.countdown = None;
        cx.notify();
        self.binding
            .call(CalendarRequest::Refresh, cx, Self::apply_refresh_result);
    }

    /// Applies the answer to a load or a retry.
    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<CalendarProtocol>,
        cx: &mut Context<Self>,
    ) {
        if self.status.is_failed(cx) {
            return;
        }
        match result {
            Ok(
                CalendarResponse::StreamStarted
                | CalendarResponse::RefreshQueued
                | CalendarResponse::Stopped,
            ) => {}
            Err(error) => {
                self.refresh_pending = false;
                // A failed read has no schedule left to count down to; the tick that
                // would notice is up to a quarter of a second away.
                self.countdown = None;
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(CalendarFailure::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    /// Applies the answer to a refresh command.
    ///
    /// An accepted command stays pending until the calendar it asked for is reported:
    /// the acknowledgement and the answer are two different messages, and the service
    /// refuses a second command in between.
    fn apply_refresh_result(
        &mut self,
        result: WidgetRpcResult<CalendarProtocol>,
        cx: &mut Context<Self>,
    ) {
        if result.is_err() {
            self.refresh_pending = false;
        }
        self.apply_rpc_result(result, cx);
    }

    /// Applies the failure the stream reported.
    ///
    /// Returns whether anything a frame draws changed. The failure itself is drawn by
    /// the framework around the body, but taking its countdown off the header is the
    /// pane's own change, so the frame that draws the header is asked for from here.
    fn apply_reliable(
        &mut self,
        key: CalendarStreamKey,
        message: Arc<CalendarStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let CalendarStreamMessage::Failed { error } = message.as_ref() else {
            return false;
        };
        self.refresh_pending = false;
        // The loop that owned the schedule has ended, so the label it left behind is
        // not a time anything will happen at.
        self.countdown = None;
        let recovery = self.recovery.clone();
        self.status.report(
            WidgetState::failed(Arc::new(CalendarFailure::Fetch(error.clone())), recovery),
            cx,
        );
        true
    }

    /// Applies the calendar the stream published.
    ///
    /// Returns whether anything a frame draws changed. A whole answer always does:
    /// the table is rebuilt from nothing, so a calendar that reads the same is a
    /// table of new rows rather than a frame that could be skipped.
    fn apply_latest(
        &mut self,
        key: CalendarStreamKey,
        message: Arc<CalendarStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let CalendarStreamMessage::Snapshot {
            calendar,
            next_refresh_at,
        } = message.as_ref()
        else {
            return false;
        };
        self.snapshot = Some(calendar.clone());
        self.next_refresh_at = Some(*next_refresh_at);
        self.refresh_pending = false;
        self.rebuild_table(cx);
        self.status.report(WidgetState::Ready, cx);

        true
    }

    /// Rebuilds the countdown, asking for the frame that draws it.
    ///
    /// The countdown is the only thing this pane draws that moves on its own, and it
    /// moves once a second: the task wakes four times a second so a second is never
    /// missed, and only the write that changed it asks for a frame. The header is not
    /// cached, so nothing else here has to announce itself.
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

    /// The control that reads the calendar again, and the time left until the next
    /// automatic read.
    ///
    /// The countdown is the button's own label rather than a word beside it: it is
    /// the number that says the pane is still alive, and the control that resets it
    /// is the one place a reader would look for it.
    fn refresh_control(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.refresh_is_disabled(cx);
        let widget = cx.entity();

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .debug_selector(|| "TEST_FINANCIAL_CALENDAR_REFRESH".into())
                    .child(
                        Button::new(ElementId::Name(SharedString::new_static(
                            "FinancialCalendarRefresh",
                        )))
                        .ghost()
                        .small()
                        .disabled(disabled)
                        .aria_label(t!(cx, "widget.financial_calendar.refresh"))
                        .child(Icon::new(IconKind::ArrowsClockwise).size_3())
                        .children(self.countdown.clone())
                        // A disabled button is only drawn as disabled, so the click
                        // handler is not attached at all — and the action guards
                        // itself as well.
                        .when(!disabled, |button| {
                            button.on_click(move |_, _, app| {
                                widget.update(app, |this, cx| this.refresh(cx));
                            })
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The three filters and the control that clears them, in the order a reader
    /// narrows a calendar down: how much it matters, when, and where.
    ///
    /// A trigger states the column it narrows and the choice it holds — "Importance:
    /// ALL" — while its menu keeps the choices themselves, because those are what a
    /// reader picks.
    ///
    /// They are greyed while there is no calendar at all: a choice only says which of
    /// the rows on screen to draw, so with no rows to narrow it would change nothing,
    /// and a live control that changes nothing reads as a broken one.
    fn filter_controls(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.snapshot.is_none();

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(control(
                "TEST_FINANCIAL_CALENDAR_IMPORTANCE",
                self.importance_select(cx, disabled),
            ))
            .child(control(
                "TEST_FINANCIAL_CALENDAR_TIME",
                self.time_select(cx, disabled),
            ))
            .child(control(
                "TEST_FINANCIAL_CALENDAR_COUNTRY",
                self.country_select(cx, disabled),
            ))
            .child(control(
                "TEST_FINANCIAL_CALENDAR_RESET",
                self.reset_control(cx, disabled),
            ))
            .into_any_element()
    }

    /// The importance filter.
    ///
    /// The levels are the page's own fixed scale, so all of them are offered whether or
    /// not the answer holds an event of that level: an option that came and went with
    /// the data would move the others under the reader's hand.
    fn importance_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let name = t!(cx, "widget.financial_calendar.importance");
        let all = t!(cx, "widget.financial_calendar.all");
        let chosen = self.table.read(cx).filter().importance.map_or_else(
            || all.clone(),
            |importance| importance_label(importance, cx),
        );
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "FinancialCalendarImportance",
        )))
        .placeholder(t!(
            cx,
            "widget.financial_calendar.filter_choice",
            filter = name.clone(),
            value = chosen
        ))
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(name.clone())
        .with_tooltip(t!(
            cx,
            "widget.financial_calendar.filter_tooltip",
            filter = name
        ))
        .menu_item(MenuItem::Action(MenuItemAction::new(
            SharedString::new_static(ALL_CHOICE),
            all,
        )));

        for importance in Importance::LEVELS {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new_static(importance_choice(importance)),
                importance_label(importance, cx),
            )));
        }

        select.on_change({
            let widget = cx.entity();
            move |selected, _, app| {
                let Some(choice) = selected.as_deref() else {
                    return;
                };
                let importance = importance_of_choice(choice);
                widget.update(app, |this, cx| this.select_importance(importance, cx));
            }
        })
    }

    /// The day filter.
    ///
    /// The days are the ones the answer holds, drawn the way the table heads them, so a
    /// reader picks the label they see over the rows they want. A day is chosen by the
    /// date itself rather than by its label: the label is a word of the language
    /// showing now, and a choice has to survive a change of it.
    fn time_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let name = t!(cx, "common.time");
        let all = t!(cx, "widget.financial_calendar.all");
        let table = self.table.read(cx);
        let chosen = table
            .filter()
            .date
            .map_or_else(|| all.clone(), |date| day_label_text(date, cx));
        let select = table.dates().iter().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "FinancialCalendarTime",
            )))
            .placeholder(t!(
                cx,
                "widget.financial_calendar.filter_choice",
                filter = name.clone(),
                value = chosen
            ))
            .ghost()
            .small()
            .disabled(disabled)
            .aria_label(name.clone())
            .with_tooltip(t!(
                cx,
                "widget.financial_calendar.filter_tooltip",
                filter = name
            ))
            .menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new_static(ALL_CHOICE),
                all,
            ))),
            |select, date| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    date_choice(*date),
                    day_label_text(*date, cx),
                )))
            },
        );

        select.on_change({
            let widget = cx.entity();
            move |selected, _, app| {
                let Some(choice) = selected.as_deref() else {
                    return;
                };
                widget.update(app, |this, cx| this.select_date(choice, cx));
            }
        })
    }

    /// The country filter.
    ///
    /// A country is drawn as the code the answer publishes, which is what the table's
    /// country column draws: a reader scanning the rows for one code finds that same
    /// code in the menu.
    fn country_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let name = t!(cx, "widget.financial_calendar.country");
        let all = t!(cx, "widget.financial_calendar.all");
        let table = self.table.read(cx);
        let chosen = table.filter().country.as_ref().map_or_else(
            || all.clone(),
            |country| SharedString::new(country.as_str()),
        );
        let select = table.countries().iter().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "FinancialCalendarCountry",
            )))
            .placeholder(t!(
                cx,
                "widget.financial_calendar.filter_choice",
                filter = name.clone(),
                value = chosen
            ))
            .ghost()
            .small()
            .disabled(disabled)
            .aria_label(name.clone())
            .with_tooltip(t!(
                cx,
                "widget.financial_calendar.filter_tooltip",
                filter = name
            ))
            .menu_item(MenuItem::Action(MenuItemAction::new(
                SharedString::new_static(ALL_CHOICE),
                all,
            ))),
            |select, country| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(country.as_str()),
                    SharedString::new(country.as_str()),
                )))
            },
        );

        select.on_change({
            let widget = cx.entity();
            move |selected, _, app| {
                let Some(choice) = selected.as_deref() else {
                    return;
                };
                let country = country_of_choice(choice);
                widget.update(app, |this, cx| this.select_country(country, cx));
            }
        })
    }

    /// The control that puts every filter back to "all".
    ///
    /// It is greyed while there is nothing to clear, and — like every other disabled
    /// control of this pane — it carries no click handler at all.
    fn reset_control(&self, cx: &Context<Self>, disabled: bool) -> Button {
        let resettable = !self.table.read(cx).filter().is_all();
        let disabled = disabled || !resettable;
        let label = t!(cx, "widget.financial_calendar.reset");
        let widget = cx.entity();

        Button::new(ElementId::Name(SharedString::new_static(
            "FinancialCalendarReset",
        )))
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(label.clone())
        .with_tooltip(label)
        .child(Icon::new(IconKind::FunnelX).size_3())
        .when(!disabled, |button| {
            button.on_click(move |_, _, app| {
                widget.update(app, |this, cx| this.reset_filters(cx));
            })
        })
    }

    /// Narrows the rows to one importance level, or to every level.
    fn select_importance(&mut self, importance: Option<Importance>, cx: &mut Context<Self>) {
        let zone = data_time_zone(cx);
        self.table
            .update(cx, |table, _| table.select_importance(importance, zone));
        cx.notify();
    }

    /// Narrows the rows to the day a menu entry names, or to every day.
    ///
    /// A choice the answer does not hold — a menu left open while a read replaced the
    /// calendar under it — names no day, which is the "all" option.
    fn select_date(&mut self, choice: &str, cx: &mut Context<Self>) {
        let date = if choice == ALL_CHOICE {
            None
        } else {
            let table = self.table.read(cx);
            table
                .dates()
                .iter()
                .copied()
                .find(|date| date_choice(*date).as_str() == choice)
        };
        let zone = data_time_zone(cx);
        self.table
            .update(cx, |table, _| table.select_date(date, zone));
        cx.notify();
    }

    /// Narrows the rows to one country, or to every country.
    fn select_country(&mut self, country: Option<SmolStr>, cx: &mut Context<Self>) {
        let zone = data_time_zone(cx);
        self.table
            .update(cx, |table, _| table.select_country(country, zone));
        cx.notify();
    }

    /// Puts every filter back to "all", which is what the reset control does.
    fn reset_filters(&mut self, cx: &mut Context<Self>) {
        let zone = data_time_zone(cx);
        self.table.update(cx, |table, _| table.reset_filter(zone));
        cx.notify();
    }
}

/// The stable id of the "all" option every filter offers.
const ALL_CHOICE: &str = "all";

/// The id an importance level is chosen by.
///
/// It is the level's name rather than its label, so the choice survives a change of
/// language the way a day's does.
fn importance_choice(importance: Importance) -> &'static str {
    match importance {
        Importance::Low => "low",
        Importance::Medium => "medium",
        Importance::High => "high",
        Importance::Unspecified => "unspecified",
    }
}

/// The level a menu entry names, or every level when it names none of them.
fn importance_of_choice(choice: &str) -> Option<Importance> {
    Importance::LEVELS
        .into_iter()
        .find(|importance| importance_choice(*importance) == choice)
}

/// The label of one importance level.
fn importance_label(importance: Importance, cx: &App) -> SharedString {
    let key = match importance {
        Importance::Low => "widget.financial_calendar.importance_level.low",
        Importance::Medium => "widget.financial_calendar.importance_level.medium",
        Importance::High => "widget.financial_calendar.importance_level.high",
        Importance::Unspecified => "widget.financial_calendar.importance_level.unspecified",
    };

    t!(cx, key)
}

/// The id a day is chosen by.
///
/// It is the date itself, written the way a date is read from an answer rather than the
/// way it is drawn: the label is a word of the language showing now, and a choice has
/// to survive a change of it.
fn date_choice(date: Date) -> SharedString {
    SharedString::new(format_smolstr!("{date}").as_str())
}

/// The country a menu entry names, or every country when it names the "all" option.
fn country_of_choice(choice: &str) -> Option<SmolStr> {
    (choice != ALL_CHOICE).then(|| SmolStr::new(choice))
}

/// One header control, in a box of its own.
///
/// The controls themselves are `base_ui` elements and carry no debug selector, and the
/// order they sit in is part of what this pane promises: a box per control is what lets
/// a test tell them apart and measure that order.
fn control(selector: &'static str, control: impl IntoElement) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .child(control)
        .into_any_element()
}

impl Render for FinancialCalendarWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the ready
        // state and nothing else. Everything the pane is *doing* is drawn by the
        // framework around this.
        div()
            .id(ElementId::Name(SharedString::new_static(
                "FinancialCalendarFrame",
            )))
            .aria_label(t!(cx, "widget.names.financial_calendar"))
            .size_full()
            .min_h_0()
            .child(Table::new(
                ElementId::Name(SharedString::new_static("FinancialCalendar")),
                &self.table,
            ))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        time_zone::DataTimeZone,
    };
    use base_ui::{animation::ANIMATION_TICK, table::TableDataSet};
    use gpui_kit::{
        AppContext, BorrowAppContext, IntoElement, Modifiers, Render, TestAppContext,
        VisualTestContext, Window, div,
    };
    use i18n::language::codes::LanguageCode;
    use widget::protocol::RequestId;

    use super::*;
    use crate::{
        table::CalendarTable,
        test_support::{self, ACCOUNT, FakeSource, FetchPlan},
    };

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

    /// Drives both executors until `ready` holds.
    ///
    /// A pane that lets the service answer reads on the GPUI executor while the
    /// service runs on the application runtime, so a test that only parks one of them
    /// would wait for a snapshot the other has not published yet.
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

    /// The widget the framework's own constructor would build, over `source`.
    fn widget_with_source<S>(
        source: S,
        cx: &mut Context<FinancialCalendarWidget>,
    ) -> FinancialCalendarWidget
    where
        S: CalendarSource,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        FinancialCalendarWidget::new_with_source(
            FinancialCalendarConfig,
            source,
            status,
            recovery,
            cx,
        )
    }

    /// A pane the framework would have built, over `source`, scoped to `account`.
    fn pane_with<S>(
        cx: &mut TestAppContext,
        account: Option<&str>,
        source: S,
    ) -> Entity<FinancialCalendarWidget>
    where
        S: CalendarSource,
    {
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx);
            sign_in(cx, account);
            cx.new(|cx| {
                let mut pane = widget_with_source(source, cx);
                pane.load(cx);
                pane
            })
        })
    }

    /// A pane the framework would have built, driven by a fake that never answers.
    ///
    /// The fake never answers because the tests below are the only thing that reports
    /// to the pane: a source that answered would race them, since its service
    /// publishes on a task of its own.
    fn pane(cx: &mut TestAppContext, account: Option<&str>) -> Entity<FinancialCalendarWidget> {
        pane_with(cx, account, FakeSource::new([FetchPlan::Pending]))
    }

    fn key(account: &str) -> CalendarStreamKey {
        CalendarStreamKey {
            account_id: SmolStr::new(account),
        }
    }

    /// Reports a calendar as the stream would, with the deadline it arms.
    fn report_snapshot(
        pane: &Entity<FinancialCalendarWidget>,
        calendar: &Arc<CalendarSnapshot>,
        next_refresh_at: i64,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| {
            pane.binding.cancel();
            pane.apply_latest(
                key(ACCOUNT),
                Arc::new(CalendarStreamMessage::Snapshot {
                    calendar: calendar.clone(),
                    next_refresh_at,
                }),
                context,
            )
        })
    }

    /// Reports a failure as the stream would.
    fn report_failure(pane: &Entity<FinancialCalendarWidget>, cx: &mut App) -> bool {
        pane.update(cx, |pane, context| {
            pane.binding.cancel();
            pane.apply_reliable(
                key(ACCOUNT),
                Arc::new(CalendarStreamMessage::Failed {
                    error: CalendarFetchError::Service(
                        kytrix_service_provider::service::engine::KytrixServiceError::Connect(
                            "the test source refused".into(),
                        ),
                    ),
                }),
                context,
            )
        })
    }

    /// The rows the pane's table draws now.
    fn rows(pane: &Entity<FinancialCalendarWidget>, cx: &App) -> usize {
        pane.read(cx).table.read(cx).rows_count()
    }

    /// The failure the pane published, which is what the framework draws.
    fn published_failure(
        pane: &Entity<FinancialCalendarWidget>,
        cx: &App,
    ) -> widget::status::Failure {
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .cloned()
            .expect("a failure should be published")
    }

    #[gpui_kit::test]
    fn a_pane_without_an_account_should_ask_for_one(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Snapshot(2)]);
        let reads = source.clone();
        let pane = pane_with(cx, None, source);
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
                "a pane nobody is signed in for says what it is waiting for"
            );
            assert_eq!(
                reads.fetch_count(),
                0,
                "and asks the service for nothing it cannot be served"
            );
            assert!(!pane.read(cx).has_content());
            assert_eq!(
                rows(&pane, cx),
                0,
                "there is nothing to draw without an account"
            );
        });
    }

    #[gpui_kit::test]
    fn a_pane_with_an_account_should_draw_what_arrives(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            assert!(
                !pane.read(cx).has_content(),
                "a pane is not content until it has an answer"
            );
            assert_eq!(
                rows(&pane, cx),
                0,
                "a pane that is still connecting draws no rows"
            );
        });

        cx.update(|cx| {
            let changed = report_snapshot(&pane, &test_support::snapshot(3), i64::MAX, cx);

            assert!(changed, "accepting an answer is a change worth a frame");
            assert!(pane.read(cx).has_content());
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
            assert_eq!(
                rows(&pane, cx),
                4,
                "three events of one day are one label and three rows"
            );
        });
    }

    /// A read answers with the whole calendar, so an answer of another length is a
    /// table of another length.
    #[gpui_kit::test]
    fn a_second_answer_should_rebuild_the_table_rather_than_append_to_it(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(3), i64::MAX, cx);
            assert_eq!(rows(&pane, cx), 4);

            report_snapshot(&pane, &test_support::snapshot(1), i64::MAX, cx);

            assert_eq!(
                rows(&pane, cx),
                2,
                "the answer replaces every row the table drew before it"
            );
        });
    }

    /// An answer is taken from the account the pane follows and from no other, in
    /// both directions: one served to somebody else is not this pane's, and one
    /// served to the account it has just left is not either.
    #[gpui_kit::test]
    fn a_late_answer_should_be_ignored_for_either_account_it_does_not_belong_to(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            let changed = pane.update(cx, |pane, context| {
                pane.binding.cancel();
                pane.apply_latest(
                    key("another-account"),
                    Arc::new(CalendarStreamMessage::Snapshot {
                        calendar: test_support::snapshot(2),
                        next_refresh_at: i64::MAX,
                    }),
                    context,
                )
            });

            assert!(!changed, "another account's answer is not this pane's");
            assert!(!pane.read(cx).has_content());
        });

        cx.update(|cx| {
            pane.update(cx, |pane, context| {
                pane.reconcile_account(Some(SmolStr::new_static("another-account")), context);
            });
            assert!(!pane.read(cx).has_content());

            let changed = pane.update(cx, |pane, context| {
                pane.apply_latest(
                    key(ACCOUNT),
                    Arc::new(CalendarStreamMessage::Snapshot {
                        calendar: test_support::snapshot(2),
                        next_refresh_at: i64::MAX,
                    }),
                    context,
                )
            });

            assert!(
                !changed,
                "the answer of the account the pane has left is not one it takes either"
            );
            assert!(!pane.read(cx).has_content());
        });
    }

    #[gpui_kit::test]
    fn a_failure_should_keep_the_calendar_on_screen_and_offer_a_recovery(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            report_failure(&pane, cx);

            assert!(pane.read(cx).status.is_failed(cx));
            let failure = published_failure(&pane, cx);
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
                "the calendar the user was reading stays on screen"
            );
            assert_eq!(rows(&pane, cx), 3, "and so do its rows");
        });
    }

    /// A command the service refused is a failure the pane reports, with the recovery
    /// every other failure offers: the click was the reader's, so the refusal is owed
    /// to them rather than silently dropped.
    #[gpui_kit::test]
    fn a_refused_refresh_should_be_reported_as_a_failure(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(pane.refresh_pending);
                pane.apply_refresh_result(
                    Err(RpcCallError::Service {
                        request_id: RequestId::new(1),
                        source: CalendarServiceError::StreamBusy,
                    }),
                    cx,
                );
            });

            assert!(pane.read(cx).status.is_failed(cx));
            assert!(
                !pane.read(cx).refresh_pending,
                "a refused command is not one to keep waiting for"
            );
            let failure = published_failure(&pane, cx);
            assert_eq!(
                failure.text(cx),
                t!(cx, "error.financial_calendar.refresh_already_waiting"),
                "the refusal names the wait it collided with rather than the stream"
            );
            assert_eq!(failure.action().label_key(), "common.retry");
        });
    }

    #[gpui_kit::test]
    fn a_failure_should_be_terminal_until_the_user_recovers(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_failure(&pane, cx);
            assert!(pane.read(cx).status.is_failed(cx));

            let answer_changed = report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);

            assert!(!answer_changed, "a failed pane takes no more answers");
            assert!(pane.read(cx).status.is_failed(cx));

            // The recovery the failure offered is what starts it again.
            pane.update(cx, |pane, cx| pane.load(cx));

            assert!(!pane.read(cx).status.is_failed(cx));
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
        });
    }

    /// The countdown belongs to a ready pane, and the control is greyed wherever there
    /// is nothing to count down to.
    #[gpui_kit::test]
    fn the_countdown_should_only_run_while_the_pane_is_ready(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));
        let tick = |pane: &Entity<FinancialCalendarWidget>, cx: &mut App| {
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
        };

        cx.update(|cx| {
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "a pane already waiting for its first answer has nothing to bring forward"
            );
        });

        cx.update(|cx| {
            report_snapshot(
                &pane,
                &test_support::snapshot(2),
                unix_millis() + 10 * 60 * 1_000,
                cx,
            );
            tick(&pane, cx);

            assert_eq!(
                pane.read(cx).countdown.clone(),
                Some(SharedString::new_static("10:00")),
                "a ready pane counts down to the deadline the service published"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));
        });

        cx.update(|cx| {
            let before = pane.read(cx).refresh_is_disabled(cx);
            assert!(!before);

            report_snapshot(&pane, &test_support::snapshot(2), unix_millis() - 1_000, cx);
            tick(&pane, cx);

            assert_eq!(
                pane.read(cx).countdown.clone(),
                Some(SharedString::new_static("00:00")),
                "a deadline that has passed reads as no time left rather than as a negative wait"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_pane_should_stop_counting_down(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(
                &pane,
                &test_support::snapshot(2),
                unix_millis() + 60_000,
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(
                pane.read(cx).countdown.is_some(),
                "a ready pane counts down to its next read"
            );

            report_failure(&pane, cx);
            assert!(
                pane.read(cx).countdown.is_none(),
                "the failure takes the countdown off at once rather than a tick later"
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));

            assert!(
                pane.read(cx).countdown.is_none(),
                "a pane whose schedule stopped has nothing to count down to"
            );
            assert!(pane.read(cx).refresh_is_disabled(cx));
        });
    }

    /// The double click the refresh control exists to absorb.
    ///
    /// The control closes itself on the click, and the service's acknowledgement is
    /// not what reopens it: the command and the answer are two messages, and the
    /// service refuses a second command in between.
    #[gpui_kit::test]
    fn an_acknowledged_refresh_should_keep_the_control_closed_until_the_answer(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(
                &pane,
                &test_support::snapshot(2),
                unix_millis() + 60_000,
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
            assert!(pane.read(cx).countdown.is_some());
            assert!(!pane.read(cx).refresh_is_disabled(cx));
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(
                    pane.countdown.is_none(),
                    "the click takes the schedule it stepped ahead of off the control"
                );
                assert!(
                    pane.refresh_is_disabled(cx),
                    "the click closes the window itself, before anything has answered"
                );
                pane.apply_refresh_result(Ok(CalendarResponse::RefreshQueued), cx);
                assert!(
                    pane.refresh_is_disabled(cx),
                    "a queued read is still a read in flight"
                );
            });
        });
    }

    /// The control is the first guard and the action is the second: a click the pane
    /// has nothing to do for leaves no refresh waiting behind it.
    #[gpui_kit::test]
    fn a_refresh_a_failed_pane_has_nothing_to_do_for_should_not_leave_one_waiting(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_failure(&pane, cx);
            assert!(pane.read(cx).refresh_is_disabled(cx));

            pane.update(cx, |pane, cx| pane.refresh(cx));

            assert!(
                !pane.read(cx).refresh_pending,
                "a click with nothing behind it is not a refresh in flight"
            );
            assert!(
                pane.read(cx).status.is_failed(cx),
                "and it is not a reason to leave the failure"
            );
        });
    }

    /// One click is one read and one answer: what the control asks for reaches the
    /// service, and the answer it waited for is what reopens the control.
    #[gpui_kit::test]
    fn a_refresh_should_read_again_and_reopen_the_control(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Snapshot(2), FetchPlan::Snapshot(3)]);
        let pane = pane_with(cx, Some(ACCOUNT), source.clone());
        settle_until(cx, "the first answer", |cx| {
            pane.read(cx).status.state(cx) == WidgetState::Ready
        });

        cx.update(|cx| {
            assert_eq!(
                rows(&pane, cx),
                3,
                "two events of one day are one label and two rows"
            );
            assert!(!pane.read(cx).refresh_is_disabled(cx));

            pane.update(cx, |pane, cx| pane.refresh(cx));

            assert!(
                pane.read(cx).refresh_pending,
                "the click leaves a refresh waiting for its answer"
            );
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "and closes the control until that answer arrives"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "a refresh is not a state of its own: the calendar on screen is still ready"
            );
        });

        settle_until(cx, "the refreshed answer", |cx| rows(&pane, cx) == 4);

        cx.update(|cx| {
            assert!(!pane.read(cx).refresh_pending, "the answer ends the wait");
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "and reopens the control"
            );
            assert!(
                pane.read(cx)
                    .next_refresh_at
                    .is_some_and(|due| due > unix_millis()),
                "on the deadline the refreshed answer armed"
            );
            assert_eq!(
                source.fetch_count(),
                2,
                "the first answer and the one the click asked for"
            );
        });
    }

    /// A failure ends the refresh the control was waiting for: the loop it belonged
    /// to has stopped, so the wait cannot outlive it.
    #[gpui_kit::test]
    fn a_failure_should_end_a_pending_refresh(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            pane.update(cx, |pane, cx| pane.refresh(cx));
            assert!(pane.read(cx).refresh_pending);

            report_failure(&pane, cx);

            assert!(
                !pane.read(cx).refresh_pending,
                "the failure the stream reported ends the wait"
            );
            assert!(pane.read(cx).refresh_is_disabled(cx));
        });
    }

    /// A whole answer ends the wait even when it draws exactly what was on screen:
    /// the pane does not decide whether the calendar it was handed is new.
    #[gpui_kit::test]
    fn an_unchanged_answer_should_end_a_pending_refresh(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));
        let calendar = test_support::snapshot(2);

        cx.update(|cx| {
            report_snapshot(&pane, &calendar, i64::MAX, cx);
            pane.update(cx, |pane, cx| pane.refresh(cx));
            assert!(pane.read(cx).refresh_pending);

            let changed = report_snapshot(&pane, &calendar, i64::MAX, cx);

            assert!(
                changed,
                "the same calendar is still an answer worth a frame"
            );
            assert!(!pane.read(cx).refresh_pending, "the answer ends the wait");
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        });
    }

    /// The zone is the one thing outside the data that decides where a day begins, so
    /// moving it regroups the rows the pane already has without reading anything.
    #[gpui_kit::test]
    fn a_zone_change_should_regroup_the_rows_without_reading_again(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let pane = pane_with(cx, Some(ACCOUNT), source);

        let reads_before = reads.fetch_count();
        cx.update(|cx| {
            report_snapshot(&pane, &test_support::boundary_snapshot(), i64::MAX, cx);
            assert_eq!(rows(&pane, cx), 4, "two days and their two events at UTC");
        });

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.data_time_zone =
                            DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                rows(&pane, cx),
                3,
                "both events are the next day at UTC+8, so one label heads them"
            );
            assert_eq!(
                reads.fetch_count(),
                reads_before,
                "a zone is not a reason to read the calendar again"
            );
        });
    }

    #[gpui_kit::test]
    fn signing_out_should_take_the_calendar_with_it(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            assert!(pane.read(cx).has_content());

            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            assert!(!pane.read(cx).has_content());
            assert_eq!(rows(&pane, cx), 0, "the rows went with the answer");
            assert!(
                pane.read(cx).next_refresh_at.is_none(),
                "and so did the schedule the signed-in pane was counting down to"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
                "and the pane asks for an account rather than reporting a failure"
            );
        });
    }

    /// Signing out does not just hide the answer: it ends the poll loop that was
    /// reading it, so a signed-out pane asks the service for nothing.
    #[gpui_kit::test]
    fn signing_out_should_stop_the_poll_loop(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let pane = pane_with(cx, Some(ACCOUNT), source);
        settle_until(cx, "the first read", |_| reads.fetch_count() == 1);

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.account = None, cx);
            });
        });

        settle_until(cx, "the read to be dropped", |_| {
            reads.cancelled_fetches() == 1
        });
        cx.update(|cx| {
            assert_eq!(
                reads.fetch_count(),
                1,
                "a signed-out pane reads nothing more"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
                "and waits for an account rather than reporting a failure"
            );
        });
    }

    /// A command belongs to the stream it was sent on, so the account it was sent for
    /// changing is what ends it.
    #[gpui_kit::test]
    fn another_account_should_end_a_pending_refresh(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            pane.update(cx, |pane, cx| pane.refresh(cx));
            assert!(pane.read(cx).refresh_pending);

            pane.update(cx, |pane, cx| {
                pane.reconcile_account(Some(SmolStr::new_static("another-account")), cx);
            });

            assert!(
                !pane.read(cx).refresh_pending,
                "the command belonged to the stream the account change replaced"
            );
            assert!(
                !pane.read(cx).has_content(),
                "and the calendar went with the account it was served to"
            );
        });
    }

    /// Another account is another read, and the pane hears about it from the settings
    /// rather than from a reload: the subscription is what carries a sign-in in the
    /// account settings to a pane that is already open.
    #[gpui_kit::test]
    fn an_account_change_should_read_the_new_account(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Snapshot(2)]);
        let reads = source.clone();
        let pane = pane_with(cx, Some(ACCOUNT), source);
        settle_until(cx, "the first read", |_| reads.fetch_count() == 1);

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

        settle_until(cx, "the replacement answer", |cx| {
            pane.read(cx).status.state(cx) == WidgetState::Ready
        });
        cx.update(|cx| {
            assert_eq!(pane.read(cx).account_id.as_deref(), Some("someone-else"));
            assert_eq!(
                reads.fetch_count(),
                2,
                "the new account is what the second read was for"
            );
            assert_eq!(rows(&pane, cx), 3, "and its answer is what is drawn");
        });
    }

    /// One pane mounted in a window of its own, which is what paints it.
    ///
    /// The header is built by the workspace around the widget, so a harness that drew
    /// the body alone would never paint the controls the header carries: this one draws
    /// the widget's own groups above the body.
    struct Harness {
        pane: Entity<FinancialCalendarWidget>,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let header = self.pane.update(cx, |pane, cx| pane.header(window, cx));

            div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .children(header.left)
                        .children(header.right),
                )
                .child(div().flex_1().min_h_0().child(self.pane.clone()))
        }
    }

    /// A pane in a window of its own, drawn once, over a source that never answers:
    /// what the tests below paint is what they report to it by hand.
    fn windowed(
        cx: &mut TestAppContext,
    ) -> (
        Entity<FinancialCalendarWidget>,
        &mut gpui_kit::VisualTestContext,
    ) {
        cx.executor().allow_parking();
        cx.update(install);
        cx.update(|cx| sign_in(cx, Some(ACCOUNT)));
        let (harness, cx): (Entity<Harness>, &mut gpui_kit::VisualTestContext) = cx
            .add_window_view(|_, cx| {
                let pane = cx.new(|cx| {
                    let mut pane = widget_with_source(FakeSource::new([FetchPlan::Pending]), cx);
                    pane.load(cx);
                    pane
                });
                Harness { pane }
            });
        let pane = cx.update(|_, cx| harness.read(cx).pane.clone());

        (pane, cx)
    }

    #[gpui_kit::test]
    fn a_pane_with_content_should_paint_the_head_of_its_first_day(cx: &mut TestAppContext) {
        let (pane, cx) = windowed(cx);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("TEST_FINANCIAL_CALENDAR_DAY").is_none(),
            "a pane with nothing to show has no day to head"
        );

        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("TEST_FINANCIAL_CALENDAR_DAY").is_some(),
            "a pane with a calendar heads its first day"
        );
    }

    /// An answer with nothing in it is content, and the body says so under its
    /// columns rather than leaving a blank pane the reader cannot read as either
    /// "no event is scheduled" or "this broke".
    #[gpui_kit::test]
    fn an_answer_with_nothing_in_it_should_draw_the_empty_state(cx: &mut TestAppContext) {
        let (pane, cx) = windowed(cx);

        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::snapshot(0), i64::MAX, cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.update(|_, cx| pane.read(cx).has_content()),
            "an empty answer is an answer: the framework draws the body rather than a spinner"
        );
        assert!(
            cx.debug_bounds("EMPTY_STATE").is_some(),
            "and the body says it is empty"
        );
        assert!(
            cx.debug_bounds("TEST_FINANCIAL_CALENDAR_DAY").is_none(),
            "while heading no day"
        );
    }

    /// The header carries the filters on the left and the refresh on the right, and the
    /// countdown is that control's own label rather than a word beside it.
    #[gpui_kit::test]
    fn the_header_should_carry_the_filters_the_refresh_and_its_countdown(cx: &mut TestAppContext) {
        let (pane, cx) = windowed(cx);

        cx.update(|_, cx| {
            report_snapshot(
                &pane,
                &test_support::snapshot(2),
                unix_millis() + 125_000,
                cx,
            );
            pane.update(cx, |pane, cx| pane.tick_countdown(cx));
        });

        cx.update(|window, cx| {
            let header = pane.update(cx, |pane, cx| pane.header(window, cx));

            assert!(
                header.left.is_some(),
                "the filters are the header's left group"
            );
            assert!(
                header.right.is_some(),
                "the refresh is the header's right group"
            );
        });

        cx.update(|_, cx| {
            let countdown = pane
                .read(cx)
                .countdown
                .clone()
                .expect("a ready pane counts down to its next read");
            assert_eq!(countdown.as_ref(), "02:05");
            assert!(!pane.read(cx).refresh_is_disabled(cx));
            assert!(
                pane.read(cx)
                    .next_refresh_at
                    .is_some_and(|due| due > unix_millis()),
                "the pane counts down to a read that is still ahead of it"
            );
        });
    }

    /// The four controls the header's left group is made of, in the order a reader
    /// narrows a calendar down: how much it matters, when, where, and the control that
    /// clears all three.
    #[gpui_kit::test]
    fn the_header_should_draw_the_filters_in_order_and_the_reset_last(cx: &mut TestAppContext) {
        let (pane, cx) = windowed(cx);
        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::boundary_snapshot(), i64::MAX, cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let selectors = [
            "TEST_FINANCIAL_CALENDAR_IMPORTANCE",
            "TEST_FINANCIAL_CALENDAR_TIME",
            "TEST_FINANCIAL_CALENDAR_COUNTRY",
            "TEST_FINANCIAL_CALENDAR_RESET",
        ]
        .map(|selector| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("the header should draw {selector}"))
        });

        for (one, next) in selectors.iter().zip(selectors.iter().skip(1)) {
            assert!(
                one.right() <= next.left(),
                "each filter sits left of the one after it: {one:?} then {next:?}"
            );
        }
    }

    /// Every filter says what it narrows while it is hovered, which is what a reader
    /// needs when the header is too narrow to show a whole choice.
    #[gpui_kit::test]
    fn every_filter_should_say_what_it_narrows_on_hover(cx: &mut TestAppContext) {
        let (pane, cx) = windowed(cx);
        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::boundary_snapshot(), i64::MAX, cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        for selector in [
            "TEST_FINANCIAL_CALENDAR_IMPORTANCE",
            "TEST_FINANCIAL_CALENDAR_TIME",
            "TEST_FINANCIAL_CALENDAR_COUNTRY",
        ] {
            hover(cx, selector);
            assert!(
                cx.debug_bounds("BASE_UI_TOOLTIP").is_some(),
                "{selector} should explain itself while it is hovered"
            );

            // The day label belongs to the body rather than to the header, so pointing
            // at it leaves the control the tooltip was opened from.
            hover(cx, "TEST_FINANCIAL_CALENDAR_DAY");
            assert!(
                cx.debug_bounds("BASE_UI_TOOLTIP").is_none(),
                "the tooltip belongs to the control it was opened from"
            );
        }
    }

    /// Moves the pointer onto what `selector` names and lets the tooltip it opens
    /// appear.
    fn hover(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the harness should draw {selector}"))
            .center();
        cx.simulate_mouse_move(target, None, Modifiers::none());
        cx.run_until_parked();
        for _ in 0..10 {
            cx.executor()
                .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }
    }

    /// A filter's trigger names the column it narrows and the choice it holds, while the
    /// menu keeps the choices themselves.
    #[test]
    fn a_filter_trigger_should_name_its_filter_and_its_choice() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");

        for (language, expected) in [
            (LanguageCode::En, "Importance: ALL"),
            (LanguageCode::ZhCn, "重要性：全部"),
        ] {
            let filter = i18n.translate(language, "widget.financial_calendar.importance");
            let value = i18n.translate(language, "widget.financial_calendar.all");
            let trigger = i18n.translate_with(
                language,
                "widget.financial_calendar.filter_choice",
                &[("filter", &filter), ("value", &value)],
            );

            assert_eq!(trigger.as_ref(), expected);
        }
    }

    /// Every filter's tooltip says what the control narrows, in the language showing
    /// now.
    #[test]
    fn a_filter_tooltip_should_say_what_the_filter_narrows() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");

        for (language, expected) in [
            (LanguageCode::En, "Filter by Country"),
            (LanguageCode::ZhCn, "按国家筛选"),
        ] {
            let filter = i18n.translate(language, "widget.financial_calendar.country");
            let tooltip = i18n.translate_with(
                language,
                "widget.financial_calendar.filter_tooltip",
                &[("filter", &filter)],
            );

            assert_eq!(tooltip.as_ref(), expected);
        }
    }

    /// A choice narrows the rows without reading the calendar again, and the reset
    /// control is what brings every row back.
    #[gpui_kit::test]
    fn a_choice_should_narrow_the_rows_and_the_reset_should_bring_them_back(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let pane = pane_with(cx, Some(ACCOUNT), source);

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            let reads_before = reads.fetch_count();
            assert_eq!(rows(&pane, cx), 3, "one label and the two events");

            pane.update(cx, |pane, cx| {
                pane.select_country(Some(SmolStr::new_static("US")), cx);
            });
            assert_eq!(rows(&pane, cx), 2, "one label and the one American event");
            assert_eq!(
                reads.fetch_count(),
                reads_before,
                "a filter is about the rows already on screen, not a reason to read again"
            );

            pane.update(cx, |pane, cx| {
                pane.select_importance(Some(Importance::High), cx);
            });
            assert_eq!(
                rows(&pane, cx),
                0,
                "the two choices narrow the calendar together: the fixture events are \
                 medium, so no row is left"
            );

            pane.update(cx, |pane, cx| pane.reset_filters(cx));
            assert_eq!(rows(&pane, cx), 3, "the reset brings every row back");
            assert!(
                pane.read(cx).table.read(cx).filter().is_all(),
                "and leaves every filter on its \"all\" option"
            );
        });
    }

    /// The reset is a control rather than a method the tests call: the click it carries
    /// is what clears the choices, and it is the same click a reader makes.
    #[gpui_kit::test]
    fn clicking_the_reset_control_should_clear_every_choice(cx: &mut TestAppContext) {
        let (pane, cx) = windowed(cx);
        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            pane.update(cx, |pane, cx| {
                pane.select_country(Some(SmolStr::new_static("US")), cx);
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| rows(&pane, cx)), 2);

        let target = cx
            .debug_bounds("TEST_FINANCIAL_CALENDAR_RESET")
            .expect("the header draws the reset control")
            .center();
        cx.simulate_click(target, Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_, cx| rows(&pane, cx)),
            3,
            "the click put the choice back to \"all\""
        );
        assert!(cx.update(|_, cx| pane.read(cx).table.read(cx).filter().is_all()));
    }

    /// The options belong to the answer, so a choice the new one no longer offers goes
    /// back to "all" rather than emptying the table.
    #[gpui_kit::test]
    fn a_choice_the_new_answer_no_longer_offers_should_be_reset(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            pane.update(cx, |pane, cx| {
                pane.select_country(Some(SmolStr::new_static("DE")), cx);
            });
            assert_eq!(rows(&pane, cx), 2, "one label and the one German event");

            report_snapshot(&pane, &test_support::boundary_snapshot(), i64::MAX, cx);

            assert_eq!(
                pane.read(cx).table.read(cx).filter().country,
                None,
                "the new answer holds no German event, so the choice is not one to keep"
            );
            assert_eq!(rows(&pane, cx), 4, "and every event of it is drawn again");
        });
    }

    /// The importance scale is the page's own rather than the answer's, so a level the
    /// new answer happens to have no event of is still a choice a reader may keep — and
    /// the table says the filter matched nothing.
    #[gpui_kit::test]
    fn an_importance_choice_should_outlive_an_answer_with_no_event_of_that_level(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx) = windowed(cx);

        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::boundary_snapshot(), i64::MAX, cx);
            pane.update(cx, |pane, cx| {
                pane.select_importance(Some(Importance::High), cx);
            });
            assert_eq!(
                rows(&pane, cx),
                2,
                "one label and the one event of that level"
            );

            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);

            assert_eq!(
                pane.read(cx).table.read(cx).filter().importance,
                Some(Importance::High),
                "the levels are fixed, so the answer cannot take one away"
            );
            assert_eq!(rows(&pane, cx), 0, "and nothing of the new answer is of it");
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("EMPTY_STATE").is_some(),
            "the table says it is empty rather than drawing nothing"
        );
    }

    /// The days an answer holds move with the zone, so a day the new grouping does not
    /// hold is a choice it cannot keep either.
    #[gpui_kit::test]
    fn a_day_the_zone_takes_away_should_reset_the_choice(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::boundary_snapshot(), i64::MAX, cx);
            let date =
                Date::from_calendar_date(2026, time::Month::October, 25).expect("a real date");
            pane.update(cx, |pane, cx| pane.select_date("2026-10-25", cx));
            assert_eq!(rows(&pane, cx), 2, "the late event of the 25th alone");
            assert_eq!(pane.read(cx).table.read(cx).filter().date, Some(date));
        });

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.data_time_zone =
                            DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).table.read(cx).filter().date,
                None,
                "eight hours east the answer holds no 25th to filter by"
            );
            assert_eq!(
                rows(&pane, cx),
                3,
                "so both events are drawn under the one day they fall on"
            );
        });
    }

    /// A pane that can no longer vouch for its answer leaves no filter behind either:
    /// the options went with the data the filter was about.
    #[gpui_kit::test]
    fn signing_out_should_reset_the_filters(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            pane.update(cx, |pane, cx| {
                pane.select_country(Some(SmolStr::new_static("US")), cx);
            });
            assert!(!pane.read(cx).table.read(cx).filter().is_all());

            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            assert!(
                pane.read(cx).table.read(cx).filter().is_all(),
                "the signed-out pane holds no option a filter could name"
            );
            assert_eq!(rows(&pane, cx), 0);
        });
    }

    /// A table nobody rebuilt is a table of no rows, which is what a pane that has not
    /// accepted an answer yet draws.
    #[gpui_kit::test]
    fn a_pane_that_has_not_answered_should_draw_no_rows(cx: &mut TestAppContext) {
        let pane = pane(cx, Some(ACCOUNT));

        cx.update(|cx| {
            let table: &Entity<CalendarTable> = &pane.read(cx).table;
            assert_eq!(table.read(cx).rows_count(), 0);
            assert!(
                !pane.read(cx).has_content(),
                "and the framework draws the connecting placeholder instead of its body"
            );
        });
    }

    /// The task behind the countdown wakes four times a second, so the frame it asks
    /// for is asked for on the write that changed the label and on no other: a notify
    /// per tick would redraw the pane four times a second for a label that moves once.
    #[gpui_kit::test]
    fn a_countdown_tick_should_ask_for_a_frame_only_when_its_label_changed(
        cx: &mut TestAppContext,
    ) {
        let (pane, cx) = windowed(cx);
        cx.update(|_, cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
        });

        let frames = Rc::new(Cell::new(0));
        let counted = frames.clone();
        let _observation = cx.update(|_, cx| {
            cx.observe(&pane, move |_, _| {
                counted.set(counted.get() + 1);
            })
        });

        // The two ticks are read before either executor is parked: parking would let
        // the task behind the countdown run the next tick, and a second can pass
        // between the two, which is a label change the test is not about.
        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.tick_countdown(cx)));
        assert_eq!(
            frames.get(),
            1,
            "the tick that writes the label asks for the frame that draws it"
        );

        cx.update(|_, cx| pane.update(cx, |pane, cx| pane.tick_countdown(cx)));
        assert_eq!(
            frames.get(),
            1,
            "a tick that leaves the label where it was asks for nothing"
        );
    }

    /// A language is not a reason to read the calendar again: the frame that draws a
    /// label or a day asks the catalogue in the language showing then, so switching
    /// one changes the text of the same rows rather than the rows.
    #[gpui_kit::test]
    fn a_language_change_should_neither_read_again_nor_drop_the_rows(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let pane = pane_with(cx, Some(ACCOUNT), source);
        let reads_before = reads.fetch_count();

        cx.update(|cx| {
            report_snapshot(&pane, &test_support::snapshot(2), i64::MAX, cx);
            let before = rows(&pane, cx);
            let english = t!(cx, "common.time");

            cx.set_global(i18n::language::codes::LanguageCode::ZhCn);

            assert_eq!(
                rows(&pane, cx),
                before,
                "the rows are the same answer in another language"
            );
            assert_ne!(
                t!(cx, "common.time"),
                english,
                "and the catalogue the next frame reads is the new language"
            );
            assert!(pane.read(cx).has_content());
            assert_eq!(
                reads.fetch_count(),
                reads_before,
                "a language is not a reason to read the calendar again"
            );
        });
    }
}
