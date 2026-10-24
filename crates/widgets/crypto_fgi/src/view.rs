//! The CMC fear and greed pane: the account it is scoped to, the reading it draws,
//! and the controls it offers.
//!
//! # What this pane is
//!
//! The index has a single current value and six history points, and the service
//! reads the whole snapshot every 60 seconds — so the pane never asks for a part of
//! the answer, it asks for *the* answer, and everything on screen is a view of the
//! one snapshot it holds. The gauge and the rows are therefore not scopes of their
//! own: a snapshot that draws something else redraws them, and a snapshot that would
//! draw the same pixels nothing at all. The collection time is not one of those
//! pixels, so a poll of an index that has not moved is not a redraw.
//!
//! # What it does with a failure
//!
//! A refused read is a failure the user sees: the pane reports it through the
//! framework, which puts a banner over the reading that is still on screen. The
//! service ends its schedule at that point, so the framework's recovery is what
//! starts it again — a service that is briefly unreachable does not turn into a
//! request every minute until someone notices.
//!
//! # Who may see it
//!
//! The service serves this snapshot over an authenticated connection, so a pane
//! with no account behind it fetches nothing and says so. That is a *waiting* state
//! rather than a failure: nothing has gone wrong, the pane is just not for an
//! anonymous reader. Nothing is retried on a timer either — the account event
//! subscription re-scopes the pane the moment the user signs in.
//!
//! # An empty snapshot is not an error
//!
//! Before its collector has run, the service answers with a snapshot whose
//! collection time is zero. That is a normal cold start rather than a broken
//! service, so the pane draws 「暂无数据」 in the body and reports readiness: a
//! spinner over a healthy empty answer would say the pane is still working, and a
//! failure would ask the user to recover from something that has not failed.

use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tooltip::Tooltip,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, base::Disableable, div, prelude::FluentBuilder, px,
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
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    gauge::{Gauge, level_color},
    model::{FgiParseError, FgiRow, FgiSnapshot, Level},
    service::{
        FgiFetchError, FgiProtocol, FgiRequest, FgiResponse, FgiService, FgiServiceError,
        FgiSource, FgiStreamKey, FgiStreamMessage,
    },
};

/// The dash a row without a reading draws.
///
/// It is not `common.no_data` and not a waiting state: the row's point does not
/// exist, while the pane may well have everything else to show.
const MISSING: &str = "—";

/// The debug selectors of the dash each row draws when it has no reading.
///
/// A painted frame's text is not readable from a test, so the selector a value
/// carries is what says a dash was drawn — and which row drew it. Every row has its
/// own, so a test can tell an absent row that still drew its dash from one that
/// silently stopped drawing it.
const MISSING_YESTERDAY: &str = "TEST_CRYPTO_FGI_MISSING_YESTERDAY";
const MISSING_LAST_WEEK: &str = "TEST_CRYPTO_FGI_MISSING_LAST_WEEK";
const MISSING_LAST_MONTH: &str = "TEST_CRYPTO_FGI_MISSING_LAST_MONTH";
const MISSING_LAST_YEAR: &str = "TEST_CRYPTO_FGI_MISSING_LAST_YEAR";
const MISSING_YEARLY_HIGH: &str = "TEST_CRYPTO_FGI_MISSING_YEARLY_HIGH";
const MISSING_YEARLY_LOW: &str = "TEST_CRYPTO_FGI_MISSING_YEARLY_LOW";

/// The pane has nothing to persist, so its config carries the schema version alone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CryptoFgiConfig;

impl WidgetConfig for CryptoFgiConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FgiFailure {
    Fetch(FgiFetchError),
    Rpc(RpcCallError<FgiServiceError>),
}

impl LocalizedError for FgiFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for FgiFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Parse(error) => error.localized_message(cx),
        }
    }
}

impl LocalizedError for FgiParseError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::ScoreOutOfRange { value } => {
                t!(cx, "error.crypto_fgi.score_out_of_range", value = value)
            }
        }
    }
}

impl LocalizedError for FgiServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t!(cx, "error.crypto_fgi.stream_unavailable")
            }
        }
    }
}

/// One fear and greed pane: the account it follows, the reading it draws, and its
/// refresh control.
pub struct CryptoFgiWidget {
    binding: WidgetBinding<FgiProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    gauge: Gauge,
    /// The answer the body is drawn from, or nothing before the first one arrives.
    snapshot: Option<Arc<FgiSnapshot>>,
    /// Whether a refresh this pane sent is still waiting for the reading it asked
    /// for. It closes the window between the click and the answer, during which the
    /// service would refuse a second command.
    refresh_pending: bool,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    /// Keeps the pane following the account the settings hold.
    _account: Subscription,
}

impl WidgetView for CryptoFgiWidget {
    type Config = CryptoFgiConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.crypto_fgi");
    const DISPLAY_NAME: &'static str = "widget.names.crypto_fgi";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "CoinMarketCap crypto fear and greed index";
    const ICON: Option<&'static str> = Some("widgets/crypto_fgi.png");
    const SELECTABLE: bool = true;

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(source, status, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // A retry keeps the reading the user is still looking at — blanking it to
        // reconnect would flash a screen of data away — while an explicit load
        // starts over, because the pane cannot vouch for what it was showing.
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

    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().right(self.refresh_control(cx))
    }
}

