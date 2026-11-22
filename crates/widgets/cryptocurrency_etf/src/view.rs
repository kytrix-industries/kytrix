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
//! retried on a timer either — the settings snapshot re-scopes the pane the moment
//! the user signs in (see `sync`).
//!
//! # The body is a chart, and the chart is a canvas
//!
//! The engine's only self-drawing entry point is a `Canvas` node, and the chart is the
//! one thing in this pane that needs it: its labels move with its data, and its tooltip
//! has to be clipped to the plot. [`WidgetView::body`] therefore builds a row of
//! readings (ordinary nodes, `bind_text` over the pane's signals) and the chart's canvas
//! node; after that, `sync` rebuilds the canvas when what it would draw moved
//! ([`CryptocurrencyEtfChart`]). A chart that is dropped and re-created — the account
//! behind the answer changed — is rebuilt into the same host node, because the body is
//! built only once.
//!
//! # The header is four controls and a countdown
//!
//! The right group holds the refresh, the asset, the fund and the window — the order a
//! reader narrows the chart down. Three of them are `kit::select` pickers and the
//! refresh is a `kit::button`; the countdown is a `bind_text` node beside the button
//! rather than the button's own label, because a kit button's label is chosen when it is
//! built and a countdown moves once a second (registered as a W3c difference: the old
//! control drew the time left inside the button).
//!
//! Every control's key comes from [`WidgetCx::unique_key`], so two panes of this widget
//! never share one handler (AGENTS §1 rule 11). A picker's *copy* is chosen when it is
//! built, so a language switch rebuilds the row; what it *offers* is its item list
//! (`set_items`), which a snapshot moves without a rebuild. The choice a control reports
//! is written into a request signal and applied by the next `sync`, because a control's
//! handler owns no widget context (AGENTS §4.2).
//!
//! # Why a reply is matched by request identity
//!
//! Two commands share one inbox. Draining it happens before anything in the frame can
//! replace a command, so an answer that was delivered before the replacement is applied
//! to the command it belongs to; on top of that a reply is only accepted when its
//! framework `RequestId` is newer than the last one this pane consumed, which is what
//! keeps a reply the pane has already answered from clearing the command now in flight.

use std::{cell::Cell, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    event::Event,
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};
use widget::{
    binding::{Inbound, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    util::{format_countdown, unix_millis},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use crate::{
    chart::{CryptocurrencyEtfChart, Metrics},
    model::{
        CryptocurrencyEtfAsset, CryptocurrencyEtfFundSelection, CryptocurrencyEtfParseError,
        CryptocurrencyEtfSnapshot, CryptocurrencyEtfWindow,
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
pub(crate) const COUNTDOWN_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// The height of the pane's own row of readings.
const METRICS_HEIGHT: f32 = 58.0;

/// The kit key suffixes the header's controls are minted from.
///
/// A kit key is **application-level** — a button derives its click action from it and
/// two panes that build the same constant key share one handler — so the pane never
/// spells one out: [`WidgetCx::unique_key`] stamps each suffix with the pane's own
/// instance scope (AGENTS §1 rule 11).
const REFRESH_KEY_SUFFIX: &str = "cryptocurrency_etf.refresh";
const ASSET_KEY_SUFFIX: &str = "cryptocurrency_etf.asset";
const FUND_KEY_SUFFIX: &str = "cryptocurrency_etf.fund";
const WINDOW_KEY_SUFFIX: &str = "cryptocurrency_etf.window";

/// The stable id of the fund choice that means every fund.
const ALL_FUNDS_ID: &str = "all";

/// What a fund's id is built from, so a fund called `all` could not collide with the
/// choice above.
const FUND_ID_PREFIX: &str = "fund:";

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

impl TranslatedError for CryptocurrencyEtfFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for CryptocurrencyEtfFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Parse(error) => error.translated_message(translate),
            Self::Timeout(timeout) => t_smol!(
                translate,
                "error.cryptocurrency_etf.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl TranslatedError for CryptocurrencyEtfServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t_smol!(translate, "error.cryptocurrency_etf.stream_unavailable")
            }
        }
    }
}

impl TranslatedError for CryptocurrencyEtfParseError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::EmptyAssetMap { .. } => t_smol!(translate, "error.cryptocurrency_etf.no_data"),
            Self::InvalidDate { value, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.invalid_date",
                value = value.as_str()
            ),
            Self::DuplicateDate { date, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.duplicate_date",
                value = date.to_string()
            ),
            Self::TooManyRows { maximum, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.too_many_rows",
                maximum = maximum,
                value = maximum.saturating_add(1)
            ),
            Self::InvalidNumber { value, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.invalid_number",
                value = value.as_str()
            ),
            Self::NonFiniteTotal { .. } | Self::NonFiniteAssets { .. } => {
                t_smol!(translate, "error.cryptocurrency_etf.non_finite")
            }
        }
    }
}

/// What a header control asks the pane to do.
///
/// A control's handler runs with an application context and no widget context, so it
/// cannot change the pane: it writes the request and asks for a frame, and the next
/// `sync` applies it (AGENTS §4.2 — a control asks for the next `sync` instead of
/// re-entering the widget).
#[derive(Clone, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Refresh,
    Asset(CryptocurrencyEtfAsset),
    Window(CryptocurrencyEtfWindow),
    Fund(CryptocurrencyEtfFundSelection),
}

