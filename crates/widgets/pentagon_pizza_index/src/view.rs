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
//! anonymous reader. Nothing is retried on a timer either; the settings snapshot is
//! what re-scopes the pane the moment the user signs in (see `sync`).
//!
//! # What it reports, and what it does not
//!
//! It reports the poll through the framework's status cell and nothing else: a failure
//! is a failure, and the framework decides where that is shown and what the recovery
//! looks like. The pane keeps no banner and no "unexpected response" state of its own.
//!
//! # Why a reply is matched by request identity
//!
//! Two commands share one inbox. Draining it happens before anything in the frame can
//! replace a command, so an answer that was delivered before the replacement is applied
//! to the command it belongs to; on top of that a reply is only accepted when its
//! framework `RequestId` is newer than the last one this pane consumed, which is what
//! keeps a reply the pane has already answered from clearing the command now in flight.
//!
//! # Why the colour is a status role
//!
//! The ladder is a severity, so it is drawn from the theme's status colors rather
//! than from a palette of its own: `success` for the calmest rung, `info`, `warning`,
//! `elevated` and `danger` for the most severe. That is what keeps the pane legible
//! in both appearances and keeps a red a reader has learned from an error banner
//! meaning the same thing here.
//!
//! # How the body follows the reading
//!
//! The body is built once, on the first frame the pane has content, and every value it
//! draws is a `bind_text`/`bind_style` closure over the pane's own signals: the reading
//! (level number, level name, bar colour), the settings snapshot (display time zone) and
//! the language. The engine's dependency graph therefore invalidates exactly the nodes
//! whose value moved — a new reading, a language switch, a display-zone change or an
//! appearance switch each redraw their own nodes.
//!
//! # Where the countdown lives
//!
//! It is text beside the refresh button — when the next read is due, and the way to
//! bring it forward, are one control's worth of information, so they sit together in the
//! header. The index publishes one point an hour, so the countdown is the only thing on
//! screen that moves on its own. It moves because an E10 one-shot timer writes the
//! countdown signal and re-arms itself (see `Countdown`); the write only requests a frame
//! when the label really changed, so a quiet pane does not repaint four times a second.
//! The timer the pane holds is cancelled on every deadline change and on release, so a
//! deadline that goes away cannot leave a chain running (I3) and a released pane cannot
//! keep waking the engine.
//!
//! # What the old implementation drew
//!
//! Nothing it drew by hand: the level bar is a plain box (a styled node, not a canvas) and
//! the previous implementation had no `canvas`/`paint_*` code at all (see the W2b
//! test-ledger's §0 for the migration evidence). The bar therefore stays a node with a
//! `bind_style` background; no engine `Canvas` is involved.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::role::Role,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::KytrixServiceError;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::{PaintSpec, StylePatch},
    text::TextStyle,
    window::WindowId,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    util::{format_countdown, unix_millis},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use crate::{
    model::{PizzaLevel, PizzaReading},
    service::{
        PizzaFetchError, PizzaProtocol, PizzaRequest, PizzaResponse, PizzaService,
        PizzaServiceError, PizzaSource, PizzaStatus, PizzaStreamKey, PizzaStreamMessage,
    },
};

/// How often the pane re-reads the clock behind the countdown.
///
/// It is not a fetch: it only re-checks the deadline the service published. A
/// quarter of a second is faster than the label's own resolution and cheaper than
/// a frame.
const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// How tall the colour bar under the reading is, and how round its ends are.
const LEVEL_BAR_HEIGHT: f32 = 6.0;
const LEVEL_BAR_RADIUS: f32 = LEVEL_BAR_HEIGHT / 2.0;

/// How wide the colour bar is allowed to grow under the reading.
const LEVEL_BAR_MAX_WIDTH: f32 = 220.0;

/// The size the rung's name is drawn at.
///
/// The theme carries the **control** size ladder (12/14/18 px) and no display
/// typography token yet (`crates/base_ui/src/theme` only has the control ladder), so
/// the heading keeps the 24 px the old `text_2xl()` drew it at. This is the same gap
/// `news` registered for its body typography; it becomes a theme token when that batch
/// lands, and the value moves there rather than staying here.
const LEVEL_NAME_SIZE: f32 = 24.0;

/// The weight the rung's number and name are drawn at (CSS `font-weight`).
const LEVEL_NUMBER_WEIGHT: u16 = 500;
const LEVEL_NAME_WEIGHT: u16 = 700;

/// The body's own spacing (the old tree's `gap_3` and `p_4`).
const BODY_GAP: f32 = 12.0;
const BODY_PADDING: f32 = 16.0;

/// The gap between the rung's number and its name (the old `gap_1`).
const LEVEL_COLUMN_GAP: f32 = 4.0;

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

impl TranslatedError for PizzaFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
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
    timer: RefCell<Option<TimerId>>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            deadline: Cell::new(None),
            label: cx.signal(None),
            now,
            timer: RefCell::new(None),
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
                log::error!(
                    "the Pentagon Pizza Index countdown could not arm its next tick: {error}"
                );
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

/// One command this pane sent and has not seen the answer to.
///
/// The pane's two commands share one binding, and the binding replaces (and aborts) the
/// call before it on every issue — but an answer can already be queued when that
/// happens. The identity [`WidgetBinding::call`] returned is what says whose answer
/// arrived: an answer this table does not know belongs to the command it replaced, and
/// applying it to the command now in flight is exactly the mis-attribution this table
/// exists to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The read that starts or restarts the stream: `Load` or `Retry`.
    Load,
    /// The read the refresh control brought forward.
    Refresh,
    /// The stop of a stream nobody follows any more: its answer is nothing to report.
    Unload,
}

/// One pane: the account it follows, the reading it holds, and the countdown to the
/// next one.
pub struct PentagonPizzaIndexWidget {
    /// The executor bridge. It is absent on a host that installed no Kytrix service
    /// connection, which is a business failure the pane reports rather than a wait.
    binding: Option<WidgetBinding<PizzaProtocol>>,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    /// The reading the pane is showing, once a round has published one. It is the
    /// content predicate, so it is a plain field: `has_content` is asked before a frame
    /// has an application context.
    reading: Option<PizzaReading>,
    /// The reading as the body's `bind_*` closures read it.
    reading_signal: Signal<Option<PizzaReading>>,
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
    /// The time left until that read, as the header draws it, and the tick that keeps
    /// it moving.
    countdown: Rc<Countdown>,
    /// Whether a refresh command this pane sent has not yet been reported on.
    refresh_pending: bool,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    /// Click requests from the header control: a button handler owns no `&mut self`,
    /// so it asks the next `sync` for the refresh instead of performing it.
    refresh_requests: Signal<u64>,
    seen_refresh_requests: u64,
    /// The root of the body this pane built, when the framework has built it. The
    /// countdown timer is scoped to it, so removing the body cancels the timer (E16).
    body_root: Option<NodeId>,
    /// The header control, kept so its state can be synchronized and it can be
    /// released.
    refresh_button: Option<Button>,
    /// The node the control was built into, and the group's parent node: a kit button's
    /// label is chosen when it is built, so a language switch rebuilds it in place.
    refresh_root: Option<NodeId>,
    refresh_parent: Option<NodeId>,
    /// The language the control was built in. A difference is what makes it rebuild.
    refresh_language: Option<LanguageCode>,
}

impl WidgetView for PentagonPizzaIndexWidget {
    type Config = PentagonPizzaIndexConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.pentagon_pizza_index");
    const DISPLAY_NAME: &'static str = "widget.names.pentagon_pizza_index";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.pentagon_pizza_index");
    const ICON: Option<&'static str> = Some("widgets/pentagon_pizza_index.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        // The index is read by the Kytrix service and handed to this pane over the
        // connector's own authenticated connection, so the route it takes is the one
        // the settings give the capability and this widget opens no socket of its
        // own. A host without the connector leaves the source absent; `load` reports
        // that as the pane's failure.
        let source = cx.services().kytrix_service().cloned();
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);

        Ok(Self::assemble(source, now, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // A retry keeps what is on screen only for the account that is showing it:
        // an account the settings replaced while the pane was failed is another
        // scope, and the reading belongs to the one that left.
        let retry = self.status.is_failed(cx.app()) && self.account_id == cx.account();
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the Pentagon Pizza Index pane could not start: {error}");
        }
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

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .items_center()
                    .justify_center()
                    .gap(BODY_GAP)
                    .padding(BODY_PADDING)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        let level = NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX_LEVEL")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .items_center()
                    .gap(LEVEL_COLUMN_GAP)
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        self.build_level_number(level, cx)?;
        self.build_level_name(level, cx)?;
        self.build_level_bar(root, cx)?;
        self.build_observed_at(root, cx)?;

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // What the service delivered is drained before anything in this frame can
        // replace a command: an answer that arrived before the frame belongs to the
        // command that was in flight when it arrived (a sign-out or an account change
        // later in this hook replaces that command, and would otherwise make the old
        // answer look like the new command's).
        self.drain_inbox(cx);

        // The settings are the pane's scope: signing in as someone else is another
        // read, and signing out is no read at all. A settings write reaches this hook on
        // the frame it schedules (the surface root reads the same signal), and the
        // comparison is what makes it a reload only when the account really moved.
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

        // A header control owns no `&mut self`; it asks for the refresh it was clicked
        // for, and this is where the request becomes one.
        let requests = *cx.read(&self.refresh_requests);
        if requests != self.seen_refresh_requests {
            self.seen_refresh_requests = requests;
            self.refresh(cx)?;
        }

        // The body is built by the framework from the answer `has_content` gives, and
        // it asks that question before this hook runs: content that first appeared here
        // is one frame away, so ask for that frame.
        if self.reading.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        // The countdown follows the schedule the state now states. Comparing first is
        // what keeps a steady frame from cancelling and re-arming the same tick.
        let deadline = self.countdown_deadline(cx.app());
        if deadline != self.countdown.deadline() {
            Countdown::update(&self.countdown, cx.app_mut(), deadline);
        }
        self.sync_refresh_control(cx)?;

        Ok(())
    }

    /// The refresh control on the right, and the countdown beside it.
    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Right {
            return Ok(());
        }
        // The pane may rebuild the row it calls this for: the control this widget
        // built into the previous one is the widget's, so it goes before the new one
        // is built (a pane that reuses one group node must not accumulate controls).
        self.release_refresh_control(cx, true)?;
        self.build_refresh_control(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        // The header's nodes belong to the pane, which removes its row: only the
        // component's signal scope and action handler are this widget's to release.
        let _ = self.release_refresh_control(cx, false);
        // The countdown tick is this widget's: the framework's subtree teardown does
        // not own it, so release is where it is cancelled.
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.body_root = None;
        self.reading = None;
        self.ready = None;
        self.accepted = None;
        self.next_refresh_at = None;
    }
}