impl CryptoFgiWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        source: S,
        status: Entity<WidgetStatus>,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: FgiSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            FgiService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
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
            gauge: Gauge::new(),
            snapshot: None,
            refresh_pending: false,
            // The pane records the account it was built for, but it does not start
            // anything: the framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            _account: account,
        }
    }

    /// The account this pane would accept an answer from.
    fn current_key(&self) -> Option<FgiStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| FgiStreamKey { account_id })
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the pane is ready and no refresh of its own is still
    /// waiting: a pane that is connecting has nothing to bring forward, a failed one
    /// is recovered through the framework's own control above the reading, and a
    /// pending refresh would be refused by the service as busy.
    fn refresh_is_disabled(&self, cx: &impl AppContext) -> bool {
        self.refresh_pending || self.status.state(cx) != WidgetState::Ready
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
        // The command that was in flight belongs to the stream this call is
        // replacing, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for.
            self.clear_snapshot(cx);
            self.status.report(
                WidgetState::waiting(Notice::message("widget.crypto_fgi.logged_out")),
                cx,
            );
            cx.notify();
            return;
        };
        if !retry {
            self.clear_snapshot(cx);
        }
        self.status.report(WidgetState::Connecting, cx);
        self.binding.call(
            if retry {
                FgiRequest::Retry(key)
            } else {
                FgiRequest::Load(key)
            },
            cx,
            Self::apply_rpc_result,
        );
        cx.notify();
    }

    /// Drops the reading the pane can no longer vouch for.
    fn clear_snapshot(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        cx.notify();
    }

    /// Reads the account's answer again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, and the control that calls it does not even
    /// carry a click handler when it is disabled — this guard is the second one, at
    /// the action's entry.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_is_disabled(cx) {
            return;
        }
        self.refresh_pending = true;
        cx.notify();
        self.binding
            .call(FgiRequest::Refresh, cx, Self::apply_refresh_result);
    }

    /// Applies the answer to a load or a retry.
    ///
    /// A failure is terminal, so an answer that arrives after one is dropped: the
    /// recovery reports `Connecting` before it asks for anything, which is what
    /// makes the next answer one this pane still wants.
    fn apply_rpc_result(&mut self, result: WidgetRpcResult<FgiProtocol>, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        match result {
            Ok(FgiResponse::StreamStarted | FgiResponse::RefreshQueued) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(FgiFailure::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    /// Applies the answer to a refresh command.
    ///
    /// An accepted command stays pending until the reading it asked for is reported:
    /// the acknowledgement and the reading are two different messages, and the
    /// service refuses a second command in between.
    fn apply_refresh_result(
        &mut self,
        result: WidgetRpcResult<FgiProtocol>,
        cx: &mut Context<Self>,
    ) {
        if result.is_err() {
            self.refresh_pending = false;
        }
        self.apply_rpc_result(result, cx);
    }

    /// Applies the failure the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a failure is drawn by the
    /// framework around the body, so reporting it is enough.
    fn apply_reliable(
        &mut self,
        key: FgiStreamKey,
        message: Arc<FgiStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let FgiStreamMessage::Failed { error } = message.as_ref() else {
            return false;
        };
        self.refresh_pending = false;
        let recovery = self.recovery.clone();
        self.status.report(
            WidgetState::failed(Arc::new(FgiFailure::Fetch(error.clone())), recovery),
            cx,
        );
        false
    }

    /// Applies the snapshot the stream published.
    ///
    /// Returns whether anything a frame draws changed, which is what the framework
    /// turns into a repaint: the body is cached, and an answer that folds into the
    /// same pixels is not worth a frame. The comparison is the snapshot's
    /// [`same_display`](FgiSnapshot::same_display) rather than the whole answer,
    /// because the collection time is never drawn.
    ///
    /// Two things still count as a change even when a frame would draw the same: an
    /// answer ends a refresh the header is waiting for (the control re-enables), and
    /// a pane a retry left in `Connecting` has to report `Ready` on the answer even
    /// though the reading it kept draws the same. Both of those are the pane's own
    /// state rather than the drawing, so they are checked before the snapshot is
    /// discarded as a no-op.
    fn apply_latest(
        &mut self,
        key: FgiStreamKey,
        message: Arc<FgiStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let FgiStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        let no_op = self
            .snapshot
            .as_ref()
            .is_some_and(|held| held.same_display(snapshot))
            && !self.refresh_pending
            && self.status.state(cx) == WidgetState::Ready;
        if no_op {
            // The answer neither moves the drawing nor ends anything the pane is
            // waiting for, so the allocation it already holds is the one the body
            // keeps drawing from.
            return false;
        }
        self.snapshot = Some(snapshot.clone());
        self.refresh_pending = false;
        self.status.report(WidgetState::Ready, cx);

        true
    }

    /// The header's right group: the control that reads the answer again.
    ///
    /// The button is disabled rather than absent for the states that cannot refresh,
    /// so the header does not reflow when the reading arrives.
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
                    .debug_selector(|| "TEST_CRYPTO_FGI_REFRESH".into())
                    .child(
                        Button::new(ElementId::Name(SharedString::new_static(
                            "CryptoFgiRefresh",
                        )))
                        .ghost()
                        .small()
                        .disabled(disabled)
                        .aria_label(t!(cx, "widget.crypto_fgi.refresh"))
                        .child(Icon::new(IconKind::ArrowsClockwise).size_3())
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

    /// The reading's card: the gauge, the four history rows and the two yearly
    /// extremes.
    fn card(&self, snapshot: &FgiSnapshot, cx: &Context<Self>) -> AnyElement {
        let level = Level::of(snapshot.current);

        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(
                div()
                    .debug_selector(|| "TEST_CRYPTO_FGI_GAUGE".into())
                    .flex_1()
                    .min_h_0()
                    .child(
                        self.gauge
                            .element(snapshot.current, t!(cx, level.key()), cx),
                    ),
            )
            .child(self.section(
                t!(cx, "widget.crypto_fgi.title"),
                "widget.crypto_fgi.info_history",
                "CryptoFgiHistoryInfo",
                cx,
            ))
            .child(self.history_rows(snapshot, cx))
            .child(self.section(
                t!(cx, "widget.crypto_fgi.yearly_extremes"),
                "widget.crypto_fgi.info_yearly",
                "CryptoFgiYearlyInfo",
                cx,
            ))
            .child(self.extreme_rows(snapshot, cx))
            .into_any_element()
    }

    /// One section heading with the note that explains it.
    fn section(
        &self,
        title: SharedString,
        info_key: &'static str,
        info_id: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();

        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(title),
            )
            .child(
                // The trigger itself must not occlude: an occluding trigger never
                // hears the hover that opens the note.
                Tooltip::new(
                    ElementId::Name(SharedString::new_static(info_id)),
                    Icon::new(IconKind::WarningCircle)
                        .size_3()
                        .text_color(theme.roles.fg.subtle.hsla),
                )
                .content(div().max_w(px(320.0)).text_xs().child(t!(cx, info_key))),
            )
            .into_any_element()
    }

    /// Yesterday, last week, last month and last year.
    fn history_rows(&self, snapshot: &FgiSnapshot, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let label_color = theme.roles.fg.muted.hsla;
        let value_color = theme.roles.fg.default.hsla;
        let rows = [
            (
                "widget.crypto_fgi.yesterday",
                "TEST_CRYPTO_FGI_ROW_YESTERDAY",
                MISSING_YESTERDAY,
                snapshot.yesterday.as_ref(),
            ),
            (
                "widget.crypto_fgi.last_week",
                "TEST_CRYPTO_FGI_ROW_LAST_WEEK",
                MISSING_LAST_WEEK,
                snapshot.last_week.as_ref(),
            ),
            (
                "widget.crypto_fgi.last_month",
                "TEST_CRYPTO_FGI_ROW_LAST_MONTH",
                MISSING_LAST_MONTH,
                snapshot.last_month.as_ref(),
            ),
            (
                "widget.crypto_fgi.last_year",
                "TEST_CRYPTO_FGI_ROW_LAST_YEAR",
                MISSING_LAST_YEAR,
                snapshot.last_year.as_ref(),
            ),
        ];

        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(rows.map(|(key, selector, missing, row)| {
                value_row(
                    selector,
                    missing,
                    t!(cx, key),
                    row,
                    label_color,
                    value_color,
                )
            }))
            .into_any_element()
    }

    /// The highest and the lowest reading of the past year, each with the day it
    /// happened and the level it sat in.
    fn extreme_rows(&self, snapshot: &FgiSnapshot, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let label_color = theme.roles.fg.muted.hsla;
        let value_color = theme.roles.fg.default.hsla;
        let rows = [
            (
                "widget.crypto_fgi.yearly_high",
                "TEST_CRYPTO_FGI_ROW_YEARLY_HIGH",
                MISSING_YEARLY_HIGH,
                snapshot.yearly_high.as_ref(),
            ),
            (
                "widget.crypto_fgi.yearly_low",
                "TEST_CRYPTO_FGI_ROW_YEARLY_LOW",
                MISSING_YEARLY_LOW,
                snapshot.yearly_low.as_ref(),
            ),
        ];

        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(rows.map(|(key, selector, missing, row)| {
                let level = row.map(|row| Level::of(row.score));
                let mut value = div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(value_color)
                    .child(value_text(row));
                if row.is_none() {
                    value = value.debug_selector(move || missing.into());
                }
                div()
                    .debug_selector(move || selector.into())
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(label_color)
                            .child(yearly_label(key, row, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(level.map(|level| {
                                div()
                                    .text_xs()
                                    .text_color(level_color(theme, level))
                                    .child(t!(cx, level.key()))
                            }))
                            .child(value),
                    )
                    .into_any_element()
            }))
            .into_any_element()
    }
}

impl Render for CryptoFgiWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the ready
        // state and nothing else. Everything the pane is *doing* is drawn by the
        // framework around this.
        let Some(snapshot) = self.snapshot.as_ref() else {
            return div().size_full().min_h_0().into_any_element();
        };
        let body = if snapshot.is_empty() {
            // An uncollected snapshot is empty rather than broken, and a blank body
            // would read as a pane that is still loading.
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    ElementId::Name(SharedString::new_static("CryptoFgiEmpty")),
                    t!(cx, "common.no_data"),
                ))
                .into_any_element()
        } else {
            self.card(snapshot, cx)
        };

        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(body)
            .into_any_element()
    }
}