/// The countdown the header draws, and the one-shot timer that keeps it moving.
///
/// It is shared with the timer callback (`Rc`), which has only `&mut App`: the deadline
/// the label counts to lives in a `Cell`, the label itself is a signal, and the callback
/// re-arms the timer. Every deadline change goes through `update`, which cancels the
/// pending tick before it writes the label and arms the next chain — a deadline that
/// flickers cannot leave two chains ticking (I3: the pane is woken once per tick, not
/// twice), and `release` cancels through the same path.
struct Countdown {
    /// The window the timer belongs to.
    window: WindowId,
    /// The wall-clock millisecond the next read is due at, or nothing worth counting
    /// to (a read in flight, a failure, a schedule that stopped).
    deadline: Cell<Option<i64>>,
    /// The time left, as the header's text node draws it.
    label: Signal<Option<SmolStr>>,
    /// The wall clock, which is what a deadline published in Unix milliseconds is
    /// comparable against; a test hands in a clock it can move.
    now: Rc<dyn Fn() -> i64>,
    /// The tick this pane is waiting for, if any. Holding the id is what makes a
    /// deadline change and `release` able to cancel it instead of leaving it to fire.
    timer: std::cell::RefCell<Option<TimerId>>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            deadline: Cell::new(None),
            label: cx.signal(None),
            now,
            timer: std::cell::RefCell::new(None),
        }
    }

    /// The deadline the label is counting to.
    fn deadline(&self) -> Option<i64> {
        self.deadline.get()
    }

    /// Moves the countdown to a new deadline (or stops it), and re-arms the timer.
    ///
    /// The label is written here rather than only on the next tick: a new answer
    /// carries a new deadline, and the header has to state it on the frame that shows
    /// the answer. The pending tick is cancelled first, so a deadline that comes back
    /// before the old tick was due cannot leave a second chain running.
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

    /// Recomputes the label from the clock; returns whether it changed.
    ///
    /// The write only requests a frame when the label really moved (`App::write`
    /// short-circuits equal values), which is what keeps a quiet 250 ms tick from
    /// repainting the pane four times a second.
    fn write_label(&self, app: &mut App) -> bool {
        app.write(
            &self.label,
            countdown_label(self.deadline.get(), (self.now)()),
        )
    }

    /// Arms the one-shot tick for the next interval, if there is anything to count to.
    fn arm(self: &Rc<Self>, app: &mut App) {
        if self.deadline.get().is_none() {
            return;
        }
        let this = Rc::clone(self);
        match app.after(self.window, COUNTDOWN_TICK, move |app| this.tick(app)) {
            Ok(id) => *self.timer.borrow_mut() = Some(id),
            Err(error) => {
                log::error!("the ETF countdown could not arm its next tick: {error}");
            }
        }
    }

    /// One tick: write the label and re-arm the next one.
    ///
    /// The timer it just served is already spent, so the field is cleared first; the
    /// arm then owns the only pending tick this chain has.
    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

/// The header controls this pane built, and the copy they were built with.
///
/// A kit picker's item labels and a kit button's label are chosen when the control is
/// built (a language switch has to rebuild it, AGENTS §15.1); its *selection* is not —
/// a picker's trigger text is a `bind_text` over the picker's own signals, so a snapshot
/// that adds a fund is `set_items`.
struct HeaderControls {
    /// The widget's own row inside the group the pane made, which the widget removes
    /// itself.
    root: NodeId,
    /// The node the refresh button lives in.
    ///
    /// A kit button's label is chosen when it is built, so a language switch replaces
    /// it; the slot is what keeps the replaced button in the row's first position
    /// without the widget having to move nodes.
    refresh_slot: NodeId,
    /// The language the controls' copy was built in.
    language: LanguageCode,
    button: Button,
    asset: Select,
    fund: Select,
    window: Select,
    /// The fund picker's item ids, so a snapshot that changed the fund list is compared
    /// rather than replaced every frame.
    fund_items: Vec<SmolStr>,
}

/// One ETF flow pane: the account it follows, the chart it draws, and its controls.
pub struct CryptocurrencyEtfWidget {
    /// The asset, window and fund this pane is showing, which it starts at the defaults.
    asset: CryptocurrencyEtfAsset,
    window: CryptocurrencyEtfWindow,
    fund: CryptocurrencyEtfFundSelection,
    /// The executor bridge, when this host has a service connection to drive.
    ///
    /// A headless host without a Kytrix service has nothing to ask for: the pane reports
    /// that as its failure instead of starting a service it could never answer.
    binding: Option<WidgetBinding<CryptocurrencyEtfProtocol>>,
    status: WidgetStatus,
    recovery: WidgetAction,
    /// The chart, which exists once an answer has been read and is dropped with the
    /// account it belonged to.
    chart: Option<CryptocurrencyEtfChart>,
    /// The node the chart's canvas lives in, once the body exists.
    ///
    /// The body is built once, so a chart that is created after it (a new account's
    /// first answer) is built into this node rather than into a new body.
    chart_host: Option<NodeId>,
    /// The row the readings' cells live in, once the body exists.
    metrics_parent: Option<NodeId>,
    /// The cells of the row of readings, so the row can be rebuilt in place when its
    /// shape changes.
    metrics_cells: Vec<NodeId>,
    /// Whether the row of readings has a net-asset cell, which is a structural fact
    /// about the row rather than a value inside it.
    metrics_show_net_assets: bool,
    /// What the row of readings draws, as the row's `bind_text` closures read it.
    metrics: Signal<Option<Metrics>>,
    snapshot: Option<Arc<CryptocurrencyEtfSnapshot>>,
    /// The revision of the answer the chart is showing, which is what makes the pane
    /// content rather than a place waiting for something.
    accepted: Option<u64>,
    /// The revision the stream has reported ready. Readiness is published at the
    /// protocol's own boundary, so a pane that accepted an older answer than the one
    /// a status described stays busy.
    ready: Option<u64>,
    /// When the next automatic read is due, as the service's own deadline.
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
    /// Whether a refresh command this pane sent has been answered *and its round
    /// reported*.
    ///
    /// It is set by the click and cleared when the service says what it did with the
    /// command, which is what closes the window between the two: the service holds
    /// its gate for the whole round, and the round is announced after the command is
    /// acknowledged — so a control that trusted the acknowledgement alone would be
    /// live again for the moment in between.
    refresh_pending: bool,
    /// Whether the load this pane sent has been answered.
    load_pending: bool,
    /// The last RPC reply this pane consumed, which is what tells a reply it has
    /// already applied from a newer one.
    last_request_id: Option<RequestId>,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    /// Where a header control hands over what the user did.
    requests: Signal<Option<HeaderRequest>>,
    /// The header controls this pane built, if the pane has built its header row.
    header: Option<HeaderControls>,
}