impl PentagonPizzaIndexWidget {
    /// The seam the framework and the tests build a pane through.
    ///
    /// The pane has no opinion about where its index comes from, so the source is an
    /// argument: the production constructor answers it with the injected Kytrix service
    /// connection, and a test with a fake one. The clock is an argument for the same
    /// reason: a countdown is measured against a wall clock, and a test that drives the
    /// ticks has to own that clock.
    fn assemble<S: PizzaSource>(
        source: Option<S>,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding = source.map(|source| WidgetBinding::spawn(cx, PizzaService::new(source)));
        let reading_signal: Signal<Option<PizzaReading>> = cx.signal(None);
        let countdown = Rc::new(Countdown::new(cx, now));
        let refresh_requests = cx.signal(0_u64);
        // The pane records the account it was built for, but it does not start
        // anything: the framework calls `load` once construction is done.
        let account_id = cx.account();

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            reading: None,
            reading_signal,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            account_id,
            refresh_requests,
            seen_refresh_requests: 0,
            body_root: None,
            refresh_button: None,
            refresh_root: None,
            refresh_parent: None,
            refresh_language: None,
        }
    }

    /// A pane over a source the caller answers, for the crate's own tests.
    #[cfg(test)]
    fn new_with_source<S: PizzaSource>(
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
    fn new_with_source_at_clock<S: PizzaSource>(
        source: S,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    /// The account this pane would accept an answer from.
    fn current_key(&self) -> Option<PizzaStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| PizzaStreamKey { account_id })
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
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.ready = None;
        self.next_refresh_at = None;
        self.accepted = None;
        // The command that was in flight belongs to the generation this call
        // replaces, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        self.pending.clear();
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for. The
            // stream the pane was reading belongs to the account that left, so it is
            // stopped rather than left polling for an answer nobody can draw.
            self.set_reading(None, cx);
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            );
            // The answer to a stop is nothing the pane reports: it is already
            // waiting for the account that would start the next read.
            if let Some(binding) = self.binding.as_mut() {
                let request_id = binding.call(cx, PizzaRequest::Unload);
                self.pending.track(request_id, PendingCommand::Unload);
            }
            cx.request_frame()?;
            return Ok(());
        };
        if !retry {
            self.set_reading(None, cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let Some(binding) = self.binding.as_mut() else {
            // The host installed no Kytrix service connection: there is nothing this
            // pane can read with, and that is a failure rather than a wait.
            self.fail(PizzaFetchError::Service(KytrixServiceError::Closed), cx);
            return Ok(());
        };
        let request_id = binding.call(cx, PizzaRequest::Load(key));
        self.pending.track(request_id, PendingCommand::Load);
        cx.request_frame()?;

        Ok(())
    }

    /// Publishes one reading (or none) to the body's `bind_*` closures.
    fn set_reading(&mut self, reading: Option<PizzaReading>, cx: &mut WidgetCx<'_>) {
        self.reading = reading;
        cx.write(&self.reading_signal, reading);
    }

    /// Reports a failure and offers the framework's recovery.
    fn fail(&mut self, error: PizzaFetchError, cx: &mut WidgetCx<'_>) {
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(PizzaFailure::Fetch(error)), recovery),
        );
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

    /// Applies one RPC reply, attributed by the identity the binding returned.
    ///
    /// The binding replaces and aborts the command before it on every issue, but an
    /// answer can already be queued when that happens. The table says which command the
    /// answer belongs to: an identity it does not know answers a command this pane
    /// replaced, and is dropped and counted instead of being applied to the command now
    /// in flight (the mis-attribution that used to let a replaced refresh's failure clear
    /// the load that replaced it).
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: Result<PizzaResponse, RpcCallError<PizzaServiceError>>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Load) => self.apply_load_result(result, cx),
            Some(PendingCommand::Refresh) => self.apply_refresh_result(result, cx),
            // The answer to the stop of a stream nobody follows: nothing to report.
            Some(PendingCommand::Unload) => {}
            None => log::debug!(
                "the Pentagon Pizza Index pane ignored reply {request_id}: no command is waiting for it"
            ),
        }
    }

    fn apply_load_result(
        &mut self,
        result: Result<PizzaResponse, RpcCallError<PizzaServiceError>>,
        cx: &mut WidgetCx<'_>,
    ) {
        // A failure is terminal: the read's answer may not repaint the failure the user
        // is reading (AGENTS §4.4).
        if self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(PizzaFailure::Rpc(error)), recovery),
            );
        }
    }

    fn apply_refresh_result(
        &mut self,
        result: Result<PizzaResponse, RpcCallError<PizzaServiceError>>,
        cx: &mut WidgetCx<'_>,
    ) {
        // An accepted command stays pending until the round it started is reported: the
        // acknowledgement and the round are two different messages. A round already
        // reported (or a failed pane) leaves the acknowledgement nothing to say.
        if !self.refresh_pending || self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(PizzaFailure::Rpc(error)), recovery),
            );
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a status is drawn by the
    /// framework around the body, so publishing it is enough.
    fn apply_reliable(
        &mut self,
        key: PizzaStreamKey,
        message: Arc<PizzaStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let PizzaStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        self.next_refresh_at = status.next_refresh_at();

        match status {
            PizzaStatus::Connecting => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            PizzaStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            PizzaStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            PizzaStatus::Failed(error) => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(PizzaFailure::Fetch(error.clone())), recovery),
                );
            }
        }

        false
    }

    /// Applies a reading the stream published.
    ///
    /// Returns whether the reading changed, which is what the pane's signals turn into
    /// a repaint: a reading identical to the one on screen is not worth a frame.
    fn apply_latest(
        &mut self,
        key: PizzaStreamKey,
        message: Arc<PizzaStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
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
        let changed = self.reading != Some(snapshot.reading());
        self.set_reading(Some(snapshot.reading()), cx);
        self.accepted = Some(snapshot.revision());
        // Readiness is published at the protocol's own boundary: a complete reading
        // is not the same thing as a reading a status said was complete, and this is
        // where the two meet.
        if self.ready.is_some_and(|ready| snapshot.revision() >= ready) {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }

        changed
    }

    /// The deadline this pane counts down to, if it has one.
    ///
    /// A countdown belongs to a ready pane: it counts down the time left until the
    /// next automatic read of the reading that is on screen. While a read is in
    /// flight there is nothing to count down to — the deadline the service
    /// announced belongs to the read after the one being waited for.
    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
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
    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    /// Reads the index again without waiting for the next interval.
    fn refresh(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.refresh_is_disabled(cx.app()) {
            return Ok(());
        }
        self.refresh_pending = true;
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, PizzaRequest::Refresh);
            self.pending.track(request_id, PendingCommand::Refresh);
        }
        cx.request_frame()?;

        Ok(())
    }

    /// Builds the rung's number, which the index itself publishes.
    fn build_level_number(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let mut style = TextStyle::new(UnifiedSize::Medium.text_size());
        style.weight = LEVEL_NUMBER_WEIGHT;
        NodeBuilder::text(SmolStr::default(), style)
            .id("TEST_PENTAGON_PIZZA_INDEX_NUMBER")
            .bind_text(move |read| {
                let _language = read.read(&language);
                let reading = *read.read(&reading);
                reading.map_or_else(SmolStr::default, |reading| {
                    t_smol!(
                        translate,
                        "widget.pentagon_pizza_index.level_number",
                        level = reading.level().number()
                    )
                })
            })
            .bind_style(move |read| {
                let level = *read.read(&reading);
                StylePatch {
                    foreground: Some(theme.role(read, reading_role(level))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    /// Builds the rung's name, which is the readiness the ladder states.
    fn build_level_name(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let mut style = TextStyle::new(LEVEL_NAME_SIZE);
        style.weight = LEVEL_NAME_WEIGHT;
        NodeBuilder::text(SmolStr::default(), style)
            .id("TEST_PENTAGON_PIZZA_INDEX_NAME")
            .bind_text(move |read| {
                let _language = read.read(&language);
                let reading = *read.read(&reading);
                reading.map_or_else(SmolStr::default, |reading| reading.level().name(&translate))
            })
            .bind_style(move |read| {
                let level = *read.read(&reading);
                StylePatch {
                    foreground: Some(theme.role(read, reading_role(level))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    /// Builds the rung's colour written across the pane: a reader who has scrolled past
    /// everything else still sees which rung it is.
    fn build_level_bar(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let theme = cx.theme().clone();
        NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX_BAR")
            .style(
                StyleBuilder::new()
                    .width_full()
                    .max_width(LEVEL_BAR_MAX_WIDTH)
                    .height(LEVEL_BAR_HEIGHT)
                    .build(),
            )
            .paint(PaintSpec {
                corner_radius: [LEVEL_BAR_RADIUS; 4],
                ..PaintSpec::default()
            })
            .bind_style(move |read| {
                let level = *read.read(&reading);
                StylePatch {
                    background: Some(theme.role(read, reading_role(level))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    /// Builds the moment the reading was observed at, read in the display zone.
    ///
    /// It is formatted on the frame that draws it rather than when the reading
    /// arrived: the display zone can move while the pane is open, and a clock
    /// formatted once is a clock frozen into the snapshot.
    fn build_observed_at(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let config = cx.services().config;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_PENTAGON_PIZZA_INDEX_OBSERVED")
        .bind_text(move |read| {
            let _language = read.read(&language);
            let reading = *read.read(&reading);
            let settings = read.read(&config);
            let zone = data_time_zone(&settings);
            reading.map_or_else(SmolStr::default, |reading| {
                t_smol!(
                    translate,
                    "widget.pentagon_pizza_index.observed_at",
                    time = format_observed_at(reading.observed_at(), zone)
                )
            })
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    /// Builds the pane's refresh control into `parent`.
    ///
    /// The label is the catalogue's sentence for the control, so it is chosen when the
    /// control is built and rebuilt by [`sync_refresh_control`](Self::sync_refresh_control)
    /// when the language moves (the kit's static-copy convention, `widgets/AGENTS.md`
    /// §15). The countdown beside it is a `bind_text` node, so it follows its signal
    /// without a rebuild.
    fn build_refresh_control(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let label = t_smol!(cx.translate(), "widget.pentagon_pizza_index.refresh_now");
        let theme = cx.theme().clone();
        let window = cx.window();
        let countdown = self.countdown.label;
        let countdown_theme = cx.theme().clone();
        // The control is built in the state the pane is in now, not in the state the
        // previous frame left it in: a pane that has nothing to refresh must not draw
        // one frame of a live button (AGENTS §4.2).
        let disabled = self.refresh_is_disabled(cx.app());

        let root = NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(UnifiedSize::Small.gap())
                    .flex_none()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let requests = self.refresh_requests;
        // The key is the control's application-level identity, and this pane is one of
        // however many the layout holds: `unique_key` scopes it to this widget's own
        // scope, so two panes never share one handler.
        let key = cx.unique_key("pentagon_pizza_index.refresh");
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            // The kit's compact ladder draws the icon at 8 px where the old
            // `size_3()` drew it at 12 px; that is the frozen kit's size rather than
            // this pane's choice, registered as a visual difference (W2b test-ledger
            // §6) instead of hand-building the icon here.
            .icon(IconKind::ArrowsClockwise)
            .disabled(disabled)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                // A button handler owns no widget; it asks the next sync for the refresh.
                ctx.update(&requests, |requests| *requests += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, root)?;

        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_PENTAGON_PIZZA_INDEX_COUNTDOWN")
        .style(StyleBuilder::new().flex_none().build())
        .bind_text(move |read| {
            let label: Option<SmolStr> = (*read.read(&countdown)).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(countdown_theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), root)?;

        self.refresh_button = Some(button);
        self.refresh_root = Some(root);
        self.refresh_parent = Some(parent);
        self.refresh_language = Some(language);

        Ok(())
    }

    /// Keeps the control's state in step and rebuilds it when its static copy moved.
    fn sync_refresh_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(button) = self.refresh_button.as_ref() else {
            return Ok(());
        };
        button.sync(cx.app_mut())?;
        let disabled = self.refresh_is_disabled(cx.app());
        let _ = button.set_disabled(cx.app_mut(), disabled);

        if self.refresh_language == Some(*cx.read(&cx.language())) {
            return Ok(());
        }
        let Some(parent) = self.refresh_parent else {
            return Ok(());
        };
        // The pane may have rebuilt the header row since: its group is not this
        // widget's node, so a stale parent means the control already went with it.
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }

        self.release_refresh_control(cx, true)?;
        self.build_refresh_control(parent, cx)
    }

    /// Removes the control's nodes (when the widget owns the teardown) and releases the
    /// component's scope and action handler.
    fn release_refresh_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.refresh_button.take() else {
            self.refresh_root = None;
            self.refresh_parent = None;
            self.refresh_language = None;

            return Ok(());
        };
        if remove_node && let Some(root) = self.refresh_root.take() {
            remove_own_node(cx, root)?;
        }
        let _ = button.release(cx.app_mut());
        self.refresh_root = None;
        self.refresh_parent = None;
        self.refresh_language = None;

        Ok(())
    }
}

/// The theme role one rung is drawn in.
///
/// The ladder is a severity, so it reads the status roles rather than a palette of
/// its own: that is what keeps two appearances legible and keeps a red a reader has
/// learned from an error banner meaning the same thing here.
const fn level_role(level: PizzaLevel) -> Role {
    match level {
        PizzaLevel::FadeOut => Role::StatusSuccessBase,
        PizzaLevel::DoubleTake => Role::StatusInfoBase,
        PizzaLevel::RoundHouse => Role::StatusWarningBase,
        PizzaLevel::FastPace => Role::StatusElevatedBase,
        PizzaLevel::ExerciseTerm => Role::StatusDangerBase,
    }
}

/// The role a reading's rung names, or the neutral foreground when there is none.
fn reading_role(reading: Option<PizzaReading>) -> Role {
    reading.map_or(Role::FgDefault, |reading| level_role(reading.level()))
}

/// The label beside the refresh button for `deadline` at `now`.
///
/// A deadline that has already passed counts as `00:00` rather than as a negative
/// wait, which is the same rule [`format_countdown`] applies: the pane never shows a
/// countdown that has run out as anything but zero.
fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

/// Removes a node this pane built, if it is still in the tree.
///
/// A node that is already gone is not a failure: the pane rebuilds its header row, and
/// the control this widget built into the old row went with it.
fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

/// The moment a reading was observed at, read in the display zone.
///
/// It is formatted on the frame that draws it rather than when the reading arrived:
/// the display zone can move while the pane is open, and a clock formatted once is a
/// clock frozen into the snapshot.
fn format_observed_at(observed_at: i64, zone: UtcOffset) -> SmolStr {
    // The offset can move an instant past the last date the calendar holds, and an
    // instant the index could not have published is written as a placeholder rather
    // than taken to a date that is not one.
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(observed_at) * 1_000_000)
        .ok()
        .and_then(|time| time.checked_to_offset(zone))
        .map_or_else(
            || SmolStr::new_static("--"),
            |time| {
                format_smolstr!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    time.year(),
                    u8::from(time.month()),
                    time.day(),
                    time.hour(),
                    time.minute()
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use base_ui::theme::{appearance::Appearance, resolved::Theme};
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use ui_core::{
        event::{Action, ActionSource},
        paint::color::Color,
    };
    use widget::{protocol::RequestId, test_support::WidgetTestApp};

    use super::*;
    use crate::{
        model::PizzaDataError,
        service::{
            PizzaResponse, PizzaServiceError, PizzaSnapshot, PizzaStatus, PizzaStreamMessage,
        },
        test_support::{ACCOUNT, FakeSource, FetchPlan, OBSERVED_AT, reading},
    };

    /// The translator the product ships: what these tests assert is the catalogue's copy.
    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    /// The framework's headless host, over the product catalogue.
    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(product_translate(LanguageCode::En))
    }

    fn current_config(harness: &WidgetTestApp) -> Config {
        let signal = harness.services().config;
        (*harness.app().read(&signal)).clone()
    }

    fn write_config(harness: &mut WidgetTestApp, config: Config) -> bool {
        let signal = harness.services().config;
        harness.app_mut().write(&signal, config)
    }

    /// Signs `user_id` in the way the composition root does: a settings write.
    fn sign_in(harness: &mut WidgetTestApp, user_id: &str) {
        let mut config = current_config(harness);
        config.account = Some(AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        assert!(
            write_config(harness, config),
            "signing in is a settings change"
        );
    }

    /// Signs the pane out the way the composition root does: a settings write.
    fn sign_out(harness: &mut WidgetTestApp) {
        let mut config = current_config(harness);
        config.account = None;
        assert!(
            write_config(harness, config),
            "signing out is a settings change"
        );
    }

    /// A pane the framework would have built, driven by a fake subscription.
    struct Pane {
        widget: PentagonPizzaIndexWidget,
        status: WidgetStatus,
        audience_parent: NodeId,
    }

    fn recovery() -> WidgetAction {
        WidgetAction::new("common.retry", |_| {})
    }

    fn pane_with_source(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            PentagonPizzaIndexWidget::new_with_source(source, status, recovery(), cx)
        });

        Pane {
            widget,
            status,
            audience_parent: parent,
        }
    }

    /// A pane with no source a stream could answer through.
    fn pane_without_source(harness: &mut WidgetTestApp) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            PentagonPizzaIndexWidget::new(
                WidgetInstanceId::new(),
                PentagonPizzaIndexConfig,
                status,
                ReloadHandle::new(|_| {}),
                WidgetConfigChange::default(),
                recovery(),
                cx,
            )
            .expect("the pane constructs")
        });

        Pane {
            widget,
            status,
            audience_parent: parent,
        }
    }

    fn pane(harness: &mut WidgetTestApp) -> Pane {
        pane_with_source(harness, FakeSource::default())
    }

    /// A wall clock the test owns, and how many times the pane read it.
    ///
    /// A countdown's label is formatted against the wall clock, which is not the
    /// engine clock [`WidgetTestApp::advance`] moves; the read count is what tells one
    /// timer chain from two (two chains tick twice per round).
    struct TestClock {
        now: Rc<Cell<i64>>,
        reads: Rc<Cell<usize>>,
    }

    impl TestClock {
        fn new(now: i64) -> Self {
            Self {
                now: Rc::new(Cell::new(now)),
                reads: Rc::new(Cell::new(0)),
            }
        }

        fn set(&self, now: i64) {
            self.now.set(now);
        }

        fn reads(&self) -> usize {
            self.reads.get()
        }

        fn now(&self) -> Rc<dyn Fn() -> i64> {
            let now = Rc::clone(&self.now);
            let reads = Rc::clone(&self.reads);
            Rc::new(move || {
                reads.set(reads.get() + 1);
                now.get()
            })
        }
    }

    /// A pane over a source and a wall clock a test drives.
    fn pane_with_source_at_clock(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        now: Rc<dyn Fn() -> i64>,
    ) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            PentagonPizzaIndexWidget::new_with_source_at_clock(source, now, status, recovery(), cx)
        });

        Pane {
            widget,
            status,
            audience_parent: parent,
        }
    }

    fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness.with_cx(|cx| pane.widget.load(cx));
    }

    fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness
            .with_cx(|cx| pane.widget.sync(cx))
            .expect("the widget syncs");
    }

    /// Builds the body the framework would build once the pane has content.
    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.audience_parent;
        harness
            .with_cx(|cx| pane.widget.body(parent, cx))
            .expect("the body builds");
    }

    /// Builds the header right group, as a pane would.
    fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.audience_parent;
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, parent, cx))
            .expect("the header builds");
    }

    /// One production-shaped frame: synchronization, then the pipeline.
    fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
        sync(harness, pane);
        harness.drain();
        harness.drain();
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
    fn report_status(harness: &mut WidgetTestApp, pane: &mut Pane, status: PizzaStatus) -> bool {
        report_from(harness, pane, key(), PizzaStreamMessage::Status(status))
    }

    /// Reports a message the stream addressed to `key`.
    fn report_from(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        key: PizzaStreamKey,
        message: PizzaStreamMessage,
    ) -> bool {
        harness.with_cx(|cx| match message {
            PizzaStreamMessage::Status(_) => pane.widget.apply_reliable(key, Arc::new(message), cx),
            PizzaStreamMessage::Snapshot(_) => pane.widget.apply_latest(key, Arc::new(message), cx),
        })
    }

    /// Reports a reading as the stream would, at `revision`.
    fn report_reading(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        revision: u64,
        reading: PizzaReading,
    ) -> bool {
        report_from(
            harness,
            pane,
            key(),
            PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(revision, reading))),
        )
    }

    /// A pane that is ready at `due` and holding `level`, built and framed.
    fn ready_pane(harness: &mut WidgetTestApp, level: u8, due: i64) -> Pane {
        sign_in(harness, ACCOUNT);
        let mut pane = pane(harness);
        report_reading(harness, &mut pane, 1, reading(level));
        report_status(
            harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: due,
            },
        );
        pane
    }

    /// A pane that is ready at `due`, counting down on a clock the test owns.
    fn ready_pane_at_clock(
        harness: &mut WidgetTestApp,
        level: u8,
        due: i64,
        now: Rc<dyn Fn() -> i64>,
    ) -> Pane {
        sign_in(harness, ACCOUNT);
        let mut pane = pane_with_source_at_clock(harness, FakeSource::default(), now);
        report_reading(harness, &mut pane, 1, reading(level));
        report_status(
            harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: due,
            },
        );
        pane
    }

    fn reported(pane: &Pane, harness: &WidgetTestApp) -> WidgetState {
        pane.status.state(harness.app())
    }

    fn level_of(pane: &Pane) -> Option<PizzaLevel> {
        pane.widget.reading.map(|reading| reading.level())
    }

    fn node_with_prefix(harness: &WidgetTestApp, parent: NodeId, prefix: &str) -> NodeId {
        harness
            .find_by_debug_prefix(parent, prefix)
            .unwrap_or_else(|| panic!("the pane built no node with `{prefix}`"))
    }

    fn text_node(harness: &WidgetTestApp, prefix: &str) -> String {
        let node = node_with_prefix(harness, harness.root(), prefix);
        harness.text_of(node)
    }

    fn bar_fill(harness: &WidgetTestApp) -> Color {
        let bar = node_with_prefix(harness, harness.root(), "TEST_PENTAGON_PIZZA_INDEX_BAR");
        harness
            .tree()
            .get(bar)
            .expect("the bar node exists")
            .style_patch()
            .background
            .expect("the bar's colour is written by its bind_style")
    }

    /// The value one of the pane's signals holds.
    ///
    /// A read outside a `bind_*` closure registers no dependency — which is what a
    /// test wants: the value the pane wrote, not a dependency edge.
    fn read<T: Clone + 'static>(harness: &WidgetTestApp, signal: &Signal<T>) -> T {
        (*harness.app().read(signal)).clone()
    }

    // ── model-adjacent rendering ─────────────────────────────────────────────

    /// A reading draws the moment it was observed at, read in the display zone rather
    /// than in UTC: the clock is resolved on the frame, so the zone can move.
    #[test]
    fn a_reading_should_draw_the_moment_it_was_observed_at() {
        assert_eq!(
            format_observed_at(OBSERVED_AT, UtcOffset::UTC).as_str(),
            "2026-10-24 06:00",
            "the fixture reading read as a wall clock in UTC"
        );
        assert_eq!(
            format_observed_at(OBSERVED_AT, UtcOffset::from_hms(8, 0, 0).unwrap()).as_str(),
            "2026-10-24 14:00",
            "the display zone moves the clock without moving the instant"
        );
        assert_eq!(
            format_observed_at(i64::MAX, UtcOffset::UTC).as_str(),
            "--",
            "an instant past the calendar is a placeholder, not a made-up date"
        );
    }

    /// The countdown is one function of the deadline and the clock: no deadline is no
    /// label, a deadline in the past is zero, and a rounded-down one never reads lower
    /// than the time left.
    #[test]
    fn a_countdown_should_be_read_from_the_deadline_and_the_clock() {
        assert_eq!(countdown_label(None, 1_000_000), None);
        assert_eq!(
            countdown_label(Some(1_600_000), 1_000_000),
            Some(SmolStr::new_static("10:00")),
            "a ten-minute wait reads as ten minutes"
        );
        assert_eq!(
            countdown_label(Some(1_598_001), 1_000_000),
            Some(SmolStr::new_static("09:59")),
            "a second left is never rounded away"
        );
        assert_eq!(
            countdown_label(Some(1_000), 1_000_000),
            Some(SmolStr::new_static("00:00")),
            "a deadline that has passed is zero, not a negative wait"
        );
        assert_eq!(
            countdown_label(Some(4_600_000), 1_000_000),
            Some(SmolStr::new_static("01:00:00")),
            "hours appear only when there are hours left"
        );
    }

    // ── state machine ────────────────────────────────────────────────────────

    /// The identity the registry and the persisted layout name this widget by.
    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(
            PentagonPizzaIndexWidget::ID.as_str(),
            "kytrix.pentagon_pizza_index"
        );
        assert_eq!(
            PentagonPizzaIndexWidget::DISPLAY_NAME,
            "widget.names.pentagon_pizza_index"
        );
        assert_eq!(
            PentagonPizzaIndexWidget::CATEGORY,
            WidgetCategory::Monitoring
        );
        assert_eq!(
            PentagonPizzaIndexWidget::DESCRIPTION,
            Some("widget.descriptions.pentagon_pizza_index")
        );
        assert_eq!(
            PentagonPizzaIndexWidget::ICON,
            Some("widgets/pentagon_pizza_index.png")
        );
        assert_eq!(
            <PentagonPizzaIndexConfig as WidgetConfig>::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    /// A ready pane is content once a reading is published for it, and the reading is
    /// what it draws.
    #[test]
    fn a_reading_should_be_the_panes_content() {
        let mut harness = harness();
        let pane = ready_pane(&mut harness, 3, 0);

        assert!(pane.widget.has_content());
        assert_eq!(reported(&pane, &harness), WidgetState::Ready);
        assert_eq!(level_of(&pane), Some(PizzaLevel::RoundHouse));
    }

    /// A failure is reported as a failure, holds no reading, and leaves the pane with
    /// nothing the framework could draw as content.
    #[test]
    fn a_failed_round_should_report_a_failure_and_hold_no_reading() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Data(PizzaDataError::UnknownLevel {
                level: 9,
            })),
        );

        assert!(pane.status.is_failed(harness.app()));
        assert!(
            !pane.widget.has_content(),
            "a failed read is not content: the framework draws the failure placeholder"
        );
        assert!(pane.widget.reading.is_none());
    }

    /// A message about a scope this pane is not following is not one it may act on,
    /// and a pane that has failed is finished until the user recovers it.
    #[test]
    fn a_late_message_should_not_revive_a_failed_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Closed)),
        );
        report_reading(&mut harness, &mut pane, 1, reading(1));

        assert!(pane.status.is_failed(harness.app()));
        assert!(
            pane.widget.reading.is_none(),
            "a reading that arrives after a failure is about a stream that is gone"
        );
    }

    /// Another account is another scope: an answer served to the one that just left
    /// is not one this pane may draw.
    #[test]
    fn a_message_about_another_account_should_not_be_drawn() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        // The pane's own account is accepted first, so the assertion below is
        // about the key rather than about a pane that never took anything.
        assert!(report_reading(&mut harness, &mut pane, 1, reading(3)));
        assert_eq!(level_of(&pane), Some(PizzaLevel::RoundHouse));

        report_from(
            &mut harness,
            &mut pane,
            other_key(),
            PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(2, reading(1)))),
        );

        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::RoundHouse),
            "the reading served to another account did not replace the one on screen"
        );
        assert_eq!(
            pane.widget.accepted,
            Some(1),
            "and it did not take the revision with it either"
        );
    }

    /// A reading the pane has already overtaken is not one it may draw: within one
    /// generation the revision only moves forwards.
    #[test]
    fn an_older_revision_should_not_replace_the_reading_on_screen() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 2,
                next_refresh_at: 0,
            },
        );
        report_reading(&mut harness, &mut pane, 2, reading(1));
        report_reading(&mut harness, &mut pane, 1, reading(5));

        assert_eq!(
            pane.widget.accepted,
            Some(2),
            "the reading on screen is still revision two"
        );
        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::ExerciseTerm),
            "the older reading did not replace the one on screen"
        );
    }

    /// A retry keeps the reading the user is looking at but not the revision it was
    /// counted at: a load replaces the stream, and the one that replaces it numbers
    /// its readings from one again.
    ///
    /// A revision kept across that boundary would refuse every reading the new
    /// stream publishes until its count caught up — the pane would keep a failed
    /// hour's level on screen and report readiness over it.
    #[test]
    fn a_retry_should_keep_the_reading_and_drop_its_revision() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_reading(&mut harness, &mut pane, 2, reading(1));
        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Closed)),
        );
        harness
            .with_cx(|cx| pane.widget.start(true, cx))
            .expect("the retry starts");

        assert_eq!(
            pane.widget.accepted, None,
            "the revision numbers one stream, and this is another one"
        );
        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::ExerciseTerm),
            "the reading the user was looking at is still there"
        );
        assert!(
            pane.widget.has_content(),
            "a pane that kept its reading still has something to draw"
        );

        assert!(
            report_reading(&mut harness, &mut pane, 1, reading(5)),
            "the first reading of the new stream is a change"
        );
        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::FadeOut),
            "and it is what the pane now draws"
        );
    }

    /// A loading reader who signs out is waiting for an account, not for the load it
    /// no longer follows: the answer belongs to the stream that left with the
    /// account, and reporting it as a failure would hide the account request.
    #[test]
    fn a_load_answer_for_a_reader_that_signed_out_should_not_fail_the_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "the load answered for the account that left"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the answer to the replaced load is dropped and counted"
        );
    }

    /// The same for a refresh: the command belonged to the stream the sign-out
    /// replaced, so its answer is not one this pane may report.
    #[test]
    fn a_refresh_answer_for_a_reader_that_signed_out_should_not_fail_the_pane() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        sync(&mut harness, &mut pane);

        harness
            .with_cx(|cx| pane.widget.refresh(cx))
            .expect("the refresh starts");
        assert!(
            pane.widget.refresh_pending,
            "the click puts a refresh in flight"
        );

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "the refresh answered for the account that left"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the answer to the replaced refresh is dropped and counted"
        );
    }

    /// A reader the service does not know is told what it is waiting for rather than
    /// being sent after an answer nobody will serve it.
    #[test]
    fn a_signed_out_load_should_wait_for_an_account() {
        let mut harness = harness();
        let mut pane = pane(&mut harness);

        load(&mut harness, &mut pane);

        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out"))
        );
        assert!(!pane.widget.has_content());
        assert!(pane.widget.current_key().is_none());
    }

    /// A reading identical to the one on screen is not worth a frame, which is what
    /// keeps a quiet hour from scheduling a repaint.
    #[test]
    fn a_reading_that_did_not_change_should_not_ask_for_a_frame() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: 0,
            },
        );
        assert!(report_reading(&mut harness, &mut pane, 1, reading(3)));
        assert!(
            !report_reading(&mut harness, &mut pane, 1, reading(3)),
            "the same reading twice is one change"
        );
    }

    /// Readiness is published at the protocol's own boundary: a reading that arrived
    /// before the status describing it leaves the pane loading, and the status then
    /// makes it ready.
    #[test]
    fn the_two_channels_should_meet_at_the_readings_revision() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_reading(&mut harness, &mut pane, 1, reading(2));
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::Connecting,
            "a reading without its status is a pane that is still connecting"
        );

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: 0,
            },
        );
        assert_eq!(reported(&pane, &harness), WidgetState::Ready);
    }

    /// The countdown belongs to a ready pane: it counts down the time left until the
    /// next automatic read of the reading on screen.
    #[test]
    fn the_countdown_should_belong_to_a_ready_pane() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        frame(&mut harness, &mut pane);

        assert!(
            read(&harness, &pane.widget.countdown.label).is_some(),
            "a ready pane counts down to its next read"
        );
        assert!(!pane.widget.refresh_is_disabled(harness.app()));

        // A pane that fails has nothing to bring forward.
        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Timeout)),
        );
        frame(&mut harness, &mut pane);
        assert!(
            read(&harness, &pane.widget.countdown.label).is_none(),
            "a failed pane is not counting down to anything"
        );
        assert!(pane.widget.refresh_is_disabled(harness.app()));
    }

    /// Signing out takes the reading with it — and the revision, the deadline and the
    /// countdown counted against it — and asks for an account rather than reporting a
    /// failure: nothing has gone wrong, the pane is just not for an anonymous reader.
    #[test]
    fn signing_out_should_take_the_reading_with_it() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.has_content());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);

        assert!(!pane.widget.has_content());
        assert!(
            pane.widget.reading.is_none(),
            "the reading was served to an account that just left"
        );
        assert_eq!(pane.widget.accepted, None);
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "a signed-out pane waits for an account"
        );
        assert!(pane.widget.current_key().is_none());
        assert!(
            read(&harness, &pane.widget.countdown.label).is_none(),
            "a pane waiting for an account is not counting down to a read"
        );
    }

    /// Another account is another read, and the reading of the one that left goes
    /// with it.
    #[test]
    fn another_account_should_take_the_reading_with_it() {
        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 3, 0);

        assert!(pane.widget.has_content());
        sign_in(&mut harness, "second-account");
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.has_content(),
            "the reading was served to the account that just left"
        );
        assert_eq!(reported(&pane, &harness), WidgetState::Connecting);
        assert_eq!(
            pane.widget.current_key(),
            Some(other_key()),
            "the pane follows the account the settings now hold"
        );
    }

    /// The pane follows the account through the settings snapshot, not only through a
    /// reload: a reader who signs in is re-scoped, and one who signs out is asked for
    /// an account rather than left with the reading of the account that left.
    #[test]
    fn an_account_change_should_re_scope_the_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);

        report_reading(&mut harness, &mut pane, 1, reading(3));
        assert!(pane.widget.has_content());

        sign_in(&mut harness, "second-account");
        sync(&mut harness, &mut pane);
        assert_eq!(
            pane.widget.account_id.as_deref(),
            Some("second-account"),
            "the pane reads the account from the settings snapshot"
        );
        assert!(
            !pane.widget.has_content(),
            "the reading was served to the account that left"
        );
        assert_eq!(pane.widget.current_key(), Some(other_key()));

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "signing out asks for an account"
        );
    }

    // ── body rendering ───────────────────────────────────────────────────────

    /// The body draws the ladder's number, its name, the rung's bar and the moment the
    /// reading was observed at — each from the reading signal, once the framework has
    /// built the body.
    #[test]
    fn the_body_should_draw_the_level_its_number_its_bar_and_the_moment() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NUMBER"),
            "Level 3"
        );
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NAME"),
            "Increase in force readiness"
        );
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED"),
            "Observed at 2026-10-24 06:00"
        );

        let bar = node_with_prefix(&harness, harness.root(), "TEST_PENTAGON_PIZZA_INDEX_BAR");
        let bounds = harness
            .tree()
            .get(bar)
            .expect("the bar exists")
            .layout()
            .bounds;
        assert!(
            (bounds.size.height - LEVEL_BAR_HEIGHT).abs() < f32::EPSILON,
            "the bar is one thin rule under the reading: {}",
            bounds.size.height
        );
        assert!(
            bounds.size.width > 0.0 && bounds.size.width <= LEVEL_BAR_MAX_WIDTH + f32::EPSILON,
            "the bar is written across the pane up to its cap: {}",
            bounds.size.width
        );
        assert_eq!(
            harness
                .tree()
                .get(bar)
                .expect("the bar")
                .style_patch()
                .background,
            Some(Theme::of(Appearance::Light).role(level_role(PizzaLevel::RoundHouse))),
            "the bar is the rung's status colour"
        );
    }

    /// The pane leaves the theme's own colours alone: the ladder is a severity and
    /// each rung maps to its status role, so two rungs never share a colour, and a
    /// dark appearance draws the dark role rather than the light one.
    #[test]
    fn every_rung_should_be_drawn_in_its_own_theme_colour() {
        let light = Theme::of(Appearance::Light);
        let dark = Theme::of(Appearance::Dark);
        let levels = [
            PizzaLevel::FadeOut,
            PizzaLevel::DoubleTake,
            PizzaLevel::RoundHouse,
            PizzaLevel::FastPace,
            PizzaLevel::ExerciseTerm,
        ];
        let colours: Vec<_> = levels
            .iter()
            .map(|level| light.role(level_role(*level)))
            .collect();
        for (index, colour) in colours.iter().enumerate() {
            assert!(
                !colours[..index].contains(colour),
                "two rungs share a colour, so two readings would look the same"
            );
        }
        // The ladder's colour *is* its severity: the calmest rung is the success
        // colour and the most severe one the danger colour, so a mapping that drifted
        // by one rung would tell a reader the wrong severity.
        for (level, role) in [
            (PizzaLevel::FadeOut, Role::StatusSuccessBase),
            (PizzaLevel::DoubleTake, Role::StatusInfoBase),
            (PizzaLevel::RoundHouse, Role::StatusWarningBase),
            (PizzaLevel::FastPace, Role::StatusElevatedBase),
            (PizzaLevel::ExerciseTerm, Role::StatusDangerBase),
        ] {
            assert_eq!(
                level_role(level),
                role,
                "the rung {level:?} changed severity"
            );
        }
        for level in levels {
            assert_ne!(
                light.role(level_role(level)),
                dark.role(level_role(level)),
                "{level:?} does not follow the appearance"
            );
        }

        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 5, 0);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert_eq!(
            bar_fill(&harness),
            light.role(level_role(PizzaLevel::FadeOut)),
            "the calmest rung is the success colour"
        );

        let theme = harness.services().theme.clone();
        theme.set_appearance(harness.app_mut(), Appearance::Dark);
        harness.drain();
        assert_eq!(
            bar_fill(&harness),
            dark.role(level_role(PizzaLevel::FadeOut)),
            "the bar follows the appearance through bind_style"
        );
    }

    /// The body's copy follows the language: the level's name and the observed-at
    /// sentence are `bind_text` closures that read the language signal, so a language
    /// switch redraws them without the pane re-reporting anything.
    #[test]
    fn the_body_should_follow_the_language() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NAME")
                .contains("Increase in force readiness")
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.drain();
        harness.drain();

        let name = text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NAME");
        assert!(
            !name.is_empty() && !name.contains("Increase in force readiness"),
            "the level's name follows the language: {name}"
        );
        let observed = text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED");
        assert!(
            observed.starts_with("观测于"),
            "the observed-at sentence follows the language: {observed}"
        );
    }

    /// The moment is read in the display zone on the frame that draws it: moving the
    /// setting redraws the observed-at node and nothing else about the reading.
    #[test]
    fn moving_the_display_zone_should_redraw_the_moment() {
        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 3, 0);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED"),
            "Observed at 2026-10-24 06:00"
        );

        let mut config = current_config(&harness);
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        assert!(write_config(&mut harness, config));
        harness.drain();
        harness.drain();

        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED"),
            "Observed at 2026-10-24 14:00"
        );
    }

    // ── header control ───────────────────────────────────────────────────────

    /// The header builds the refresh control and the countdown beside it: the control
    /// is a kit button whose label is catalogue copy, and the countdown is a
    /// `bind_text` node over the pane's countdown signal.
    #[test]
    fn the_header_should_build_the_refresh_control_and_its_countdown() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let refresh = node_with_prefix(
            &harness,
            pane.audience_parent,
            "TEST_PENTAGON_PIZZA_INDEX_REFRESH",
        );
        assert_eq!(
            harness.tree().get(refresh).expect("the control").kind(),
            &ui_core::node::tree::NodeKind::Box
        );
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_COUNTDOWN"),
            "10:00"
        );

        let button = pane
            .widget
            .refresh_button
            .as_ref()
            .expect("the control is kept for release");
        assert_eq!(
            button.action_identity(),
            Some(Action::Custom(format_smolstr!("{}.click", button.key()))),
            "the control's click is an engine action named by the control's key"
        );
        assert!(
            button.key().starts_with("pentagon_pizza_index.refresh@"),
            "the key is this pane's own rather than a constant two panes would share: {}",
            button.key()
        );
        assert!(
            !*harness.app().read(&button.interactive().disabled_signal()),
            "a ready pane can refresh"
        );
    }

    /// The refresh control is drawn disabled when there is nothing to bring forward,
    /// and a disabled control does not act: the click the widget still receives is
    /// refused by the same predicate the drawing uses.
    #[test]
    fn the_refresh_control_should_be_disabled_with_nothing_to_refresh() {
        let mut harness = harness();
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);

        // The state is the control's own from the first frame: a control built while
        // the pane has nothing to refresh must not be drawn live for one frame
        // (AGENTS §4.2).
        let button = pane.widget.refresh_button.as_ref().expect("the control");
        assert!(
            read(&harness, &button.interactive().disabled_signal()),
            "the control is born disabled when there is nothing to refresh"
        );

        frame(&mut harness, &mut pane);
        let button = pane.widget.refresh_button.as_ref().expect("the control");
        assert!(
            read(&harness, &button.interactive().disabled_signal()),
            "a pane waiting for an account has nothing to refresh"
        );

        // A click still reaches the widget (a disabled kit control only draws its
        // state); the refresh path refuses it by the same predicate.
        let window = harness.window();
        let node = button.node().expect("the control is built");
        let action = button.action_identity().expect("the control has a click");
        harness.with_cx(|cx| {
            let _ = cx
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic);
        });
        frame(&mut harness, &mut pane);
        assert!(
            !pane.widget.refresh_pending,
            "the disabled control did not start a refresh"
        );
    }

    /// A click on a live control becomes a refresh on the pane's next `sync`: the
    /// header handler owns no widget, so it asks through a signal.
    #[test]
    fn a_click_should_become_the_refresh_the_pane_starts() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let (node, action) = {
            let button = pane.widget.refresh_button.as_ref().expect("the control");
            (
                button.node().expect("the control is built"),
                button.action_identity().expect("the control has a click"),
            )
        };
        let window = harness.window();
        harness.with_cx(|cx| {
            let _ = cx
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic);
        });
        sync(&mut harness, &mut pane);

        assert!(
            pane.widget.refresh_pending,
            "the click became the refresh the pane started"
        );
    }

    /// A pane may rebuild its header row, and the widget's control goes with the row it
    /// was built into: building the control twice must not leave the first one behind.
    #[test]
    fn rebuilding_the_header_should_not_accumulate_controls() {
        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 2, 0);
        build_header(&mut harness, &mut pane);
        let after_first = harness.node_count();
        assert!(after_first > 0);

        build_header(&mut harness, &mut pane);

        assert_eq!(
            harness.node_count(),
            after_first,
            "the replaced control's nodes were taken down with it"
        );
    }

    /// The control's copy and its label are catalogued copy chosen when it was built,
    /// so a language switch rebuilds it in place: the old control's nodes are removed,
    /// a new control is built under the same key, and its label is the sentence showing
    /// now. (The accessible name is not readable through the kit's public face, so the
    /// rebuild itself — the copy's only lifecycle — is what is pinned here; the
    /// catalogue's Chinese sentence is asserted by `every_message_this_pane_shows_should_resolve`.)
    #[test]
    fn a_language_change_should_rebuild_the_refresh_control() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let old = {
            let button = pane.widget.refresh_button.as_ref().expect("the control");
            (
                button.node().expect("the control is built"),
                button.action_identity(),
            )
        };

        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);

        let rebuilt = pane.widget.refresh_button.as_ref().expect("the control");
        let new_node = rebuilt.node().expect("the rebuilt control is built");
        assert_ne!(new_node, old.0, "the control was rebuilt, not reused");
        assert!(
            harness.tree().get(old.0).is_err(),
            "the English control's nodes were taken down"
        );
        assert_eq!(
            rebuilt.action_identity(),
            old.1,
            "the rebuilt control is the same action: its key is the pane group, not the copy"
        );
        assert_eq!(
            pane.widget.refresh_language,
            Some(LanguageCode::ZhCn),
            "the pane recorded the language it rebuilt the control in"
        );
    }

    // ── countdown lifecycle ──────────────────────────────────────────────────

    /// A tick recomputes the label from the wall clock against the deadline the pane
    /// holds, and only the tick that moved the label asks for a frame (I3).
    #[test]
    fn a_countdown_tick_should_move_the_label_on_the_clock() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert_eq!(countdown_text(&harness), "10:00");

        let requests = harness.frame_requests();
        assert!(
            !harness.advance(Duration::from_millis(125)),
            "half the interval is an idle round"
        );
        assert_eq!(countdown_text(&harness), "10:00");

        clock.set(1_000);
        assert!(
            harness.advance(Duration::from_millis(125)),
            "the tick that reached the deadline drew the label it moved"
        );
        assert_eq!(countdown_text(&harness), "09:59");
        assert!(harness.frame_requests() > requests);
    }

    /// The timer re-arms itself for the next interval, so the label keeps moving
    /// without any polling task.
    #[test]
    fn a_countdown_should_rearm_its_timer_for_each_tick() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.countdown.timer.borrow().is_some());

        for second in 1..=3 {
            clock.set(second * 1_000);
            assert!(
                harness.advance(COUNTDOWN_TICK),
                "tick {second} moved the label"
            );
            assert_eq!(countdown_text(&harness), format!("09:{:02}", 60 - second));
            assert!(
                pane.widget.countdown.timer.borrow().is_some(),
                "the chain re-armed itself after tick {second}"
            );
        }
    }

    /// A failure and a sign-out both take the schedule away, and the pending tick is
    /// cancelled with it rather than left to fire one last time.
    #[test]
    fn a_failed_or_signed_out_pane_should_stop_the_countdown() {
        // A failure takes the schedule away, and the tick with it.
        {
            let clock = TestClock::new(0);
            let mut harness = harness();
            let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
            build_header(&mut harness, &mut pane);
            frame(&mut harness, &mut pane);
            assert!(pane.widget.countdown.timer.borrow().is_some());

            report_status(
                &mut harness,
                &mut pane,
                PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Timeout)),
            );
            frame(&mut harness, &mut pane);
            assert!(
                pane.widget.countdown.timer.borrow().is_none(),
                "the failed pane's tick is cancelled"
            );
            assert!(
                read(&harness, &pane.widget.countdown.label).is_none(),
                "a failed pane is not counting down to anything"
            );
            harness.drain();
            let requests = harness.frame_requests();
            assert!(
                !harness.advance(COUNTDOWN_TICK * 4),
                "a pane that stopped has no timer left to fire"
            );
            assert_eq!(harness.frame_requests(), requests);
            assert_eq!(countdown_text(&harness), "");
        }

        // ... and the same for the reader who signed out: the label does not come
        // back a tick later.
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.countdown.timer.borrow().is_some());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget.countdown.timer.borrow().is_none(),
            "the signed-out pane's tick is cancelled"
        );
        assert!(read(&harness, &pane.widget.countdown.label).is_none());
        harness.drain();
        let requests = harness.frame_requests();
        assert!(!harness.advance(COUNTDOWN_TICK * 4));
        assert_eq!(harness.frame_requests(), requests);
        assert_eq!(
            countdown_text(&harness),
            "",
            "the label does not come back after the account left"
        );
    }

    /// Releasing the pane cancels the tick: nothing the pane registered may keep waking
    /// the engine (or writing a signal whose scope is gone) after it is released.
    #[test]
    fn releasing_the_pane_should_cancel_the_countdown_timer() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        assert!(pane.widget.countdown.timer.borrow().is_some());

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(
            pane.widget.countdown.timer.borrow().is_none(),
            "release cancels the tick"
        );
        assert!(read(&harness, &pane.widget.countdown.label).is_none());
        harness.drain();
        let requests = harness.frame_requests();
        assert!(
            !harness.advance(COUNTDOWN_TICK * 4),
            "a released pane's timer never fires"
        );
        assert_eq!(harness.frame_requests(), requests);
    }

    /// A deadline that goes away and comes back before the cancelled tick was due arms
    /// exactly one chain: the pending tick is cancelled before the next one is armed.
    #[test]
    fn a_deadline_that_comes_back_should_not_arm_a_second_timer_chain() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.countdown.timer.borrow().is_some());

        // A read in flight takes the schedule away ...
        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Refreshing {
                next_refresh_at: 600_000,
            },
        );
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget.countdown.timer.borrow().is_none(),
            "the read in flight cancelled the tick it replaced"
        );
        assert!(read(&harness, &pane.widget.countdown.label).is_none());

        // ... and the answer puts the same deadline back before the cancelled tick would
        // have been due.
        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: 600_000,
            },
        );
        sync(&mut harness, &mut pane);

        let reads = clock.reads();
        let requests = harness.frame_requests();
        harness.advance(COUNTDOWN_TICK);
        assert_eq!(
            clock.reads() - reads,
            1,
            "the deadline that came back arms one chain, not two"
        );
        assert_eq!(
            harness.frame_requests(),
            requests,
            "the tick that changed nothing asks for no frame"
        );

        clock.set(1_000);
        let reads = clock.reads();
        harness.advance(COUNTDOWN_TICK);
        assert_eq!(countdown_text(&harness), "09:59");
        assert_eq!(
            clock.reads() - reads,
            1,
            "and the surviving chain ticks once per round"
        );
    }

    /// The countdown label the header draws.
    fn countdown_text(harness: &WidgetTestApp) -> String {
        text_node(harness, "TEST_PENTAGON_PIZZA_INDEX_COUNTDOWN")
    }

    // ── multi-pane keys ──────────────────────────────────────────────────────

    /// The key of the refresh control a pane just built into `parent`.
    ///
    /// The control's key is its debug id prefix, so the test reads it back rather than
    /// reconstructing the format: what it asserts is the pane's own identity, not how
    /// `unique_key` spells it.
    fn built_control_key(harness: &WidgetTestApp, parent: NodeId, before: &[NodeId]) -> SmolStr {
        let wrapper = harness
            .tree()
            .get(parent)
            .expect("the header row exists")
            .children()
            .iter()
            .find(|child| !before.contains(child))
            .copied()
            .expect("the header built a control");
        let debug = harness
            .find_by_debug_prefix(wrapper, "pentagon_pizza_index.refresh")
            .and_then(|node| harness.tree().debug_id(node))
            .expect("the refresh control is built");
        // The kit names the node its key identifies with an `:interactive` suffix; what
        // is left is the key the action was derived from.
        SmolStr::new(
            debug
                .strip_suffix(":interactive")
                .expect("the kit names its interactive node after the key"),
        )
    }

    fn children_of(harness: &WidgetTestApp, parent: NodeId) -> Vec<NodeId> {
        harness
            .tree()
            .get(parent)
            .expect("the header row exists")
            .children()
            .to_vec()
    }

    /// Two panes in one application build their own controls: the key is the pane's, so
    /// building the second does not take the first's handler and releasing the first
    /// does not take the second's (AGENTS §1.11 and §11's multi-pane rule).
    #[test]
    fn two_panes_should_keep_their_own_refresh_controls() {
        let mut harness = harness();
        let first = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        let second = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        harness.drain();
        let parent = harness.placeholder_parent();

        let before = children_of(&harness, parent);
        first
            .header(harness.app_mut(), HeaderGroup::Right, parent)
            .expect("the first pane's header builds");
        let first_key = built_control_key(&harness, parent, &before);
        let before = children_of(&harness, parent);
        second
            .header(harness.app_mut(), HeaderGroup::Right, parent)
            .expect("the second pane's header builds");
        let second_key = built_control_key(&harness, parent, &before);

        let first_action = Action::Custom(format_smolstr!("{first_key}.click"));
        let second_action = Action::Custom(format_smolstr!("{second_key}.click"));
        assert!(
            harness.app().has_action_handler(&first_action),
            "the first pane's control has a click handler"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "building the second pane did not take the first pane's handler"
        );

        first.release(harness.app_mut());

        assert!(
            !harness.app().has_action_handler(&first_action),
            "releasing the first pane took its own handler"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "and left the second pane's handler alone"
        );
        assert_ne!(
            first_key, second_key,
            "two panes build their controls under different keys"
        );
        assert!(
            harness.tree().get(parent).is_ok(),
            "the header row both panes built into is not the released pane's to remove"
        );
    }

    // ── RPC reply attribution ────────────────────────────────────────────────

    /// An answer that was already in the inbox when the account changed is applied to
    /// the command it answers, not to the load that replaced it: the replacement still
    /// waits for its own answer, and the old command's failure is not reported as the
    /// new account's.
    #[test]
    fn a_replaced_commands_answer_should_not_clear_the_command_that_replaced_it() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        frame(&mut harness, &mut pane);

        // The click starts a refresh whose answer is already waiting when the frame
        // that changes the account runs.
        harness
            .with_cx(|cx| pane.widget.refresh(cx))
            .expect("the refresh starts");
        assert!(pane.widget.refresh_pending);
        let mut delivered = false;
        for _ in 0..64 {
            harness.drain_runtime();
            if !pane
                .widget
                .binding
                .as_ref()
                .expect("the pane has a bridge")
                .inbox()
                .is_empty()
            {
                delivered = true;
                break;
            }
        }
        assert!(delivered, "the refresh was answered before the frame");

        sign_in(&mut harness, "second-account");
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.pending.is_empty(),
            "the replacement's own answer is still waited for"
        );
        assert!(!pane.widget.refresh_pending);
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::Connecting,
            "the old command's answer did not become the new account's failure"
        );

        // ... and the load's own acknowledgement reaches the pane when it arrives.
        for _ in 0..64 {
            harness.drain_runtime();
            sync(&mut harness, &mut pane);
            if pane.widget.pending.is_empty() {
                break;
            }
        }
        assert!(
            pane.widget.pending.is_empty(),
            "the load's own acknowledgement was applied"
        );
    }

    /// A reply carrying a request identity the pane has already answered is not the
    /// answer to the command now in flight.
    #[test]
    fn a_reply_the_pane_already_answered_should_not_clear_the_command_in_flight() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);
        frame(&mut harness, &mut pane);

        // The first command the pane sends is a load, whose acknowledgement it consumes.
        harness
            .with_cx(|cx| pane.widget.start(false, cx))
            .expect("the load starts");
        assert!(!pane.widget.pending.is_empty());
        harness.with_cx(|cx| {
            pane.widget
                .apply_rpc(RequestId::new(1), Ok(PizzaResponse::StreamStarted), cx);
        });
        assert!(
            pane.widget.pending.is_empty(),
            "the load's acknowledgement was applied"
        );

        // A command is in flight ...
        harness
            .with_cx(|cx| pane.widget.start(false, cx))
            .expect("the load starts");
        assert!(!pane.widget.pending.is_empty());

        // ... and an answer the pane has already applied cannot be its answer.
        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert!(
            !pane.widget.pending.is_empty(),
            "the stale reply did not answer the command in flight"
        );
        assert_eq!(pane.widget.pending.dropped(), 1);
        assert_eq!(reported(&pane, &harness), WidgetState::Connecting);
    }

    // ── service seam ────────────────────────────────────────────────────────

    /// The binding is the bridge: a round the fake service publishes reaches the
    /// pane's reading through the inbox its `sync` drains.
    #[test]
    fn a_round_the_stream_publishes_should_reach_the_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane_with_source(&mut harness, FakeSource::new([FetchPlan::Snapshot(4)]));
        load(&mut harness, &mut pane);

        for _ in 0..64 {
            harness.drain_runtime();
            sync(&mut harness, &mut pane);
            harness.drain();
            if pane.widget.reading.is_some() && pane.widget.ready.is_some() {
                break;
            }
        }

        assert_eq!(level_of(&pane), Some(PizzaLevel::DoubleTake));
        assert_eq!(pane.widget.accepted, Some(1));
        assert_eq!(reported(&pane, &harness), WidgetState::Ready);
    }

    /// A host without the Kytrix service connection is a business failure, not a
    /// wait: the pane reports the typed closed-connection failure with the recovery the
    /// framework handed it.
    #[test]
    fn a_host_without_a_service_connection_should_report_a_failure() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane_without_source(&mut harness);

        load(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let state = reported(&pane, &harness);
        let failure = state.failure().expect("the pane reports a failure");
        assert!(
            !failure.text(&harness.services().translate).is_empty(),
            "the failure renders"
        );
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "the framework's recovery is the one the failure offers"
        );
        assert!(
            !pane.widget.has_content(),
            "a failed pane has no content: the surface draws the failure placeholder"
        );
    }

    // ── surface integration (mounted through the real registration path) ─────

    /// A signed-out pane is presented by the framework as a rich-text notice: the
    /// catalogue writes it as a `<p align="center">` paragraph, and the surface draws
    /// it as text rather than as markup.
    #[test]
    fn a_signed_out_pane_should_be_presented_in_the_language_showing_now() {
        let mut harness = harness();
        let widget = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        harness.drain();
        assert!(
            widget.config().is_none(),
            "the index is the same index for every reader, so the pane persists nothing"
        );

        let placeholder = node_with_prefix(&harness, widget.surface(), "WIDGET_PLACEHOLDER");
        assert!(harness.visible(placeholder));
        let english = harness.text_of(placeholder);
        assert!(
            english.contains("register or log in"),
            "the notice is drawn as text: {english}"
        );
        assert!(
            !english.contains("<p") && !english.contains("</p"),
            "the notice's markup is not drawn as text: {english}"
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.drain();
        harness.drain();
        // The surface rebuilds a notice's subtree to draw it in the new language, so
        // the placeholder is found again rather than reusing the released handle.
        let placeholder = node_with_prefix(&harness, widget.surface(), "WIDGET_PLACEHOLDER");
        let chinese = harness.text_of(placeholder);
        assert!(
            chinese.contains("注册") || chinese.contains("登陆"),
            "the notice follows the language: {chinese}"
        );
    }

    /// A signed-in pane on a host with no service connection is presented as a failure
    /// placeholder with the framework's recovery control.
    #[test]
    fn a_failed_pane_should_be_presented_with_the_frameworks_recovery() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let widget = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        harness.drain();

        assert!(widget.status().is_failed(harness.app()));
        let placeholder = node_with_prefix(&harness, widget.surface(), "WIDGET_PLACEHOLDER");
        let text = harness.text_of(placeholder);
        assert!(!text.is_empty(), "the failure is presented: {text}");
        assert!(
            harness
                .find_by_debug_prefix(placeholder, "widget.placeholder.action.")
                .is_some(),
            "the failure placeholder carries the framework's recovery control"
        );
        assert!(
            !harness.has_canvas(placeholder),
            "a failure is not a spinner"
        );
    }

    // ── release ─────────────────────────────────────────────────────────────

    /// Releasing the pane drops the bridge and releases the header component's scope
    /// and action handler; the body's nodes belong to the framework's surface teardown.
    #[test]
    fn releasing_the_pane_should_release_what_it_built() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let action = pane
            .widget
            .refresh_button
            .as_ref()
            .and_then(Button::action_identity)
            .expect("the control's click is an engine action");
        let refresh_node = pane
            .widget
            .refresh_button
            .as_ref()
            .and_then(Button::node)
            .expect("the control is built");

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(pane.widget.binding.is_none(), "the bridge is dropped");
        assert!(
            pane.widget.refresh_button.is_none(),
            "the control is released"
        );
        assert!(pane.widget.body_root.is_none());
        assert!(
            !harness.app().has_action_handler(&action),
            "the control's handler was removed with its scope"
        );
        assert!(
            harness.tree().get(refresh_node).is_ok(),
            "the header row's nodes belong to the pane, not to the widget's release"
        );
    }

    // ── copy and errors resolve ─────────────────────────────────────────────

    /// The message keys this pane draws are all named in both locales, which is what
    /// keeps a missing translation from being a blank line rather than a failure.
    #[test]
    fn every_message_this_pane_shows_should_resolve() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = product_translate(language);

            for level in [
                PizzaLevel::FadeOut,
                PizzaLevel::DoubleTake,
                PizzaLevel::RoundHouse,
                PizzaLevel::FastPace,
                PizzaLevel::ExerciseTerm,
            ] {
                assert!(
                    !level.name(&translate).is_empty(),
                    "{} has no copy in {language}",
                    level.name_key()
                );
            }
            for key in [
                "widget.names.pentagon_pizza_index",
                "widget.descriptions.pentagon_pizza_index",
                "widget.pentagon_pizza_index.refresh_now",
                "widget.pentagon_pizza_index.observed_at",
                "widget.pentagon_pizza_index.level_number",
                "widget.pentagon_pizza_index.logged_out",
            ] {
                assert!(
                    !t_smol!(translate, key).is_empty(),
                    "{key} has no copy in {language}"
                );
            }
            assert!(
                !t_smol!(
                    translate,
                    "widget.pentagon_pizza_index.observed_at",
                    time = "2026-10-24 06:00"
                )
                .is_empty(),
                "widget.pentagon_pizza_index.observed_at has no copy in {language}"
            );
            assert!(
                !t_smol!(
                    translate,
                    "widget.pentagon_pizza_index.level_number",
                    level = 3
                )
                .is_empty(),
                "widget.pentagon_pizza_index.level_number has no copy in {language}"
            );
        }
    }

    /// Every failure this pane can report resolves to a sentence in both locales: a
    /// failure is the one thing a reader has to act on, so a blank one is the worst
    /// place for a missing key.
    #[test]
    fn every_error_this_pane_reports_should_resolve() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = product_translate(language);

            // One of each failure rather than one per value: every variant is a
            // key of its own, and a level that renders is the same key whatever
            // number it names.
            for error in [
                PizzaDataError::MissingObservation,
                PizzaDataError::UnknownLevel { level: 9 },
            ] {
                assert!(
                    !error.translated_message(&translate).is_empty(),
                    "{error:?} renders empty in {language}"
                );
            }
            for error in [
                PizzaServiceError::StreamClosed,
                PizzaServiceError::StreamBusy,
            ] {
                assert!(
                    !error.translated_message(&translate).is_empty(),
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
                    !PizzaFailure::Fetch(PizzaFetchError::Service(error.clone()))
                        .translated_message(&translate)
                        .is_empty(),
                    "{error:?} renders empty in {language}"
                );
            }
            assert!(
                !PizzaFailure::Fetch(PizzaFetchError::Data(PizzaDataError::MissingObservation))
                    .translated_message(&translate)
                    .is_empty(),
                "a data failure renders empty in {language}"
            );
            assert!(
                !PizzaFailure::Rpc(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                })
                .translated_message(&translate)
                .is_empty(),
                "an RPC failure renders empty in {language}"
            );
        }
    }
}