/// One row of the history: what it is, then the number or a dash.
///
/// A row without a reading marks its value with `missing`, so a test can assert from
/// the painted frame that the dash was drawn and which row drew it.
fn value_row(
    selector: &'static str,
    missing: &'static str,
    label: SharedString,
    row: Option<&FgiRow>,
    label_color: Hsla,
    value_color: Hsla,
) -> AnyElement {
    let mut value = div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(value_color)
        .child(value_text(row));

    if row.is_none() {
        value = value.debug_selector(move || missing.into());
    }

    div()
        .debug_selector(move || selector.into())
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(div().text_sm().text_color(label_color).child(label))
        .child(value)
        .into_any_element()
}

/// The label of a yearly row, naming the day the reading happened on.
///
/// A row without a reading names the dash as its day rather than dropping the
/// parenthesis: `Yearly High (—)` says the row exists and has no value, while a
/// second label without a date would be a second message to translate.
fn yearly_label(key: &'static str, row: Option<&FgiRow>, cx: &App) -> SharedString {
    t!(cx, key, date = date_text(row))
}

/// The value of a row, or the dash that says there is none.
fn value_text(row: Option<&FgiRow>) -> SharedString {
    row.map_or_else(
        || SharedString::new_static(MISSING),
        |row| SharedString::new(format_smolstr!("{}", row.score).as_str()),
    )
}