impl WidgetView for CryptocurrencyEtfWidget {
    type Config = CryptocurrencyEtfConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.cryptocurrency_etf");
    const DISPLAY_NAME: &'static str = "widget.names.cryptocurrency_etf";
    const CATEGORY: WidgetCategory = WidgetCategory::Statistics;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.cryptocurrency_etf");
    const ICON: Option<&'static str> = Some("widgets/cryptocurrency_etf.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        // The answer is read through the Kytrix service connection the composition root
        // injects; a host without one leaves the source absent and `load` reports that.
        let source = cx.services().kytrix_service().cloned();
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);

        Ok(Self::assemble(source, now, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        let account = cx.account();
        // The answer on screen was served to the account that left, so a load for
        // another account starts over rather than keeping it.
        let kept = retry && account == self.account_id;
        self.account_id = account;
        self.start(kept, cx);
    }

    /// A pane is content once it has accepted an answer for the account it follows.
    ///
    /// Having a chart is not the same thing: a chart is drawn for whatever the view
    /// names as soon as an answer arrives, and an empty chart is not content — it is a
    /// pane that has asked for something and is waiting for it.
    fn has_content(&self) -> bool {
        self.chart.is_some() && self.accepted.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_ETF")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let row = NodeBuilder::boxed()
            .id("TEST_ETF_ROW")
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height(METRICS_HEIGHT)
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(12.0)
                    .padding_x(12.0)
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        Separator::horizontal(cx.theme().clone()).build(cx.tree_mut(), root)?;
        let chart_host = NodeBuilder::boxed()
            .id("TEST_ETF_CHART_HOST")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_height_0()
                    .min_width_0()
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;

        let show_net_assets = self.show_net_assets();
        self.rebuild_metrics_cells(row, show_net_assets, cx)?;
        self.metrics_show_net_assets = show_net_assets;
        self.metrics_parent = Some(row);
        self.chart_host = Some(chart_host);

        if let Some(chart) = self.chart.as_mut() {
            chart.build(cx, chart_host)?;
        }
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // What the service delivered is drained before anything in this frame can
        // replace a command: an answer that arrived before the frame belongs to the
        // command that was in flight when it arrived (a sign-out or an account change
        // later in this hook replaces that command, and would otherwise make the old
        // answer look like the new command's).
        self.drain_inbox(cx);

        // The settings are the pane's scope: signing in as someone else is another read,
        // and signing out is no read at all. A settings write reaches this hook on the
        // frame it schedules (the surface root reads the same signal), and the comparison
        // is what makes it a reload only when the account really moved.
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx);
        }

        // A header control owns no `&mut self`; it asks for the transition it was used
        // for, and this is where the request becomes one. The read is taken out of the
        // signal before the write: an engine signal is not copied on read, so a write
        // while a read guard is alive would alias the value it is reading.
        let request = cx.read(&self.requests).clone();
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is
        // one frame away, so ask for that frame.
        if self.has_content() && self.chart_host.is_none() {
            cx.request_frame()?;
        }

        // A language switch rewrites every label the chart draws and the copy of every
        // header control; both are decided on the frame that shows them.
        self.sync_chart_locale(cx);
        self.sync_metrics_row(cx)?;
        self.sync_chart(cx)?;

        // The countdown follows the schedule the state now states. Comparing first is
        // what keeps a steady frame from cancelling and re-arming the same tick.
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
        if group != HeaderGroup::Right {
            return Ok(());
        }
        // The pane may rebuild the row it calls this for: the controls this widget built
        // into the previous one are the widget's, so they go before the new ones are
        // built (a pane that reuses one group node must not accumulate controls).
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        let Some(chart) = self.chart.as_mut() else {
            return false;
        };
        // A pointer that is over the plot follows the reading under it; one that left
        // says so. Either way only a *reading* that moved is a change worth a frame: a
        // pointer that moved a pixel inside one day is the same drawing.
        let moved = match input {
            ui_core::event::InputEvent::PointerMove { position, .. }
            | ui_core::event::InputEvent::PointerEnter { position, .. } => {
                chart.point_at(*position)
            }
            ui_core::event::InputEvent::PointerLeave { .. } => chart.clear_hover(),
            _ => return false,
        };
        if moved {
            let _ = cx.request_frame();
        }
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        // The header's group node belongs to the pane, which removes its row: only the
        // components' signal scopes and action handlers are this widget's to release.
        self.release_header(cx, false);
        // The chart's canvas node lives in the body, which the framework removes after
        // this hook; the geometry it registered is the chart's own, so it goes here.
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        // The countdown tick is this widget's: the framework's subtree teardown does not
        // own it, so release is where it is cancelled.
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.chart_host = None;
        self.metrics_parent = None;
        self.metrics_cells.clear();
        self.snapshot = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
    }
}

impl CryptocurrencyEtfWidget {
    /// Builds a pane over the collaborators a caller hands it.
    ///
    /// The source is an argument because the pane has no opinion about where its answer
    /// comes from: the production constructor answers it with the injected Kytrix service
    /// connection, and a test with a fake one. The clock is an argument for the same
    /// reason: a countdown is measured against a wall clock, and a test that drives the
    /// ticks has to own that clock.
    fn assemble<S: CryptocurrencyEtfSource>(
        source: Option<S>,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding =
            source.map(|source| WidgetBinding::spawn(cx, CryptocurrencyEtfService::new(source)));
        let countdown = Rc::new(Countdown::new(cx, now));
        let metrics = cx.signal(None::<Metrics>);
        let requests = cx.signal(None::<HeaderRequest>);
        // The pane records the account it was built for, but it does not start anything:
        // the framework calls `load` once construction is done.
        let account_id = cx.account();

        Self {
            asset: CryptocurrencyEtfAsset::default(),
            window: CryptocurrencyEtfWindow::default(),
            fund: CryptocurrencyEtfFundSelection::default(),
            binding,
            status,
            recovery,
            chart: None,
            chart_host: None,
            metrics_parent: None,
            metrics_cells: Vec::new(),
            metrics_show_net_assets: true,
            metrics,
            snapshot: None,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            load_pending: false,
            last_request_id: None,
            account_id,
            requests,
            header: None,
        }
    }

    /// A pane over a source the caller answers, for the crate's own tests.
    #[cfg(test)]
    fn new_with_source<S: CryptocurrencyEtfSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    /// A pane whose countdown reads a clock the test moves.
    #[cfg(test)]
    fn new_with_source_at_clock<S: CryptocurrencyEtfSource>(
        source: S,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    /// A pane with no service connection behind it.
    #[cfg(test)]
    fn new_without_source(
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        Self::assemble(
            None::<crate::service::test_support::FakeSource>,
            now,
            status,
            recovery,
            cx,
        )
    }

    /// The account the pane would accept an answer from.
    fn current_key(&self) -> Option<CryptocurrencyEtfStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CryptocurrencyEtfStreamKey { account_id })
    }

    /// Whether this pane has anything to read.
    ///
    /// Every header control reads this rather than deciding for itself: a window and a
    /// fund only say which part of an answer to show, so with no answer coming they
    /// change nothing, and a pane that cannot refresh must not present itself as able to
    /// re-scope either.
    fn has_scope(&self) -> bool {
        self.current_key().is_some()
    }

    /// Whether the pane's row of readings carries a net-asset cell.
    fn show_net_assets(&self) -> bool {
        matches!(self.fund, CryptocurrencyEtfFundSelection::All)
    }

    /// The deadline this pane counts down to, if it has one to count down to.
    ///
    /// A countdown belongs to a pane that is *ready*: it counts down the time left until
    /// the next automatic read of the answer that is on screen. While a read is in flight
    /// there is nothing to count down to — the deadline the service announced belongs to
    /// the read after the one being waited for — so a number that started moving then
    /// would be counting down to an answer the pane has not received. A pane with no
    /// account has no deadline either: it is not reading.
    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the countdown is: a pane that is connecting, reading or
    /// failed has nothing to bring forward — the first two are already waiting on data,
    /// and a failure is a state the framework offers its own recovery over, above the
    /// chart. So the control refuses input the rest of the time, and its countdown is
    /// shown only while there is one.
    ///
    /// The pending half is not decoration: the service refuses a second read while one
    /// is in flight and the framework shows a refusal as a failure, so a control that
    /// forwarded the click while its own command was still unanswered would answer a
    /// fast double click with a banner.
    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    /// Starts reading, or restarts for an account the user is retrying.
    ///
    /// A retry keeps the chart the user is still reading — blanking it to reconnect would
    /// flash a screen of data away — while an explicit load starts over, because the pane
    /// cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.ready = None;
        self.next_refresh_at = None;
        // The commands that were in flight belong to the generation this call is
        // replacing, so their answers are not something to wait for any more.
        self.refresh_pending = false;
        self.load_pending = false;
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for.
            self.clear_answer(cx);
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
            );
            let _ = cx.request_frame();
            return;
        };
        if !retry {
            self.clear_answer(cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let request = if retry {
            CryptocurrencyEtfRequest::Retry(key)
        } else {
            CryptocurrencyEtfRequest::Load(key)
        };
        let Some(binding) = self.binding.as_mut() else {
            // The host installed no Kytrix service connection: there is nothing this pane
            // can read with, and that is a failure rather than a wait.
            let recovery = self.recovery.clone();
            let error = CryptocurrencyEtfFetchError::Service(
                kytrix_service_provider::service::engine::KytrixServiceError::Disconnected,
            );
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(CryptocurrencyEtfFailure::Fetch(error)), recovery),
            );
            return;
        };
        self.load_pending = true;
        binding.call(cx, request);
        let _ = cx.request_frame();
    }

    /// Drops the answer and the chart drawn from it.
    ///
    /// The chart's canvas node is the chart's own, so it goes with it: a body is built
    /// once, and the node the next answer's chart is built into is the host it already
    /// has.
    fn clear_answer(&mut self, cx: &mut WidgetCx<'_>) {
        self.snapshot = None;
        self.accepted = None;
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        cx.write(&self.metrics, None);
        let _ = cx.request_frame();
    }

    /// Consumes what the binding delivered, in arrival order.
    fn drain_inbox(&mut self, cx: &mut WidgetCx<'_>) {
        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for message in messages {
            match message {
                Inbound::Reliable { key, message } => {
                    self.apply_reliable(key, message, cx);
                }
                Inbound::Latest { key, message } => {
                    self.apply_latest(key, message, cx);
                }
                Inbound::Rpc { request_id, result } => {
                    self.apply_rpc(request_id, result, cx);
                }
            }
        }
    }

    /// Applies one RPC reply, once its request identity says it is a new answer.
    ///
    /// Only one call is in flight at a time (the binding replaces and aborts the previous
    /// one), so the pending flags say which command an answer belongs to. The identity is
    /// the second half of that: a reply that is not newer than the last one this pane
    /// consumed is an answer it has already applied, and applying it again would clear
    /// the command that replaced the one it answered.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self
            .last_request_id
            .is_some_and(|answered| request_id <= answered)
        {
            log::debug!(
                "the ETF pane ignored reply {request_id}: it is not newer than the last one it answered"
            );
            return;
        }
        self.last_request_id = Some(request_id);
        if self.load_pending {
            self.load_pending = false;
            self.apply_rpc_result(result, cx);
        } else if self.refresh_pending {
            self.apply_rpc_result(result, cx);
        }
    }

    /// Reported one way, whichever request it answers: the two differ in what they asked
    /// for, not in what a failure means to the reader.
    ///
    /// An accepted refresh stays pending until the round it started is reported: the
    /// acknowledgement and the round are two different messages, and the gate the service
    /// holds spans both.
    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match result {
            Ok(
                CryptocurrencyEtfResponse::StreamStarted | CryptocurrencyEtfResponse::RefreshQueued,
            ) => {}
            Err(error) => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(CryptocurrencyEtfFailure::Rpc(error)), recovery),
                );
            }
        }
    }

    /// Applies a status the stream reported.
    fn apply_reliable(
        &mut self,
        key: CryptocurrencyEtfStreamKey,
        message: Arc<CryptocurrencyEtfStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let CryptocurrencyEtfStreamMessage::Status(status) = message.as_ref() else {
            return;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            CryptocurrencyEtfStatus::Connecting => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            CryptocurrencyEtfStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            CryptocurrencyEtfStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                // The reliable channel and the latest one have no order between them, so
                // this can arrive before the answer it describes: claiming readiness now
                // would be claiming a chart the pane does not have.
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            CryptocurrencyEtfStatus::Failed { error } => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(CryptocurrencyEtfFailure::Fetch(error.clone())),
                        recovery,
                    ),
                );
            }
        }
    }

    /// Applies an answer the stream published.
    fn apply_latest(
        &mut self,
        key: CryptocurrencyEtfStreamKey,
        message: Arc<CryptocurrencyEtfStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let CryptocurrencyEtfStreamMessage::Snapshot { revision, snapshot } = message.as_ref()
        else {
            return;
        };
        // An answer the pane has already overtaken would only make the chart jump
        // backwards: within one account the service's revision only moves forwards.
        if self.accepted.is_some_and(|held| *revision < held) {
            return;
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
        {
            return;
        }
        self.snapshot = Some(Arc::clone(snapshot));
        self.accepted = Some(*revision);
        self.sync_chart_view(cx);
        // Readiness is published at the protocol's own boundary: a complete answer is not
        // the same thing as the answer a status said was complete, and this is where the
        // two meet.
        if self.ready.is_some_and(|ready| *revision >= ready) {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        let _ = cx.request_frame();
    }

    /// Points the chart at what the pane's view names, out of the answer it holds.
    ///
    /// The chart is created on the first answer and kept afterwards: a new answer is a
    /// new view of the same drawing, which is what
    /// [`CryptocurrencyEtfChart::set_view`] is for.
    fn sync_chart_view(&mut self, cx: &mut WidgetCx<'_>) {
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
        self.sync_chart_metrics(cx);
    }

    /// Re-resolves the chart's labels when the language moved under it.
    fn sync_chart_locale(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(chart) = self.chart.as_mut() else {
            return;
        };
        if chart.sync_locale(cx) {
            self.sync_chart_metrics(cx);
            let _ = cx.request_frame();
        }
    }

    /// Builds the chart's canvas node if the chart arrived after the body, and rebuilds
    /// its canvas from what the pane now shows.
    fn sync_chart(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(host) = self.chart_host else {
            return Ok(());
        };
        let Some(chart) = self.chart.as_mut() else {
            return Ok(());
        };
        if chart.node().is_none() {
            chart.build(cx, host)?;
        }
        if chart.sync(cx)? {
            self.sync_chart_metrics(cx);
        }
        Ok(())
    }

    /// Publishes the reading the row of readings shows for the point under the pointer.
    ///
    /// The row follows the pointer and falls back to the newest day, so a pane with an
    /// answer always has a reading to show; the write is skipped when the reading did not
    /// move (I3).
    fn sync_chart_metrics(&mut self, cx: &mut WidgetCx<'_>) {
        let metrics = self
            .chart
            .as_ref()
            .and_then(CryptocurrencyEtfChart::metrics);
        cx.write(&self.metrics, metrics);
    }

    /// Drops a fund the asset the pane now shows does not have.
    ///
    /// A fund belongs to one asset's complex, so a choice made under BTC is not a choice
    /// under SOL: it would draw an empty chart for a fund that does not exist rather than
    /// the total the user can read.
    fn normalize_fund(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let CryptocurrencyEtfFundSelection::Named(name) = &self.fund else {
            return;
        };
        let dataset = snapshot.dataset(self.asset);
        if dataset.fund_index(name.as_str()).is_none() {
            self.fund = CryptocurrencyEtfFundSelection::All;
        }
    }

    /// Applies what a header control handed over.
    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Refresh => {
                self.refresh(cx);
                return;
            }
            HeaderRequest::Asset(asset) => {
                if self.asset == asset {
                    return;
                }
                self.asset = asset;
                self.normalize_fund();
            }
            HeaderRequest::Window(window) => {
                if self.window == window {
                    return;
                }
                self.window = window;
            }
            HeaderRequest::Fund(fund) => {
                if self.fund == fund {
                    return;
                }
                self.fund = fund;
            }
        }
        self.sync_chart_view(cx);
        let _ = cx.request_frame();
    }

    /// Reads the account's answer again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, so the command it sends is one the loop answers: a
    /// failure is recovered through the framework's own control, above the chart.
    fn refresh(&mut self, cx: &mut WidgetCx<'_>) {
        if self.refresh_is_disabled(cx.app()) {
            return;
        }
        self.refresh_pending = true;
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        binding.call(cx, CryptocurrencyEtfRequest::Refresh);
        let _ = cx.request_frame();
    }

    /// Builds the pane's row of readings into `parent`.
    ///
    /// The row is data-shaped rather than static: a chart of one fund has a flow but no
    /// assets of its own to report, so the net-asset cell is not drawn. That is a
    /// structural fact about the row (a different number of cells), which is why a change
    /// of fund rebuilds the row rather than hiding a cell.
    fn rebuild_metrics_cells(
        &mut self,
        parent: NodeId,
        show_net_assets: bool,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let metrics = self.metrics;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();

        let date = self.build_metric_cell(
            parent,
            "TEST_ETF_DATE",
            {
                let translate = translate.clone();
                move |read| {
                    let _language = *read.read(&language);
                    t_smol!(translate, "widget.cryptocurrency_etf.date")
                }
            },
            {
                let theme = theme.clone();
                move |read| {
                    let _ = theme.appearance(read);
                    Role::FgDefault
                }
            },
            move |read| {
                let value: Option<Metrics> = (*read.read(&metrics)).clone();
                value.map_or_else(SmolStr::default, |metrics| metrics.date)
            },
            cx,
        )?;
        // The flow reads in the colour of the direction it went: an inflow and an outflow
        // are two readings, and the row is where they are compared.
        let flow = self.build_metric_cell(
            parent,
            "TEST_ETF_FLOW",
            move |read| {
                let value: Option<Metrics> = (*read.read(&metrics)).clone();
                value.map_or_else(SmolStr::default, |metrics| metrics.caption)
            },
            {
                let theme = theme.clone();
                move |read| {
                    let _ = theme.appearance(read);
                    let reading: Option<Metrics> = (*read.read(&metrics)).clone();
                    let value = reading.map(|metrics| metrics.flow_value);
                    match value {
                        Some(value) if value > 0.0 => Role::MarketUp,
                        Some(value) if value < 0.0 => Role::MarketDown,
                        _ => Role::MarketFlat,
                    }
                }
            },
            move |read| {
                let value: Option<Metrics> = (*read.read(&metrics)).clone();
                value.map_or_else(SmolStr::default, |metrics| metrics.flow)
            },
            cx,
        )?;
        let mut cells = vec![date, flow];
        if show_net_assets {
            let net_assets = self.build_metric_cell(
                parent,
                "TEST_ETF_NET_ASSETS",
                move |read| {
                    let _language = *read.read(&language);
                    t_smol!(translate, "widget.cryptocurrency_etf.total_net_assets")
                },
                move |read| {
                    let _ = theme.appearance(read);
                    Role::FgDefault
                },
                move |read| {
                    let value: Option<Metrics> = (*read.read(&metrics)).clone();
                    value
                        .and_then(|metrics| metrics.net_assets)
                        .unwrap_or_default()
                },
                cx,
            )?;
            cells.push(net_assets);
        }
        self.metrics_cells = cells;
        Ok(())
    }

    /// Builds one reading of the row: what it is, then the number.
    ///
    /// The title is a `bind_text` reading the language and the value a `bind_text` reading
    /// the row's own signal, so neither changes the row's structure and neither needs a
    /// rebuild on a language switch.
    fn build_metric_cell(
        &self,
        parent: NodeId,
        id: &'static str,
        title: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
        role: impl Fn(&mut ReadCtx<'_>) -> Role + 'static,
        value: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
        cx: &mut WidgetCx<'_>,
    ) -> Result<NodeId, EngineError> {
        let theme = cx.theme().clone();
        let cell = NodeBuilder::boxed()
            .id(id)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .flex_col()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let title_theme = theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .bind_text(title)
        .bind_style(move |read| StylePatch {
            foreground: Some(title_theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), cell)?;
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Medium.text_size()),
        )
        .bind_text(value)
        .bind_style(move |read| {
            let role = role(read);
            StylePatch {
                foreground: Some(theme.role(read, role)),
                ..StylePatch::default()
            }
        })
        .build(cx.tree_mut(), cell)?;
        Ok(cell)
    }

    /// Keeps the row of readings in step with the chart's shape.
    ///
    /// The net-asset cell exists while the chart is of the whole complex and not while it
    /// is of one fund, which is a change of the row's structure rather than of a value in
    /// it: the row is rebuilt in place, under the same parent, with the same ids.
    fn sync_metrics_row(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.metrics_show_net_assets == self.show_net_assets() {
            return Ok(());
        }
        let show_net_assets = self.show_net_assets();
        let Some(row) = self.metrics_parent else {
            return Ok(());
        };
        if cx.tree().get(row).is_err() {
            // The pane took the body down; the cells went with it.
            self.metrics_cells.clear();
            self.metrics_parent = None;
            return Ok(());
        }
        for cell in std::mem::take(&mut self.metrics_cells) {
            remove_own_node(cx, cell);
        }
        self.rebuild_metrics_cells(row, show_net_assets, cx)?;
        self.metrics_show_net_assets = show_net_assets;
        cx.request_frame()?;
        Ok(())
    }

    /// Builds the pane's header controls into the group node the pane made.
    fn build_header(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let row = NodeBuilder::boxed()
            .id("TEST_ETF_HEADER")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        // The refresh control: an icon button, and the countdown beside it. The slot is
        // what a language switch rebuilds the button *in*, so the replaced button keeps
        // the row's first position without any node being moved.
        let refresh_slot = NodeBuilder::boxed()
            .id("TEST_ETF_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), row)?;
        let button = self.build_refresh_button(cx, refresh_slot)?;

        let countdown = self.countdown.label;
        let countdown_theme = theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_ETF_COUNTDOWN")
        .bind_text(move |read| {
            let label: Option<SmolStr> = read.read(&countdown).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(countdown_theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), refresh_slot)?;

        // The asset picker: which ETF complex the chart is about.
        let asset_id = SmolStr::new_static(self.asset.label());
        let asset = self.build_select(
            cx,
            theme.clone(),
            ASSET_KEY_SUFFIX,
            "widget.cryptocurrency_etf.asset",
            asset_items(),
            asset_id.clone(),
            asset_id,
            RequestKind::Asset,
            row,
        )?;
        // The fund picker: the whole complex, or one fund inside it.
        let fund_items = self.fund_items(cx);
        let fund_ids = item_ids(&fund_items);
        let fund = self.build_select(
            cx,
            theme.clone(),
            FUND_KEY_SUFFIX,
            "widget.cryptocurrency_etf.fund",
            fund_items,
            fund_id(&self.fund),
            t_smol!(translate, "widget.cryptocurrency_etf.all"),
            RequestKind::Fund,
            row,
        )?;
        // The window picker: how much of the history the chart shows.
        let window_select = self.build_select(
            cx,
            theme,
            WINDOW_KEY_SUFFIX,
            "widget.cryptocurrency_etf.time_window",
            window_items(&translate),
            SmolStr::new_static(window_id(self.window)),
            window_label(&translate, self.window),
            RequestKind::Window,
            row,
        )?;

        self.header = Some(HeaderControls {
            root: row,
            refresh_slot,
            language,
            button,
            asset,
            fund,
            window: window_select,
            fund_items: fund_ids,
        });
        Ok(())
    }

    /// Builds the refresh control into `parent`, in the state the pane is in now.
    fn build_refresh_button(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Button, EngineError> {
        let requests = self.requests;
        let label = t_smol!(cx.translate(), "widget.cryptocurrency_etf.refresh_now");
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
                // A button handler owns no widget: it asks the next sync for the refresh.
                ctx.update(&requests, |request| *request = Some(HeaderRequest::Refresh));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

    /// Builds one picker of the header over `items`, with `chosen` selected.
    fn build_select(
        &self,
        cx: &mut WidgetCx<'_>,
        theme: ThemeHandle,
        key_suffix: &str,
        label_key: &str,
        items: Vec<SelectItem>,
        chosen: SmolStr,
        placeholder: SmolStr,
        kind: RequestKind,
        parent: NodeId,
    ) -> Result<Select, EngineError> {
        let requests = self.requests;
        let disabled = !self.has_scope();
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(cx.translate(), label_key))
            .placeholder(placeholder);
        let window = cx.window();
        let key = cx.unique_key(key_suffix);
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                hand_over_select(requests, kind, window, app, &chosen);
            },
        );
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!("the ETF header picker `{key_suffix}` could not set its items: {error}");
            });
        select.set_chosen(cx.app_mut(), [chosen.as_str()], false);
        let _ = select.set_disabled(cx.app_mut(), disabled);
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    /// The fund picker's items for the asset the pane is showing.
    ///
    /// The funds come from the answer rather than from a list of their own: which funds an
    /// asset has is a property of the data, and a list compiled into the pane would go
    /// stale the first time a fund launched. Before the first answer there is only the
    /// total, which is what a pane shows anyway.
    fn fund_items(&self, cx: &WidgetCx<'_>) -> Vec<SelectItem> {
        let mut items = vec![SelectItem::new(
            SmolStr::new_static(ALL_FUNDS_ID),
            t_smol!(cx.translate(), "widget.cryptocurrency_etf.all"),
        )];
        let Some(snapshot) = self.snapshot.as_ref() else {
            return items;
        };
        for fund in snapshot.dataset(self.asset).funds.iter() {
            items.push(SelectItem::new(
                format_smolstr!("{FUND_ID_PREFIX}{fund}"),
                fund.clone(),
            ));
        }
        items
    }

    /// Keeps the header controls in step with what they should state.
    ///
    /// A kit button's label and tooltip are chosen when it is built, so a language switch
    /// rebuilds it — in the slot it already occupies, so nothing else in the row moves.
    /// A picker is different: its *trigger text* is a `bind_text` over its own signals, so
    /// the language is carried by `set_items` (with the translated labels) rather than by a
    /// rebuild. What a picker *offers* is likewise its item list, which a snapshot
    /// (`set_items`) or a selection (`set_chosen`) moves without a rebuild.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(language) = self.header.as_ref().map(|header| header.language) else {
            return Ok(());
        };
        if language != *cx.read(&cx.language()) {
            let Some(header) = self.header.as_ref() else {
                return Ok(());
            };
            let slot = header.refresh_slot;
            if cx.tree().get(slot).is_err() {
                // The pane took its row down; the controls went with it, and only the
                // components are left to release.
                self.release_header(cx, false);
                return Ok(());
            }
            self.replace_refresh_button(cx, slot)?;
            let translate = cx.translate().clone();
            let asset = asset_items();
            let window = window_items(&translate);
            let fund = self.fund_items(cx);
            let fund_ids = item_ids(&fund);
            let Some(header) = self.header.as_mut() else {
                return Ok(());
            };
            for (select, items) in [
                (&header.asset, asset),
                (&header.fund, fund),
                (&header.window, window),
            ] {
                select
                    .set_items(cx.app_mut(), items)
                    .unwrap_or_else(|error| {
                        log::warn!("the ETF header picker could not replace its items: {error}");
                    });
            }
            header.fund_items = fund_ids;
            header.language = *cx.read(&cx.language());
        }
        let disabled = !self.has_scope();
        let refresh_disabled = self.refresh_is_disabled(cx.app());
        let fund_items = self.fund_items(cx);
        let fund_ids = item_ids(&fund_items);
        let asset = SmolStr::new_static(self.asset.label());
        let fund = fund_id(&self.fund);
        let window = SmolStr::new_static(window_id(self.window));
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        if fund_ids != header.fund_items {
            header
                .fund
                .set_items(cx.app_mut(), fund_items)
                .unwrap_or_else(|error| {
                    log::warn!("the ETF fund picker could not replace its items: {error}");
                });
            header.fund_items = fund_ids;
        }
        header.button.sync(cx.app_mut())?;
        let _ = header.button.set_disabled(cx.app_mut(), refresh_disabled);
        sync_chosen(&header.asset, cx, &asset, disabled);
        sync_chosen(&header.fund, cx, &fund, disabled);
        sync_chosen(&header.window, cx, &window, disabled);
        header.asset.sync(cx.app_mut())?;
        header.fund.sync(cx.app_mut())?;
        header.window.sync(cx.app_mut())?;
        Ok(())
    }

    /// Replaces the refresh button in the slot it occupies.
    ///
    /// The old button's node goes with it: a kit button draws into the parent it was
    /// built for, and the row's slot is what keeps the replacement where the old one was.
    fn replace_refresh_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        slot: NodeId,
    ) -> Result<(), EngineError> {
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        if let Some(node) = header.button.node() {
            remove_own_node(cx, node);
        }
        let _ = header.button.release(cx.app_mut());
        let button = self.build_refresh_button(cx, slot)?;
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        header.button = button;
        Ok(())
    }

    /// Takes down the header controls' components, and the widget's own row when the pane
    /// still has its group node.
    fn release_header(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header.take() else {
            return;
        };
        if remove_node {
            remove_own_node(cx, header.root);
        }
        let _ = header.button.release(cx.app_mut());
        let _ = header.asset.release(cx.app_mut());
        let _ = header.fund.release(cx.app_mut());
        let _ = header.window.release(cx.app_mut());
    }
}