/// The day a row happened on, or the dash that says there is no reading.
fn date_text(row: Option<&FgiRow>) -> SharedString {
    row.map_or_else(
        || SharedString::new_static(MISSING),
        |row| SharedString::new(row.date.as_str()),
    )
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use gpui_kit::{
        AppContext, BorrowAppContext, Entity, IntoElement, Modifiers, Render, TestAppContext,
        VisualTestContext, Window, div,
    };
    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use widget::{protocol::RequestId, rpc::RpcCallError, status::Failure};

    use super::*;
    use crate::test_support::{
        ACCOUNT, COLLECTED_AT, FakeSource, FetchPlan, empty_snapshot, row, snapshot,
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
    ///
    /// Each is an application-wide global an application installs exactly once, so a
    /// test that needs a second pane in one application installs them once and scopes
    /// the panes with [`sign_in`] rather than installing them again.
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
    /// Two executors are involved: the pane sends its commands from GPUI's own, and
    /// the service answers on the application runtime. So both are given a turn —
    /// the pane first, because a command that has not been sent is a service with
    /// nothing to answer.
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

    /// A pane the framework would have built, driven by the source a test answers
    /// through.
    fn pane_with_source<S>(source: S, cx: &mut Context<CryptoFgiWidget>) -> CryptoFgiWidget
    where
        S: FgiSource,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        CryptoFgiWidget::new_with_source(source, status, recovery, cx)
    }

    fn pane_with(
        cx: &mut TestAppContext,
        account: Option<&str>,
        source: FakeSource,
    ) -> Entity<CryptoFgiWidget> {
        cx.update(install);
        pane_over(cx, account, source)
    }

    /// The same pane over the state [`install`] already put in place.
    fn pane_over(
        cx: &mut TestAppContext,
        account: Option<&str>,
        source: FakeSource,
    ) -> Entity<CryptoFgiWidget> {
        cx.executor().allow_parking();
        cx.update(|cx| {
            sign_in(cx, account);
            cx.new(|cx| {
                let mut pane = pane_with_source(source, cx);
                pane.load(cx);
                pane
            })
        })
    }

    fn key() -> FgiStreamKey {
        FgiStreamKey {
            account_id: SmolStr::new_static(ACCOUNT),
        }
    }

    /// The reading of `snapshot(current)` as a later poll collected it.
    ///
    /// The collector publishes again every minute, so an index that has not moved
    /// arrives as a new answer carrying the same drawing and a new collection time.
    fn recollected(current: u8, collected_at: i64) -> Arc<FgiSnapshot> {
        Arc::new(FgiSnapshot {
            collected_at,
            ..(*snapshot(current)).clone()
        })
    }

    /// Reports a snapshot as the stream would.
    ///
    /// The framework's conditional stream handler asks the pane for a frame exactly
    /// when the handler reports that one changed — it calls `notify` only for a
    /// `true` answer. Mirroring that one line is what lets a test count the frames
    /// the pane asks for, rather than only the answer its handler returns.
    fn report_snapshot(
        pane: &Entity<CryptoFgiWidget>,
        snapshot: Arc<FgiSnapshot>,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, context| {
            let changed = pane.apply_latest(
                key(),
                Arc::new(FgiStreamMessage::Snapshot(snapshot)),
                context,
            );
            if changed {
                context.notify();
            }
            changed
        })
    }

    /// Reports the failure the stream would.
    ///
    /// A failure is drawn by the framework around the body, so the handler answers
    /// that nothing a frame draws changed and the pane is not notified; the state
    /// cell it writes is what the surface redraws from.
    fn report_failure(pane: &Entity<CryptoFgiWidget>, cx: &mut App) -> bool {
        pane.update(cx, |pane, context| {
            pane.apply_reliable(
                key(),
                Arc::new(FgiStreamMessage::Failed {
                    error: FgiFetchError::Service(KytrixServiceError::Disconnected),
                }),
                context,
            )
        })
    }

    /// The failure the pane published.
    fn published_failure(pane: &Entity<CryptoFgiWidget>, cx: &App) -> Failure {
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .cloned()
            .expect("a failure should be published")
    }

    #[gpui_kit::test]
    fn a_pane_without_an_account_should_ask_for_one(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let pane = pane_with(cx, None, source.clone());

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.crypto_fgi.logged_out")),
                "a pane nobody is signed in for says what it is waiting for"
            );
            assert!(!pane.read(cx).has_content());
            assert!(
                pane.read(cx).current_key().is_none(),
                "there is nothing to read without an account"
            );
            assert_eq!(
                source.fetch_count(),
                0,
                "a pane nobody is signed in for fetches nothing"
            );
        });
    }

    #[gpui_kit::test]
    fn an_account_should_start_the_stream_with_its_first_fetch(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let pane = pane_with(cx, Some(ACCOUNT), source.clone());
        settle_until(cx, "the first fetch", |_| source.fetch_count() == 1);

        cx.update(|cx| {
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
            assert!(!pane.read(cx).has_content());
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "a pane waiting for its first answer offers no refresh"
            );
        });
    }

    #[gpui_kit::test]
    fn a_snapshot_should_be_drawn_as_content(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            let changed = report_snapshot(&pane, snapshot(46), cx);

            assert!(changed, "accepting an answer is a change worth a frame");
            assert!(pane.read(cx).has_content());
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "an idle pane with an answer can read again on demand"
            );
        });
    }

    /// The reading the pane draws is what it compares: a later poll of an index
    /// that has not moved carries a new collection time but the same pixels, so it
    /// is not a frame and the allocation it replaced is kept.
    ///
    /// The frames are counted on the entity rather than inferred from the handler's
    /// answer: the identical answer is applied in an update of its own, and the pane
    /// has to stay silent — no notification, so no frame.
    #[gpui_kit::test]
    fn a_snapshot_that_changed_nothing_should_not_ask_for_a_frame(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));
        let held = snapshot(46);
        let polled_again = recollected(46, COLLECTED_AT + 60_000);
        assert_ne!(
            held.as_ref(),
            polled_again.as_ref(),
            "the second answer differs: only its collection time moved"
        );
        let notifications = Rc::new(Cell::new(0));
        let counted = notifications.clone();
        let _observed = cx.update(|cx| {
            cx.observe(&pane, move |_, _| {
                counted.set(counted.get() + 1);
            })
        });

        cx.update(|cx| {
            assert!(
                report_snapshot(&pane, held.clone(), cx),
                "the first answer is a change worth a frame"
            );
        });
        assert_eq!(notifications.get(), 1, "the first answer asks for a frame");

        cx.update(|cx| {
            let changed = report_snapshot(&pane, polled_again, cx);

            assert!(
                !changed,
                "an answer that folds into the same pixels is not worth a frame"
            );
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
            assert!(
                Arc::ptr_eq(
                    &held,
                    pane.read(cx).snapshot.as_ref().expect("the answer is kept")
                ),
                "the allocation of an unchanged answer is the one that is kept"
            );
        });
        assert_eq!(
            notifications.get(),
            1,
            "the same reading polled again notifies nobody"
        );
    }

    /// An uncollected answer draws 「暂无数据」, so the first collected answer is a
    /// change even when its rows are still the empty defaults: the empty state goes
    /// away, and the pane has to keep the collected answer rather than the old one.
    #[gpui_kit::test]
    fn a_collected_answer_should_replace_an_empty_one_with_the_same_rows(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            let uncollected = empty_snapshot();
            assert!(
                report_snapshot(&pane, uncollected.clone(), cx),
                "the first answer is a change worth a frame"
            );
            assert!(
                pane.read(cx)
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.is_empty()),
                "the pane holds the uncollected answer"
            );

            let collected = Arc::new(FgiSnapshot {
                collected_at: COLLECTED_AT,
                ..(*uncollected).clone()
            });
            assert!(
                report_snapshot(&pane, collected.clone(), cx),
                "the empty state disappearing is a change worth a frame"
            );
            assert!(
                Arc::ptr_eq(
                    &collected,
                    pane.read(cx).snapshot.as_ref().expect("the answer is kept")
                ),
                "the collected answer is the one the body draws from"
            );
        });
    }

    /// A refresh is one command and one answer: even when the answer draws exactly
    /// what is already on screen, it still ends the wait and notifies, because that
    /// is what re-enables the header's control.
    #[gpui_kit::test]
    fn an_unchanged_answer_should_end_a_pending_refresh(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));
        cx.update(|cx| {
            assert!(report_snapshot(&pane, snapshot(46), cx));
            pane.update(cx, |pane, cx| pane.refresh(cx));
            assert!(
                pane.read(cx).refresh_pending,
                "the click leaves a refresh waiting for its answer"
            );
        });

        let notifications = Rc::new(Cell::new(0));
        let counted = notifications.clone();
        let _observed = cx.update(|cx| {
            cx.observe(&pane, move |_, _| {
                counted.set(counted.get() + 1);
            })
        });

        cx.update(|cx| {
            assert!(
                report_snapshot(&pane, snapshot(46), cx),
                "the answer a refresh waited for is a change even when it draws the same"
            );
            assert!(
                !pane.read(cx).refresh_pending,
                "the answer ends the wait the click started"
            );
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "the header's control becomes live again"
            );
        });
        assert_eq!(
            notifications.get(),
            1,
            "the answer notifies even though nothing drawn changed"
        );
    }

    /// A retry keeps the reading and reports `Connecting`, so the answer that draws
    /// the same is still the answer that finishes it: the pane reports `Ready` and
    /// asks for a frame rather than staying in a state nothing else would end.
    #[gpui_kit::test]
    fn an_unchanged_answer_should_still_finish_a_retry(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            assert!(report_snapshot(&pane, snapshot(46), cx));
            report_failure(&pane, cx);
            pane.update(cx, |pane, cx| pane.load(cx));
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Connecting,
                "a recovery reports Connecting over the reading it kept"
            );
            assert!(
                pane.read(cx).has_content(),
                "the reading stays on screen through the retry"
            );

            assert!(
                report_snapshot(&pane, snapshot(46), cx),
                "the retry's answer is a change even though it draws the same"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "the answer ends the Connecting the retry reported"
            );
        });
    }

    /// A missing row is drawn as a dash rather than as a missing snapshot: the pane
    /// is content, the gauge is drawn, and the six rows are in the painted frame with
    /// a dash where a reading is absent.
    ///
    /// The rows are asserted through the debug selectors of a real frame: a row that
    /// stopped being drawn, or a dash that stopped being painted, fails here even
    /// though the text helpers below still answer correctly.
    #[gpui_kit::test]
    fn a_snapshot_with_missing_rows_should_draw_six_rows_and_dashes(cx: &mut TestAppContext) {
        cx.update(install);
        let pane = pane_over(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));
        let (harness, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness::new(pane.clone()));
        let mut partial = (*snapshot(46)).clone();
        partial.yesterday = None;
        partial.yearly_high = None;

        cx.update(|_, cx| {
            assert!(
                report_snapshot(&pane, Arc::new(partial), cx),
                "a partly empty answer is still content"
            );
            assert!(pane.read(cx).has_content());
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "a missing row is not a missing snapshot"
            );
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| harness.read(cx).frames.get()) > 0,
            "the pane has to be drawn before its rows can be read"
        );

        for selector in [
            "TEST_CRYPTO_FGI_ROW_YESTERDAY",
            "TEST_CRYPTO_FGI_ROW_LAST_WEEK",
            "TEST_CRYPTO_FGI_ROW_LAST_MONTH",
            "TEST_CRYPTO_FGI_ROW_LAST_YEAR",
            "TEST_CRYPTO_FGI_ROW_YEARLY_HIGH",
            "TEST_CRYPTO_FGI_ROW_YEARLY_LOW",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} is one of the six rows the pane draws"
            );
        }
        for selector in [MISSING_YESTERDAY, MISSING_YEARLY_HIGH] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector}: a row without a reading paints its dash"
            );
        }
        for selector in [
            MISSING_LAST_WEEK,
            MISSING_LAST_MONTH,
            MISSING_LAST_YEAR,
            MISSING_YEARLY_LOW,
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{selector}: a row with a reading paints no dash"
            );
        }
        assert!(
            cx.debug_bounds("TEST_CRYPTO_FGI_GAUGE").is_some(),
            "the gauge is drawn beside the rows"
        );

        cx.update(|_, cx| {
            assert_eq!(value_text(None).as_ref(), "—");
            assert_eq!(date_text(None).as_ref(), "—");
            assert_eq!(value_text(Some(&row("2026-10-23", 45))).as_ref(), "45");
            assert_eq!(
                date_text(Some(&row("2026-10-23", 45))).as_ref(),
                "2026-10-23"
            );
            assert_eq!(
                yearly_label("widget.crypto_fgi.yearly_high", None, cx).as_ref(),
                "Yearly High (—)",
                "a missing day is named by the same dash as a missing value"
            );
            assert_eq!(
                yearly_label(
                    "widget.crypto_fgi.yearly_high",
                    Some(&row("2025-11-20", 88)),
                    cx
                )
                .as_ref(),
                "Yearly High (2025-11-20)"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_should_keep_the_reading_and_offer_the_framework_recovery(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            report_snapshot(&pane, snapshot(46), cx);
            pane.update(cx, |pane, cx| pane.refresh(cx));
            assert!(
                pane.read(cx).refresh_pending,
                "a click leaves a refresh waiting for its reading"
            );
            report_failure(&pane, cx);

            assert!(pane.read(cx).status.is_failed(cx));
            assert!(
                !pane.read(cx).refresh_pending,
                "the failure the refresh was waiting for ends the wait"
            );
            let failure = published_failure(&pane, cx);
            assert!(
                !failure.text(cx).is_empty(),
                "a failure has to say what went wrong"
            );
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "a failure always offers the framework's way out"
            );
            assert!(pane.read(cx).has_content(), "the reading stays on screen");
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "a failed pane offers no refresh of its own"
            );
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.load(cx));

            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
            assert!(
                pane.read(cx).has_content(),
                "a retry keeps the reading the user was looking at"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_should_be_terminal_for_late_messages(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            report_snapshot(&pane, snapshot(46), cx);
            report_failure(&pane, cx);

            let changed = report_snapshot(&pane, snapshot(40), cx);

            assert!(!changed, "a failed pane takes no more snapshots");
            assert!(pane.read(cx).status.is_failed(cx));
            assert_eq!(
                pane.read(cx)
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.current),
                Some(46),
                "the reading the user was looking at is what stays"
            );
        });
    }

    #[gpui_kit::test]
    fn a_snapshot_from_another_account_should_be_ignored(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            let changed = pane.update(cx, |pane, context| {
                pane.apply_latest(
                    FgiStreamKey {
                        account_id: SmolStr::new_static("another-account"),
                    },
                    Arc::new(FgiStreamMessage::Snapshot(snapshot(46))),
                    context,
                )
            });

            assert!(!changed, "another account's answer is not this pane's");
            assert!(!pane.read(cx).has_content());
        });
    }

    #[gpui_kit::test]
    fn another_account_should_take_the_reading_with_it(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            report_snapshot(&pane, snapshot(46), cx);
            pane.update(cx, |pane, cx| {
                pane.reconcile_account(Some(SmolStr::new_static("second-account")), cx);
            });

            assert!(
                !pane.read(cx).has_content(),
                "the answer was served to the account that just left"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Connecting,
                "another account is another read"
            );

            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            assert!(!pane.read(cx).has_content());
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.crypto_fgi.logged_out")),
                "signing out asks for an account rather than reporting a failure"
            );
            assert!(pane.read(cx).current_key().is_none());
        });
    }

    /// Another account is another read, and the pane hears about it from the
    /// settings rather than from a reload: what the new account is owed is fetched
    /// for it, and signing out takes the reading away.
    #[gpui_kit::test]
    fn an_account_change_should_read_the_new_account(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Snapshot(40)]);
        let pane = pane_with(cx, Some(ACCOUNT), source.clone());
        settle_until(cx, "the pane to read", |cx| pane.read(cx).has_content());
        assert_eq!(source.fetch_count(), 1);

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
        cx.run_until_parked();

        settle_until(cx, "the replacement read", |cx| {
            pane.read(cx).account_id.as_deref() == Some("someone-else")
                && pane.read(cx).has_content()
        });
        assert_eq!(source.fetch_count(), 2);
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx)
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.current),
                Some(40),
                "the new account is read for itself"
            );
        });

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.account = None, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::waiting(Notice::message("widget.crypto_fgi.logged_out"))
            );
            assert!(
                !pane.read(cx).has_content(),
                "signing out takes the reading with it"
            );
        });
    }

    /// A refresh is one command and one answer, and the control is closed between
    /// them: the service refuses a second command while one is in flight, so a
    /// control that forwarded a fast double click would answer it with a banner.
    #[gpui_kit::test]
    fn a_refresh_should_close_the_control_until_the_answer_arrives(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Snapshot(40)]);
        let pane = pane_with(cx, Some(ACCOUNT), source);
        settle_until(cx, "the first answer", |cx| {
            pane.read(cx).status.state(cx) == WidgetState::Ready
        });

        cx.update(|cx| {
            assert!(!pane.read(cx).refresh_is_disabled(cx));
            pane.update(cx, |pane, cx| pane.refresh(cx));

            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "the click closes the window itself, before anything has answered"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "a refresh is not a state of its own: the reading on screen is still ready"
            );
        });

        settle_until(cx, "the refreshed answer", |cx| {
            pane.read(cx)
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.current == 40)
        });
        cx.update(|cx| {
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
            assert!(
                !pane.read(cx).refresh_is_disabled(cx),
                "the answer reopens the control"
            );
        });

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(pane.refresh_pending, "the click puts a refresh in flight");
                pane.reconcile_account(Some(SmolStr::new_static("someone-else")), cx);
                assert!(
                    !pane.refresh_pending,
                    "the command belonged to the stream the account change replaced"
                );
            });
        });
    }

    #[gpui_kit::test]
    fn a_refused_refresh_should_be_reported_as_a_failure(cx: &mut TestAppContext) {
        let pane = pane_with(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));

        cx.update(|cx| {
            report_snapshot(&pane, snapshot(46), cx);
            pane.update(cx, |pane, cx| {
                pane.refresh(cx);
                assert!(pane.refresh_is_disabled(cx));
                pane.apply_refresh_result(
                    Err(RpcCallError::Service {
                        request_id: RequestId::new(1),
                        source: FgiServiceError::StreamBusy,
                    }),
                    cx,
                );
            });

            assert!(pane.read(cx).status.is_failed(cx));
            assert!(
                !pane.read(cx).refresh_pending,
                "a refused command is not one to keep waiting for"
            );
            assert!(
                pane.read(cx).refresh_is_disabled(cx),
                "a failed pane offers no refresh of its own"
            );
            let failure = published_failure(&pane, cx);
            assert!(!failure.text(cx).is_empty());
            assert_eq!(failure.action().label_key(), "common.retry");
        });
    }

    /// A level's name and the notes beside the headings are catalogue references
    /// resolved on the frame that draws them, and the gauge keeps its prepared text
    /// until that text, the reading or the canvas changes — so a language change has
    /// to redraw the pane and rebuild that prepared frame.
    ///
    /// The gauge's prepared level is read back because a painted frame's text is not
    /// otherwise readable: it is the exact string the last frame drew. Reading `t!`
    /// alone would prove nothing about the render.
    #[gpui_kit::test]
    fn a_language_change_should_redraw_the_level_name_in_the_new_language(cx: &mut TestAppContext) {
        cx.update(install);
        let pane = pane_over(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));
        let (harness, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness::new(pane.clone()));

        cx.update(|_, cx| {
            assert!(report_snapshot(&pane, snapshot(66), cx));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let (english_level, english_note, frames_before) = cx.update(|_, cx| {
            (
                t!(cx, Level::Greed.key()),
                t!(cx, "widget.crypto_fgi.info_history"),
                harness.read(cx).frames.get(),
            )
        });
        assert_eq!(english_level.as_ref(), "Greed");
        assert!(!english_note.is_empty());
        cx.update(|_, cx| {
            assert_eq!(
                pane.read(cx)
                    .gauge
                    .prepared_level()
                    .expect("a drawn gauge prepares its text")
                    .as_ref(),
                english_level.as_ref(),
                "the gauge draws the level in the language of the frame that built it"
            );
        });

        cx.update(|_, cx| {
            cx.set_global(LanguageCode::ZhCn);
            cx.refresh_windows();
        });
        cx.run_until_parked();

        assert!(
            cx.update(|_, cx| harness.read(cx).frames.get()) > frames_before,
            "a language change asks the window for a new frame"
        );
        cx.update(|_, cx| {
            assert_eq!(
                pane.read(cx)
                    .gauge
                    .prepared_level()
                    .expect("the redrawn pane prepares its text again")
                    .as_ref(),
                "贪婪",
                "the gauge of the redrawn frame names the level in the new language"
            );
            assert_eq!(t!(cx, Level::Greed.key()).as_ref(), "贪婪");
            assert_ne!(
                t!(cx, "widget.crypto_fgi.info_history"),
                english_note,
                "the note beside the heading is resolved in the language showing now"
            );
            assert_eq!(
                yearly_label("widget.crypto_fgi.yearly_high", None, cx).as_ref(),
                "年度最高值（—）"
            );
        });
    }

    /// One pane mounted in a window of its own, which is what paints its body.
    struct BodyHarness {
        pane: Entity<CryptoFgiWidget>,
        /// How many frames the window has drawn the pane in.
        frames: Rc<Cell<usize>>,
    }

    impl BodyHarness {
        fn new(pane: Entity<CryptoFgiWidget>) -> Self {
            Self {
                pane,
                frames: Rc::new(Cell::new(0)),
            }
        }
    }

    impl Render for BodyHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.frames.set(self.frames.get() + 1);
            div().size_full().child(self.pane.clone())
        }
    }

    /// Nothing collected yet is a healthy empty answer: the body says so, and no
    /// gauge is drawn from a reading that does not exist.
    #[gpui_kit::test]
    fn an_empty_snapshot_should_say_there_is_no_data_instead_of_drawing_a_gauge(
        cx: &mut TestAppContext,
    ) {
        cx.update(install);
        let pane = pane_over(cx, Some(ACCOUNT), FakeSource::new([FetchPlan::Pending]));
        let (_, cx): (Entity<BodyHarness>, &mut VisualTestContext) =
            cx.add_window_view(|_, _| BodyHarness::new(pane.clone()));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("EMPTY_STATE").is_none(),
            "a pane with no answer yet has nothing to say about data"
        );

        cx.update(|_, cx| {
            report_snapshot(&pane, snapshot(46), cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_CRYPTO_FGI_GAUGE").is_some(),
            "a collected reading is drawn as a gauge"
        );
        assert!(cx.debug_bounds("EMPTY_STATE").is_none());

        cx.update(|_, cx| {
            report_snapshot(&pane, snapshot(0), cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("TEST_CRYPTO_FGI_GAUGE").is_some(),
            "a collected zero is a reading at the bottom of the scale, not an empty snapshot"
        );
        assert!(cx.debug_bounds("EMPTY_STATE").is_none());

        cx.update(|_, cx| {
            assert!(
                report_snapshot(&pane, empty_snapshot(), cx),
                "an uncollected snapshot is content of its own"
            );
            assert!(
                pane.read(cx).has_content(),
                "an empty snapshot must not turn the body into a placeholder"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "an uncollected snapshot is a healthy answer, not a failure or a wait"
            );
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("EMPTY_STATE").is_some(),
            "an uncollected snapshot says there is no data"
        );
        assert!(
            cx.debug_bounds("TEST_CRYPTO_FGI_GAUGE").is_none(),
            "nothing is drawn from a reading nobody collected"
        );
    }

    /// One pane's header row, mounted in a window of its own.
    ///
    /// The pane chrome is what lays a header out in the application, and it is not
    /// part of this crate; the harness stands in for it, so the control is built and
    /// painted exactly the way a frame builds it.
    struct HeaderHarness {
        pane: Entity<CryptoFgiWidget>,
    }

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let header = self.pane.update(cx, |pane, cx| pane.header(window, cx));
            div()
                .size_full()
                .flex()
                .child(
                    div()
                        .debug_selector(|| "TEST_CRYPTO_FGI_HEADER_LEFT".into())
                        .children(header.left),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .debug_selector(|| "TEST_CRYPTO_FGI_HEADER_RIGHT".into())
                        .children(header.right),
                )
        }
    }

    /// Mounts one pane's header row in a window of its own, over `source`.
    fn header_window(
        cx: &mut TestAppContext,
        source: FakeSource,
    ) -> (Entity<HeaderHarness>, &mut VisualTestContext) {
        cx.update(install);
        cx.executor().allow_parking();
        cx.update(|cx| sign_in(cx, Some(ACCOUNT)));
        cx.add_window_view(|_, cx| {
            let pane = cx.new(|cx| {
                let mut pane = pane_with_source(source, cx);
                pane.load(cx);
                pane
            });
            HeaderHarness { pane }
        })
    }

    /// The click sends a refresh command, not a reload: the pane stays `Ready`,
    /// keeps the reading on screen and waits for the answer it asked for.
    ///
    /// A header that called `load` would show `Connecting` and drop the reading, so
    /// the assertions are made while the fetch is still in flight.
    #[gpui_kit::test]
    fn the_header_should_refresh_without_starting_over(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Held(52)]);
        let (harness, cx) = header_window(cx, source.clone());
        settle_until(cx, "the first answer", |cx| {
            harness.read(cx).pane.read(cx).status.state(cx) == WidgetState::Ready
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let button = cx
            .debug_bounds("TEST_CRYPTO_FGI_REFRESH")
            .expect("the header should draw its refresh control");
        cx.simulate_click(button.center(), Modifiers::none());
        settle_until(cx, "the refresh to fetch", |_| source.fetch_count() == 2);

        cx.update(|_, cx| {
            let pane = harness.read(cx).pane.read(cx);

            assert_eq!(
                pane.status.state(cx),
                WidgetState::Ready,
                "a refresh is not a state of its own: the reading on screen is still ready"
            );
            assert!(
                pane.refresh_pending,
                "the click leaves a refresh waiting for its reading"
            );
            assert!(
                pane.has_content(),
                "a refresh does not clear the reading on screen"
            );
            assert_eq!(
                pane.snapshot.as_ref().map(|snapshot| snapshot.current),
                Some(46),
                "the reading the user is looking at stays while its replacement is read"
            );
        });

        source.release();
        settle_until(cx, "the refreshed answer", |cx| {
            harness
                .read(cx)
                .pane
                .read(cx)
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.current == 52)
        });
        cx.update(|_, cx| {
            let pane = harness.read(cx).pane.read(cx);

            assert!(
                !pane.refresh_pending,
                "the answer the refresh waited for ends the wait"
            );
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
        });
        assert_eq!(
            source.fetch_count(),
            2,
            "a refresh fetches once; it does not restart the stream"
        );
    }

    /// The control is disabled while the first answer is still outstanding, and a
    /// disabled control carries no click handler at all.
    #[gpui_kit::test]
    fn a_disabled_refresh_should_not_forward_a_click(cx: &mut TestAppContext) {
        let source = FakeSource::new([FetchPlan::Pending]);
        let (harness, cx) = header_window(cx, source.clone());
        settle_until(cx, "the first fetch", |_| source.fetch_count() == 1);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let button = cx
            .debug_bounds("TEST_CRYPTO_FGI_REFRESH")
            .expect("the header draws its refresh control even while it is disabled");
        cx.simulate_click(button.center(), Modifiers::none());
        cx.run_until_parked();

        cx.update(|_, cx| {
            assert_eq!(
                source.fetch_count(),
                1,
                "a disabled control sends nothing to the service"
            );
            assert_eq!(
                harness.read(cx).pane.read(cx).status.state(cx),
                WidgetState::Connecting
            );
        });
    }
}