/// Which of the three picks a picker is, so the id it reports can be read as a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Asset,
    Fund,
    Window,
}

/// Hands a picker's choice over to the next `sync`.
///
/// The picker's callback runs with an application context and no widget context, so it
/// cannot change the pane: it writes the request signal and asks for a frame, and
/// [`WidgetView::sync`] applies the choice. A picker reports the id it was chosen by, and
/// the id says what it names — so the pane never has to guess which of the three it is.
fn hand_over_select(
    requests: Signal<Option<HeaderRequest>>,
    kind: RequestKind,
    window: WindowId,
    app: &mut App,
    chosen: &[SmolStr],
) {
    let Some(id) = chosen.first() else {
        return;
    };
    let request = match kind {
        RequestKind::Asset => CryptocurrencyEtfAsset::ALL
            .iter()
            .find(|asset| asset.label() == id.as_str())
            .copied()
            .map(HeaderRequest::Asset),
        RequestKind::Fund => Some(HeaderRequest::Fund(fund_from_id(id))),
        RequestKind::Window => window_from_id(id).map(HeaderRequest::Window),
    };
    let Some(request) = request else {
        return;
    };
    app.write(&requests, Some(request));
    let _ = app.request_repaint(window);
}

/// Writes the selection a picker should state, and whether it can be used at all.
///
/// The write is skipped when the picker already states it: `set_chosen` re-indexes the
/// selection against the item list, and doing that on every frame would be work nobody
/// can see (I3). `notify` is false because the pane is the one that *decided* this
/// selection — a picker reporting it back would be a second, redundant request.
fn sync_chosen(select: &Select, cx: &mut WidgetCx<'_>, id: &SmolStr, disabled: bool) {
    let stated = select.chosen(cx.app());
    if stated.len() != 1 || stated.first() != Some(id) {
        select.set_chosen(cx.app_mut(), [id.as_str()], false);
    }
    let _ = select.set_disabled(cx.app_mut(), disabled);
}

/// The ids of a picker's items, in the order they are offered.
fn item_ids(items: &[SelectItem]) -> Vec<SmolStr> {
    items.iter().map(|item| item.id.clone()).collect()
}

/// The asset picker's items: the complexes the service publishes, in its own order.
///
/// A fund's ticker is its name and its id at once: `HYPE` reads the same in every locale,
/// so there is nothing to translate here and nothing to keep apart the way the window's
/// stable ids are kept apart from its labels.
fn asset_items() -> Vec<SelectItem> {
    CryptocurrencyEtfAsset::ALL
        .iter()
        .map(|asset| {
            let ticker = SmolStr::new_static(asset.label());
            SelectItem::new(ticker.clone(), ticker)
        })
        .collect()
}

/// The window picker's items: a number of trading days, or all of them.
fn window_items(translate: &Translate) -> Vec<SelectItem> {
    CryptocurrencyEtfWindow::ALL
        .iter()
        .map(|window| {
            SelectItem::new(
                SmolStr::new_static(window_id(*window)),
                window_label(translate, *window),
            )
        })
        .collect()
}

/// Removes a node this widget built, if it is still in the tree.
///
/// A node that is already gone is not a failure: the pane rebuilds its rows, and a node
/// this widget built into the old one went with it.
fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) {
    if cx.tree().get(node).is_err() {
        return;
    }
    let window = cx.window();
    let _ = cx.app_mut().remove_node(window, node);
}

/// The label beside the refresh button for `deadline` at `now`.
///
/// A deadline that has already passed counts as `00:00` rather than as a negative wait,
/// which is the same rule [`format_countdown`] applies: the pane never shows a countdown
/// that has run out as anything but zero.
fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

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
fn window_label(translate: &Translate, window: CryptocurrencyEtfWindow) -> SmolStr {
    match window {
        CryptocurrencyEtfWindow::Days30 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 30)
        }
        CryptocurrencyEtfWindow::Days90 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 90)
        }
        CryptocurrencyEtfWindow::Days180 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 180)
        }
        CryptocurrencyEtfWindow::Days360 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 360)
        }
        CryptocurrencyEtfWindow::All => t_smol!(translate, "widget.cryptocurrency_etf.all"),
    }
}

/// The id a fund choice is named by in its picker.
fn fund_id(fund: &CryptocurrencyEtfFundSelection) -> SmolStr {
    match fund {
        CryptocurrencyEtfFundSelection::All => SmolStr::new_static(ALL_FUNDS_ID),
        CryptocurrencyEtfFundSelection::Named(name) => {
            format_smolstr!("{FUND_ID_PREFIX}{name}")
        }
    }
}

/// The fund choice an id names.
fn fund_from_id(id: &str) -> CryptocurrencyEtfFundSelection {
    match id.strip_prefix(FUND_ID_PREFIX) {
        Some(name) => CryptocurrencyEtfFundSelection::Named(SmolStr::new(name)),
        None => CryptocurrencyEtfFundSelection::All,
    }
}

#[cfg(test)]
mod tests;
