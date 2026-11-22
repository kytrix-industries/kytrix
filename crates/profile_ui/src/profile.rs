//! The account who is signed in, and how that changes, behind the account control the title bar
//! draws.
//!
//! The control is this type: the title bar mounts [`ProfileMenu`], and it draws the account's face
//! in its square and the menu behind it — or, while nobody is signed in, the button whose press
//! starts the sign-in.
//!
//! # The five stages
//!
//! This is an application-layer control, so it implements the engine's stages itself
//! (`crates/widgets/AGENTS.md` §4 / §9), with the caller driving them:
//!
//! | stage | who calls | what it does |
//! |---|---|---|
//! | [`ProfileMenu::new`] | the title bar | stores the host; **starts no work, reports no failure** |
//! | [`ProfileMenu::build`] | the title bar, once the slot exists | mints the signals, builds the trigger and its hint, starts watching the account session |
//! | [`ProfileMenu::sync`] | the shell's frame route, every frame | applies the settings the signal carried, drains the async results, syncs the overlay / hint / alert |
//! | [`ProfileMenu::release`] | the title bar, on teardown | releases every kit component and the control's scope; idempotent |
//!
//! The kit components that need the composition root's registries ([`Overlay`], [`Tooltip`],
//! [`Dialog`]) are built from the host's handles — the same layer stack, frame router and
//! animation router the widgets use; the control never builds a second one.
//!
//! # What it owns
//!
//! One account, and every flow that can change it: starting a sign-in, waiting for the browser,
//! storing what comes back, asking the account service what tier that account is on, opening the
//! account dashboard, signing out, and noticing when the credential behind the account stops
//! being valid. The avatar is generated (and rasterized) here too, because it is a property of the
//! account and has to be dropped with it.
//!
//! # What it does not own
//!
//! The account itself. That lives in the application settings the composition root injects
//! ([`crate::host::AccountSettings`]): this control reads and writes it, but the store is the
//! root's (P4-3). Every screen that cares who is signed in follows the same record, so signing in
//! and signing out cannot leave the title bar and the panes disagreeing.
//!
//! # What it does not do
//!
//! It never retries. A failed sign-in reports itself and stops; the user asks again. A tier that
//! could not be read leaves the menu saying it is unknown rather than asking a second time,
//! because the tiers of an account do not change while it is signed in.
//!
//! # Following the settings
//!
//! The root keeps [`WidgetServices::config`](widget::services::WidgetServices::config) in step
//! with its settings manager, so `sync` reads that signal, compares the two fields this feature
//! cares about against the snapshot it last applied, and re-reads the authoritative snapshot
//! through [`AccountSettings::read`](crate::host::AccountSettings::read) when they moved. A
//! settings write that changed neither costs nothing — which is what keeps the control at zero
//! frames while it is idle (I3).
//!
//! # Async
//!
//! Every flow that waits is a future on the **injected** tokio runtime
//! ([`WidgetServices::runtime`], the handle the composition root hands the widgets), because the
//! frame thread must never wait for a browser, a request or a blocking pool, and because a
//! control this crate mounts has no runtime of its own. The blocking half (generating and
//! rasterizing a face) is `spawn_blocking` on the same handle. Results travel back through one
//! unbounded channel and wake the frame loop with `AppHandle::send` + `App::request_repaint`;
//! `sync` drains the channel. An attempt that is still waiting when the control is released is
//! cancelled by dropping the channel that tells its task to stop (a `oneshot` sender), which
//! drops the attempt's loopback listener with it.
//!
//! [`WidgetServices::runtime`]: widget::services::WidgetServices::runtime

use std::{rc::Rc, sync::Arc};

use app_config::settings::AccountConfig;
use base_ui::kit::{
    button::Button,
    dialog::{Dialog, DialogError},
    layer::{Band, LayerSlot},
    overlay::{
        Anchor, DismissPolicy, Overlay, OverlayA11y, OverlayError, OverlayOptions, OverlaySurface,
        Placement,
    },
    size::UnifiedSizable,
    tooltip::{Tooltip, TooltipContent, TooltipOptions},
};
use i18n::{t_smol, translate::Translate};
use kytrix_service_provider::auth::{constants::ACCOUNT_ORIGIN, model::PlanCode};
use smol_str::{SmolStr, format_smolstr};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    oneshot,
};
use ui_core::{
    app::App,
    error::EngineError,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    resources::PendingResource,
    window::WindowId,
};
use widget::error::TranslatedError;

use crate::{
    account::SignedInAccount,
    avatar::{AvatarError, Face},
    dialogs::{self, AlertAction, AlertContent},
    host::ProfileHost,
    menu::{MenuContent, MenuRequest, MenuShape, MenuSignals, plan_label},
    oauth::{self, AccountFailure, LoginError},
    session::SessionWatch,
};

/// How large the face inside the trigger is while no generated avatar is ready.
///
/// The face is drawn at the size the title bar's other marks take, which the placeholder keeps
/// even for a signed-in account.
const TRIGGER_AVATAR_SIZE: f32 = 16.0;

/// How large the face inside the trigger is once the account's generated avatar is ready: larger
/// than the bar's marks, so the account's picture reads as the control rather than as a mark in
/// it.
const TRIGGER_ACCOUNT_AVATAR_SIZE: f32 = 24.0;

/// The debug id of the face inside the trigger.
const TRIGGER_AVATAR_ID: &str = "TEST_PROFILE_TRIGGER_AVATAR";

/// The rasterized face of one avatar SVG, as an engine image node draws it.
///
/// The engine decodes raster formats only, so the face reaches it as the PNG the blocking job
/// produced ([`crate::avatar`]). `generation` bumps once per accepted face: the menu content and
/// the trigger compare it against what they were built with, because an engine image node cannot
/// be re-pointed at another resource.
pub(crate) struct FaceTexture {
    /// The rasterized PNG, still being decoded by the engine (the node draws it as soon as it is).
    pub pending: PendingResource,
    /// The PNG bytes' own SVG, so a face that is still the current one is not rasterized twice.
    pub svg: SmolStr,
    /// How many faces this control has accepted (a rebuild detector, not an identity).
    pub generation: u64,
}

/// A result one of the control's background tasks produced.
///
/// Everything a task has to say arrives on one channel, and every arm is applied in `sync`;
/// nothing here touches the application from the task's own thread except through
/// `AppHandle::send` (which is the engine's one cross-thread入口).
pub(crate) enum MenuEvent {
    /// A wallet's face was generated and/or rasterized.
    Face {
        /// The wallet the face is for (the result is dropped when it is no longer the account).
        wallet: SmolStr,
        /// Whether the face had to be generated (`false` = a stored SVG was rasterized).
        generated: bool,
        /// The face, or why it could not be produced.
        result: Result<(SmolStr, Face), AvatarError>,
    },
    /// A tier query answered.
    Plan {
        /// The query generation the answer belongs to.
        generation: u64,
        /// The account the query was about.
        user_id: SmolStr,
        /// The answer.
        result: Result<Option<PlanCode>, kytrix_service_provider::auth::model::AuthError>,
    },
    /// The attempt that was waiting for its browser callback ended.
    SignIn {
        /// `None` = the attempt was cancelled or expired (nothing to report).
        outcome: Option<Result<AccountConfig, LoginError>>,
    },
    /// A dashboard handoff answered.
    Handoff {
        /// The account the handoff was requested for.
        user_id: SmolStr,
        /// The one-time code, or why there is none.
        result: Result<String, kytrix_service_provider::auth::model::AuthError>,
    },
    /// The account session says the stored credential is no longer good.
    Invalidated,
}

/// The trigger's face: which size the face takes and whether it is a picture.
///
/// It is part of [`TriggerShape`] because the trigger's box and its a11y name are build-time
/// values: a face that arrives after the trigger was built has to rebuild it (a rebuild under the
/// same key, `crates/widgets/AGENTS.md` §15.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TriggerShape {
    /// Whether somebody is signed in (the control is a menu trigger rather than a sign-in button).
    signed_in: bool,
    /// The language the trigger's hint and a11y name were resolved in.
    language: i18n::language::codes::LanguageCode,
    /// The generation of the face the trigger was built with.
    face: Option<u64>,
}

/// The control's trigger: the kit button, its node and the values it was built from.
struct Trigger {
    button: Button,
    node: NodeId,
    shape: TriggerShape,
}

/// One account control's built state (everything that needs an [`App`] to mint).
struct State {
    window: WindowId,
    parent: NodeId,
    key: SmolStr,
    scope: SignalScope,
    /// Whether the menu is open (the overlay writes it; the trigger paints from it).
    open: Signal<bool>,
    /// Whether the menu should be open: what the trigger's presses toggle, and what `sync` makes
    /// the overlay agree with.
    ///
    /// It is a wish rather than a fact because opening needs the trigger to have been laid out
    /// (the overlay anchors on its box): a press that arrives before the first layout — or one
    /// the caller's slot is not ready for — is honoured on the next frame instead of being lost.
    wants_open: bool,
    signals: MenuSignals,
    trigger: Trigger,
    tooltip: Tooltip,
    overlay: Overlay,
    /// The account record as last applied, and the avatar it carried.
    snapshot: crate::host::AccountSnapshot,
    /// The face on screen, once one was rasterized for the account.
    face: Option<FaceTexture>,
    /// The face job in flight, as `"<wallet>|<svg-or-none>"`; also the record of a job that
    /// failed, so a face that cannot be produced is not attempted on every frame.
    face_job: Option<SmolStr>,
    /// How many faces have been accepted (see [`FaceTexture::generation`]).
    face_generation: u64,
    /// The tier of the signed-in account, once a query answered.
    plan: Option<PlanCode>,
    /// Bumped whenever the tier a query was for stops being the tier in question.
    plan_generation: u64,
    /// The query in flight: `(generation, user_id)`.
    plan_query: Option<(u64, SmolStr)>,
    /// The dashboard handoff in flight, as the account it was requested for.
    handoff: Option<SmolStr>,
    /// The menu's content, while the menu is open.
    menu: Option<MenuContent>,
    /// The alert a flow is being reported in, if one is up.
    alert: Option<Dialog>,
    /// The background results this control is waiting for.
    events: UnboundedReceiver<MenuEvent>,
    events_tx: UnboundedSender<MenuEvent>,
    /// Ends the attempt that is waiting for its browser callback (dropping it cancels the task).
    sign_in: Option<oneshot::Sender<()>>,
    /// Whether the alert that is up is the one that reports a **wait** for the browser.
    ///
    /// It is what tells an attempt's own ending apart from a report: the wait's alert is taken
    /// down when the wait ends, while a report that replaced it belongs to a wait that is already
    /// over.
    awaiting_alert: bool,
    /// Ends the account-session watch (dropping it cancels the task).
    session: Option<oneshot::Sender<()>>,
}

/// The account control in the title bar.
///
/// See the module documentation for the five stages and for what the control owns.
pub struct ProfileMenu {
    host: ProfileHost,
    state: Option<State>,
}

impl ProfileMenu {
    /// Stores the host. **Starts no work and reports no failure** — the engine's `new` boundary,
    /// borrowed by this control (`crates/widgets/AGENTS.md` §4.1).
    #[must_use]
    pub fn new(host: ProfileHost) -> Self {
        Self { host, state: None }
    }

    /// Builds the control into `parent`: the trigger, the hint under it, and the signals every
    /// later frame reads.
    ///
    /// The menu itself is not built here: a menu exists only while it is open, and nobody has
    /// opened one yet. The account session starts being watched here, because this is the stage
    /// the engine calls once the control has somewhere to report to.
    ///
    /// Calling it twice is a no-op (the first build owns the nodes).
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / icon resource errors from the trigger's components.
    pub fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        if self.state.is_some() {
            return Ok(());
        }
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window });
        }
        let scope = app.signal_scope();
        let key = format_smolstr!("profile@{}", scope.index());
        let signals = MenuSignals {
            request: app.signal_scoped(scope, None),
            copied: app.signal_scoped(scope, None),
            plan: app.signal_scoped(scope, plan_label(None, self.host.translate())),
        };
        let open = app.signal_scoped(scope, false);
        let snapshot = self.host.settings().read(app);
        let trigger = TriggerBuild {
            host: &self.host,
            window,
            parent,
            key: &key,
            snapshot: &snapshot,
            face: None,
            requests: signals.request,
        }
        .build(app)?;
        let tooltip = Tooltip::new(
            app,
            self.host.theme().clone(),
            self.host.layer().clone(),
            self.host.animation(),
            format_smolstr!("{key}:hint"),
            TooltipOptions {
                // The hint sits under the face the press is on.
                placement: Placement::BOTTOM,
                ..TooltipOptions::default()
            },
        );
        tooltip.attach(app, window, trigger.node);
        let overlay = Overlay::new(
            app,
            self.host.theme().clone(),
            self.host.layer().clone(),
            self.host.animation(),
            format_smolstr!("{key}:menu"),
            OverlayOptions {
                // The control sits on the title bar's right-hand side, so the menu hangs from its
                // right edge and grows leftwards into the window rather than off the edge.
                placement: Placement::BOTTOM_END,
                band: Band::Popup,
                slot: LayerSlot::Exclusive,
                // A menu closes when the pointer lands outside it, and consumes that press (the
                // press that dismissed it does not also open the thing behind it).
                dismiss: DismissPolicy::OutsideClick,
                modal: false,
                surface: OverlaySurface::menu(),
                a11y: OverlayA11y::menu(trigger_a11y_label(&snapshot, self.host.translate())),
                ..OverlayOptions::default()
            },
        );
        let (events_tx, events) = unbounded_channel();
        let mut state = State {
            window,
            parent,
            key,
            scope,
            open,
            wants_open: false,
            signals,
            trigger,
            tooltip,
            overlay,
            snapshot,
            face: None,
            face_job: None,
            face_generation: 0,
            plan: None,
            plan_generation: 0,
            plan_query: None,
            handoff: None,
            menu: None,
            alert: None,
            events,
            events_tx,
            sign_in: None,
            awaiting_alert: false,
            session: None,
        };
        self.start_session_watch(app, &mut state);
        // An account the settings already hold is a signed-in account: its tier is asked for
        // here, because waiting for a settings *change* would never ask for the first one.
        if let Some(user_id) = state
            .snapshot
            .account
            .as_ref()
            .map(|account| SmolStr::new(account.user_id.as_str()))
        {
            self.start_plan_query(app, &mut state, user_id);
        }
        self.state = Some(state);
        // The first frame starts the face job the snapshot asks for and applies everything above.
        self.sync(app)
    }

    /// One frame: applies what the settings now say, drains the background results, and syncs the
    /// trigger, the menu, its hint and any alert.
    ///
    /// This is the control's only state transition point, and it writes nothing when nothing
    /// changed (the same zero-frame steady state a widget's `sync` keeps, I3).
    ///
    /// # Errors
    /// Engine failures from the nodes and kit components this control owns.
    pub fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(mut state) = self.state.take() else {
            return Ok(());
        };
        let result = self.sync_state(app, &mut state);
        self.state = Some(state);
        result
    }

    /// Releases every kit component this control built, its overlay, and its signal scope.
    ///
    /// The caller removes the control's node first (E16: nodes before scopes); this method does
    /// **not** delete the trigger node — it only gives back what the components own. It is
    /// idempotent: a second call has nothing left to release.
    pub fn release(&mut self, app: &mut App) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        // Dropping the senders ends the tasks: the attempt that is waiting for its browser
        // callback is cancelled (and releases its loopback listener), and the session watch stops
        // waiting for a change nobody is drawing any more.
        state.sign_in = None;
        state.session = None;
        if let Some(alert) = state.alert.take() {
            alert.release(app);
        }
        if let Some(content) = state.menu.take() {
            remove_if_present(app, state.window, content.root);
            content.release(app);
        }
        state.overlay.release(app);
        state.tooltip.release(app);
        state.trigger.button.release(app);
        let _ = app.release_scope(state.scope);
    }

    /// Whether the account menu is open.
    ///
    /// The overlay is the only writer: an outside click or `Escape` closes it in the engine, and
    /// the next `sync` reports that here.
    #[must_use]
    pub fn is_menu_open(&self, app: &App) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.wants_open && state.overlay.is_open(app))
    }

    /// The complete address the copy control last asked for, if it asked.
    #[must_use]
    pub fn copied_address(&self, app: &App) -> Option<SmolStr> {
        let state = self.state.as_ref()?;
        app.read(&state.signals.copied).clone()
    }

    /// The node the trigger was built into (the caller removes it before [`Self::release`]).
    #[must_use]
    pub fn trigger_node(&self) -> Option<NodeId> {
        self.state.as_ref().map(|state| state.trigger.node)
    }

    /// The account the control is showing, as the settings last said.
    #[must_use]
    pub fn account(&self) -> Option<&AccountConfig> {
        self.state
            .as_ref()
            .and_then(|state| state.snapshot.account.as_ref())
    }

    /// The tier the menu believes the account is on.
    #[must_use]
    pub fn plan(&self) -> Option<PlanCode> {
        self.state.as_ref().and_then(|state| state.plan)
    }

    /// Whether an attempt is waiting for its browser callback.
    #[must_use]
    pub fn is_waiting_for_browser(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.sign_in.is_some())
    }

    /// One frame of the control, with its built state taken out of `self`.
    fn sync_state(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        if app.window(state.window).is_none() {
            return Ok(());
        }
        self.poll_events(app, state)?;
        self.refresh_settings(app, state)?;
        self.apply_requests(app, state)?;
        self.sync_face_job(app, state);
        // The menu is synced before the trigger: an engine-side close (an outside click,
        // `Escape`) has to become the control's own state before a trigger rebuild could
        // re-anchor the overlay the engine just dismissed.
        self.sync_menu(app, state)?;
        self.sync_trigger(app, state)?;
        self.sync_tooltip(app, state);
        self.sync_alert(app, state)?;
        self.sync_plan_label(app, state);

        Ok(())
    }

    /// Applies the result every background task may have produced since the last frame.
    fn poll_events(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        while let Ok(event) = state.events.try_recv() {
            match event {
                MenuEvent::Face {
                    wallet,
                    generated,
                    result,
                } => self.apply_face(app, state, wallet, generated, result),
                MenuEvent::Plan {
                    generation,
                    user_id,
                    result,
                } => self.apply_plan(state, generation, &user_id, result),
                MenuEvent::SignIn { outcome } => self.finish_sign_in(app, state, outcome)?,
                MenuEvent::Handoff { user_id, result } => {
                    self.finish_handoff(app, state, &user_id, result)?
                }
                MenuEvent::Invalidated => self.forget_invalidated_account(app, state),
            }
        }

        Ok(())
    }

    /// Re-reads the account when the settings signal moved.
    ///
    /// The signal is the change detector, the injected store is the authority: the root keeps
    /// both in step, so a write that changed neither the account nor its avatar costs no re-read
    /// (see the module documentation on following the settings).
    fn refresh_settings(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        let carried = self.host.snapshot_signal(app);
        if carried == state.snapshot {
            return Ok(());
        }
        let snapshot = self.host.settings().read(app);
        if snapshot == state.snapshot {
            // The signal moved but the record this control draws did not: remember the value so
            // the next frame does not read the store again.
            state.snapshot = snapshot;
            return Ok(());
        }
        let previous_user = state
            .snapshot
            .account
            .as_ref()
            .map(|account| account.user_id.clone());
        let next_user = snapshot
            .account
            .as_ref()
            .map(|account| account.user_id.clone());
        let previous_wallet = state
            .snapshot
            .account
            .as_ref()
            .map(|account| account.wallet_address.clone());
        let next_wallet = snapshot
            .account
            .as_ref()
            .map(|account| account.wallet_address.clone());
        state.snapshot = snapshot;

        // The menu goes with the account that opened it: a signed-out control is a plain button,
        // and the state that said a menu was open is not the next account's to inherit.
        if previous_user != next_user {
            self.close_menu(app, state);
            self.clear_plan(state);
            if let Some(user_id) = next_user {
                self.start_plan_query(app, state, SmolStr::new(user_id));
            }
        }
        if previous_wallet != next_wallet {
            // The face belongs to the wallet: a different account's face is not this one's.
            state.face = None;
            state.face_job = None;
        }

        Ok(())
    }

    /// Takes the request a control wrote and runs the flow it stands for.
    fn apply_requests(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        let Some(request) = *app.read(&state.signals.request) else {
            return Ok(());
        };
        app.write(&state.signals.request, None);
        match request {
            MenuRequest::Trigger => {
                if state.snapshot.account.is_some() {
                    // The press toggles what the next frame makes the overlay agree with; a
                    // closing menu is closed here so the fade starts on the frame of the press.
                    state.wants_open = !state.wants_open;
                    if !state.wants_open {
                        state.overlay.close(app);
                    }
                } else {
                    self.start_sign_in(app, state)?;
                }
            }
            MenuRequest::OpenAccount => {
                // Choosing an entry takes the menu down before the entry's own flow opens
                // anything: a menu left up would sit over the question it just asked.
                self.close_menu(app, state);
                self.open_account(app, state);
            }
            MenuRequest::SignOut => {
                self.close_menu(app, state);
                self.confirm_sign_out(app, state)?;
            }
            MenuRequest::CopyAddress => self.copy_address(app, state),
            MenuRequest::CancelSignIn => self.cancel_sign_in(app, state),
            MenuRequest::ConfirmSignOut => self.sign_out(app, state)?,
        }

        Ok(())
    }

    /// Puts the signed-in account's complete address on the clipboard.
    ///
    /// The write is a **request** rather than a signal comparison, so asking twice puts the
    /// address on the clipboard twice: a user who copied something else in between and presses
    /// copy again has to get the address back, which an "already copied" value would swallow. The
    /// value the request writes is what a shell (and a test) reads as what was last offered.
    fn copy_address(&self, app: &mut App, state: &mut State) {
        let Some(address) = state
            .snapshot
            .account
            .as_ref()
            .map(|account| SmolStr::new(account.wallet_address.as_str()))
        else {
            return;
        };
        app.write(&state.signals.copied, Some(address.clone()));
        if let Err(error) = app.set_clipboard_text(address.as_str()) {
            // A clipboard the platform refused (no native window, another process holding it) is
            // not a failure the user can act on; the address is still offered whole by the hint
            // beside the line.
            log::warn!("failed to copy the account address: {error}");
        }
    }

    /// Starts the rasterization of the face the snapshot asks for, when there is one to start.
    ///
    /// A stored avatar SVG is rasterized; a wallet without one is generated first. The job key
    /// includes both the wallet and whether the SVG was stored, so a result for the wrong account
    /// (or for a face that has already been superseded) is recognised when it lands.
    fn sync_face_job(&self, app: &mut App, state: &mut State) {
        let Some(account) = state.snapshot.account.as_ref() else {
            // Nobody is signed in: no face is being drawn, and a result that arrives is dropped by
            // `apply_face` because the wallet is no longer the account.
            return;
        };
        let stored = crate::account::avatar_of(&state.snapshot).cloned();
        let drawn = state.face.as_ref().map(|face| face.svg.clone());
        if stored.is_some() && drawn == stored {
            // The face on screen is the one the settings name.
            return;
        }
        let wallet = SmolStr::new(account.wallet_address.as_str());
        let job = face_job_key(&wallet, stored.as_deref());
        if state.face_job.as_ref() == Some(&job) {
            return;
        }
        state.face_job = Some(job);
        let runtime = self.host.runtime().clone();
        let scale = app
            .window(state.window)
            .map_or(1.0, |window| window.scale().factor());
        let side = (crate::menu::ACCOUNT_AVATAR_SIZE * scale).round().max(1.0) as u32;
        let (events, handle, window) = (state.events_tx.clone(), app.handle(), state.window);
        let generated = stored.is_none();
        let job_wallet = wallet;
        self.host.runtime().spawn(async move {
            let result =
                crate::avatar::render_face(job_wallet.clone(), stored, side, runtime).await;
            let _ = events.send(MenuEvent::Face {
                wallet: job_wallet,
                generated,
                result,
            });
            // The wake is what runs the `sync` that applies this: the frame loop is otherwise
            // asleep, exactly as it is for a widget whose stream delivered a message.
            handle.send(move |app| {
                let _ = app.request_repaint(window);
            });
        });
    }

    /// Accepts a face, unless the account it was for is no longer the signed-in one.
    ///
    /// A failed job keeps its key recorded, so a face that cannot be produced (a malformed stored
    /// SVG) is not attempted on every frame: the user gets the default glyph, and the reason goes
    /// to the log rather than into an alert nobody can act on.
    fn apply_face(
        &self,
        app: &mut App,
        state: &mut State,
        wallet: SmolStr,
        generated: bool,
        result: Result<(SmolStr, Face), AvatarError>,
    ) {
        let current = state
            .snapshot
            .account
            .as_ref()
            .is_some_and(|account| account.wallet_address == wallet.as_str());
        if !current {
            log::debug!("a generated avatar no longer belonged to the signed-in account");

            return;
        }
        let (wallet, face) = match result {
            Ok(face) => face,
            Err(error) => {
                log::warn!("failed to produce the wallet avatar: {error}");

                return;
            }
        };
        state.face_job = None;
        if generated {
            // The settings publish the new avatar; this control reads it back from the settings
            // signal, so the face and the record cannot disagree.
            let settings = Rc::clone(self.host.settings());
            if !settings.store_avatar_if_current(app, wallet.as_str(), face.svg.clone()) {
                log::warn!("a generated avatar no longer belonged to the signed-in account");

                return;
            }
        }
        state.face_generation = state.face_generation.wrapping_add(1);
        let svg = face.svg_smol();
        let pending = app.resources_mut().insert_async(face.png);
        state.face = Some(FaceTexture {
            pending,
            svg,
            generation: state.face_generation,
        });
    }

    /// Starts the tier query for `user_id`.
    ///
    /// The tier is reported against the generation it was asked for: a query that outlives the
    /// account it was about would otherwise label the next account.
    fn start_plan_query(&self, app: &mut App, state: &mut State, user_id: SmolStr) {
        let generation = state.plan_generation;
        state.plan_query = Some((generation, user_id.clone()));
        let accounts = Arc::clone(self.host.accounts());
        let (events, handle, window) = (state.events_tx.clone(), app.handle(), state.window);
        self.host.runtime().spawn(async move {
            let result = accounts.current_plan(user_id.as_str()).await;
            let _ = events.send(MenuEvent::Plan {
                generation,
                user_id,
                result,
            });
            handle.send(move |app| {
                let _ = app.request_repaint(window);
            });
        });
    }

    /// Applies a tier answer, unless it belongs to a query that is no longer current.
    fn apply_plan(
        &self,
        state: &mut State,
        generation: u64,
        user_id: &str,
        result: Result<Option<PlanCode>, kytrix_service_provider::auth::model::AuthError>,
    ) {
        let is_current = state.plan_generation == generation
            && state
                .snapshot
                .account
                .as_ref()
                .is_some_and(|account| account.user_id == user_id);
        if !is_current {
            log::debug!("dropped the answer to a tier query for an account that moved on");

            return;
        }
        state.plan_query = None;
        match result {
            Ok(plan) => state.plan = plan,
            Err(error) => {
                state.plan = None;
                log::warn!("failed to inspect the active account plan: {error}");
            }
        }
    }

    /// Stops believing the tier that is on screen.
    fn clear_plan(&self, state: &mut State) {
        state.plan_generation = state.plan_generation.wrapping_add(1);
        state.plan_query = None;
        state.plan = None;
    }

    /// Writes the tier's label into the signal the menu's badge reads.
    ///
    /// A language change, and a tier that arrives late, both land here: the badge is bound, so
    /// neither rebuilds the block.
    fn sync_plan_label(&self, app: &mut App, state: &mut State) {
        let label = plan_label(state.plan, self.host.translate());
        app.write(&state.signals.plan, label);
    }

    /// Builds the trigger (or rebuilds it when what it was built from changed).
    fn sync_trigger(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        let language = self.host.language(app);
        let shape = TriggerShape {
            signed_in: state.snapshot.account.is_some(),
            language,
            face: state.face.as_ref().map(|face| face.generation),
        };
        if state.trigger.shape != shape {
            self.rebuild_trigger(app, state)?;
        }
        state.trigger.button.sync(app)?;
        let open = *app.read(&state.open);
        state.trigger.button.set_selected(app, open);

        Ok(())
    }

    /// Replaces the trigger with one built from the values that changed.
    ///
    /// A rebuild under the same key is the frozen way to follow a build-time value
    /// (`crates/widgets/AGENTS.md` §15.1): the component is released before the new one registers
    /// the same action identity, and the node is removed first (E16's order).
    ///
    /// A menu that was open stays open: the overlay is anchored on the trigger's box, so the
    /// replacement hands it the new box rather than closing what the user is reading (a language
    /// change reaches this method, and a menu does not close because the application changed
    /// language).
    fn rebuild_trigger(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        let was_open = state.wants_open && state.overlay.is_open(app);
        // The old component goes first: it registered the action identity the new one is about to
        // register, and releasing it afterwards would take the new trigger's handler away with it
        // (`Button::release` is what unregisters `"<key>.click"`).
        remove_if_present(app, state.window, state.trigger.node);
        state.trigger.button.release(app);
        state.trigger = TriggerBuild {
            host: &self.host,
            window: state.window,
            parent: state.parent,
            key: &state.key,
            snapshot: &state.snapshot,
            face: state.face.as_ref(),
            requests: state.signals.request,
        }
        .build(app)?;
        state.tooltip.attach(app, state.window, state.trigger.node);
        if was_open {
            let reopened = state.overlay.open(
                app,
                state.window,
                Anchor::node(state.trigger.node),
                |_tree, _panel| Ok(()),
            );
            if let Err(error) = reopened {
                log::warn!("failed to re-anchor the account menu: {error}");
            }
        }

        Ok(())
    }

    /// Opens the account menu: the overlay, and the account block with its entries.
    fn open_menu(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        if state.overlay.is_open(app) {
            return Ok(());
        }
        let Some(account) = state.snapshot.account.as_ref() else {
            // Nobody is signed in: the control is a sign-in button, not a menu.
            return Ok(());
        };
        let opened = state
            .overlay
            .open(
                app,
                state.window,
                Anchor::node(state.trigger.node),
                |_tree, _panel| Ok(()),
            )
            .map_err(|error| overlay_error(state.window, error))?;
        if !opened {
            // The trigger has not been laid out yet (or its anchor is gone): the next frame that
            // follows the press opens the menu.
            log::debug!("the account menu could not be anchored yet");

            return Ok(());
        }
        let Some(panel) = state.overlay.panel_node() else {
            return Err(EngineError::UnknownWindow {
                window: state.window,
            });
        };
        let account = SignedInAccount::from(account);
        let content = MenuContent::build(
            app,
            &self.host,
            state.window,
            panel,
            &state.key,
            &account,
            state.face.as_ref(),
            &state.signals,
        )?;
        state.menu = Some(content);
        app.write(&state.open, true);

        Ok(())
    }

    /// Closes the account menu: the overlay fades out, and the content is released once it has
    /// finished closing.
    fn close_menu(&self, app: &mut App, state: &mut State) {
        state.wants_open = false;
        state.overlay.close(app);
    }

    /// Releases the menu's content components, if one is still held.
    fn release_menu_content(&self, app: &mut App, state: &mut State) {
        if let Some(content) = state.menu.take() {
            remove_if_present(app, state.window, content.root);
            content.release(app);
        }
    }

    /// One frame of the menu: the overlay's own fade / placement, and the content's components.
    ///
    /// The overlay is the only writer of the open state: a press toggles what the control
    /// *wants*, and this step makes the overlay agree — which is also how an outside click or
    /// `Escape` (the engine closing the overlay on its own) becomes the control's own state one
    /// frame later.
    fn sync_menu(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        let was_open = state.overlay.is_open(app);
        state.overlay.sync(app);
        if was_open && !state.overlay.is_open(app) {
            // Closed by the engine (an outside click, `Escape`): a menu nobody chose to keep open
            // is not one the next frame reopens.
            state.wants_open = false;
        }
        if !state.overlay.is_open(app) {
            // Fading out (or already closed): the content's components are given back, and the
            // wish to be open is honoured once the overlay is gone.
            app.write(&state.open, false);
            if state.wants_open && state.snapshot.account.is_some() {
                self.open_menu(app, state)?;
            } else {
                self.release_menu_content(app, state);
            }

            return Ok(());
        }
        app.write(&state.open, true);
        let Some(account) = state.snapshot.account.as_ref() else {
            // The account went away while its menu was open: the menu goes with it.
            self.close_menu(app, state);

            return Ok(());
        };
        let shape = MenuShape {
            language: self.host.language(app),
            face: state.face.as_ref().map(|face| face.generation),
        };
        if state
            .menu
            .as_ref()
            .is_some_and(|content| content.shape != shape)
        {
            let account = account.clone();
            self.rebuild_menu_content(app, state, &account)?;
        }
        if let Some(content) = state.menu.as_ref() {
            content.sync(app)?;
        }

        Ok(())
    }

    /// Rebuilds the menu's content for the values that changed (a language, a new face).
    fn rebuild_menu_content(
        &self,
        app: &mut App,
        state: &mut State,
        account: &AccountConfig,
    ) -> Result<(), EngineError> {
        let Some(panel) = state.overlay.panel_node() else {
            return Ok(());
        };
        if let Some(previous) = state.menu.take() {
            remove_if_present(app, state.window, previous.root);
            previous.release(app);
        }
        let account = SignedInAccount::from(account);
        let content = MenuContent::build(
            app,
            &self.host,
            state.window,
            panel,
            &state.key,
            &account,
            state.face.as_ref(),
            &state.signals,
        )?;
        state.menu = Some(content);

        Ok(())
    }

    /// Keeps the hint under the face saying what pressing it does.
    fn sync_tooltip(&self, app: &mut App, state: &mut State) {
        let open = *app.read(&state.open);
        let content = if open {
            // The menu occupies the space under the face, and a hint left over its first entry
            // would take the presses meant for the entry.
            None
        } else {
            Some(TooltipContent::new(hint_label(
                &state.snapshot,
                self.host.translate(),
            )))
        };
        state.tooltip.set_content(content);
        state.tooltip.sync(app);
    }

    /// Syncs the alert a flow is being reported in, and releases it once it has gone.
    fn sync_alert(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        let Some(alert) = state.alert.clone() else {
            return Ok(());
        };
        alert
            .sync(app)
            .map_err(|error| dialog_error(state.window, error))?;
        if !alert.is_open(app) {
            state.alert = None;
            alert.release(app);
        }

        Ok(())
    }

    /// Shows an alert, replacing the one that is up.
    fn show_alert(
        &self,
        app: &mut App,
        state: &mut State,
        content: AlertContent,
    ) -> Result<(), EngineError> {
        let key = format_smolstr!("{}:alert", state.key);
        let alert = dialogs::build(app, &self.host, key, content);
        alert
            .open(app, state.window)
            .map_err(|error| dialog_error(state.window, error))?;
        if let Some(previous) = state.alert.replace(alert) {
            previous.release(app);
        }

        Ok(())
    }

    /// Dismisses the alert that is up, if any (the wait it reported is over and there is nothing
    /// to report about it).
    fn dismiss_alert(&self, app: &mut App, state: &mut State) {
        if let Some(alert) = state.alert.as_ref() {
            alert.close(app);
        }
    }

    /// Starts a sign-in: prepares the attempt, opens the browser, and waits.
    ///
    /// The attempt lives exactly as long as the alert that reports it. Reading that alert is how
    /// the user says they are done with this attempt, so the alert cancels it — there is nothing
    /// behind it any more, and the control offers a sign-in again the moment the alert is gone. An
    /// attempt nothing is waiting on would be a listener holding a loopback port for five minutes
    /// while the user has no way to ask for another one.
    ///
    /// A sign-in that cannot even be prepared says so in the same alert the wait would have used:
    /// the user asked for something and is owed the reason it did not happen, in the place they
    /// asked from.
    fn start_sign_in(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        // Whoever asks for a sign-in asks for the one they are about to get: an attempt left over
        // from the last time is ended here rather than left to be reported later, over the alert
        // this one is about to open.
        self.cancel_sign_in(app, state);
        let translate = self.host.translate();
        let dpop_key = match self.host.accounts().prepare_login() {
            Ok(dpop_key) => dpop_key,
            Err(error) => {
                return self.show_alert(
                    app,
                    state,
                    AlertContent::Report {
                        title: t_smol!(translate, "window.account.sign_in"),
                        message: AccountFailure::Service(&error).translated_message(translate),
                        confirm: t_smol!(translate, "common.confirm"),
                        on_confirm: None,
                    },
                );
            }
        };
        let attempt = match oauth::prepare_login(dpop_key) {
            Ok(attempt) => attempt,
            Err(error) => {
                return self.show_alert(
                    app,
                    state,
                    AlertContent::Report {
                        title: t_smol!(translate, "window.account.sign_in"),
                        message: error.translated_message(translate),
                        confirm: t_smol!(translate, "common.confirm"),
                        on_confirm: None,
                    },
                );
            }
        };
        let authorization_url = attempt.authorization_url().to_owned();
        let accounts = Arc::clone(self.host.accounts());
        let (cancel, cancelled) = oneshot::channel();
        let (events, handle, window) = (state.events_tx.clone(), app.handle(), state.window);
        self.host.runtime().spawn(async move {
            let exchange = move |dpop_key, code, redirect_uri, code_verifier| {
                let accounts = Arc::clone(&accounts);
                async move {
                    accounts
                        .exchange_authorization_code(dpop_key, code, redirect_uri, code_verifier)
                        .await
                }
            };
            let outcome = tokio::select! {
                result = oauth::run_login(attempt, exchange) => Some(result),
                // The attempt was cancelled (the alert was read, the control was released): the
                // listener goes back to the system with the dropped future.
                _ = cancelled => None,
            };
            let _ = events.send(MenuEvent::SignIn { outcome });
            handle.send(move |app| {
                let _ = app.request_repaint(window);
            });
        });
        state.sign_in = Some(cancel);
        // The alert is the attempt's own report, so reading it is what cancels the attempt: the
        // request the button writes is taken in `sync`, where the control holds the sender.
        let request = state.signals.request;
        let confirm = t_smol!(translate, "window.account.cancel_sign_in");
        let on_confirm: AlertAction = Rc::new(move |app: &mut App| {
            let _ = app.write(&request, Some(MenuRequest::CancelSignIn));
            let _ = app.request_repaint(window);
        });
        self.show_alert(
            app,
            state,
            AlertContent::Report {
                title: t_smol!(translate, "window.account.sign_in"),
                message: t_smol!(translate, "window.account.awaiting_authorization"),
                confirm,
                on_confirm: Some(on_confirm),
            },
        )?;
        state.awaiting_alert = true;
        self.host.open_url(&authorization_url);

        Ok(())
    }

    /// Ends the attempt whose browser callback has not arrived.
    fn cancel_sign_in(&self, app: &mut App, state: &mut State) {
        // Dropping the sender is the cancellation: the task's `select!` loses `cancelled` and the
        // attempt (with its loopback listener) is dropped with the future.
        state.sign_in = None;
        state.awaiting_alert = false;
        self.dismiss_alert(app, state);
    }

    /// Accepts the end of the attempt that was waiting for its browser callback.
    fn finish_sign_in(
        &self,
        app: &mut App,
        state: &mut State,
        outcome: Option<Result<AccountConfig, LoginError>>,
    ) -> Result<(), EngineError> {
        state.sign_in = None;
        let was_awaiting = std::mem::take(&mut state.awaiting_alert);
        let translate = self.host.translate();
        match outcome {
            // The account is stored here rather than after a frame: it is the news itself, and
            // the settings write is what asks for the frame that draws it.
            Some(Ok(account)) => {
                let wallet = SmolStr::new(account.wallet_address.as_str());
                let kind = crate::account::wallet_kind_label(account.wallet_kind);
                self.host.settings().store_account(app, account);
                self.show_alert(
                    app,
                    state,
                    AlertContent::Report {
                        title: t_smol!(translate, "window.account.authorization_complete"),
                        message: t_smol!(
                            translate,
                            "window.account.authorization_complete_message",
                            wallet = wallet,
                            kind = kind
                        ),
                        confirm: t_smol!(translate, "common.confirm"),
                        on_confirm: None,
                    },
                )
            }
            // The alert the user is watching becomes the report: replacing it in place says what
            // went wrong without asking them to go looking.
            Some(Err(error)) => self.show_alert(
                app,
                state,
                AlertContent::Report {
                    title: t_smol!(translate, "window.account.sign_in"),
                    message: error.translated_message(translate),
                    confirm: t_smol!(translate, "common.confirm"),
                    on_confirm: None,
                },
            ),
            // Nothing to report: the attempt expired, or the user read the alert already. An
            // alert of this attempt's is taken down either way, which is a dismissal of nothing
            // when it is the one that was read.
            None => {
                // Only the wait's own alert is taken down: a report that replaced it belongs to
                // a wait that is already over, and dismissing it would take down news the user
                // has not read.
                if was_awaiting {
                    self.dismiss_alert(app, state);
                }

                Ok(())
            }
        }
    }

    /// Opens the account dashboard in the browser from the signed-in account.
    ///
    /// The browser receives only a one-time code, and the account stays signed in here. A failure
    /// is reported where the user asked for it, like a failed sign-in.
    fn open_account(&self, app: &mut App, state: &mut State) {
        let Some(user_id) = state
            .snapshot
            .account
            .as_ref()
            .map(|account| SmolStr::new(account.user_id.as_str()))
        else {
            return;
        };
        state.handoff = Some(user_id.clone());
        let accounts = Arc::clone(self.host.accounts());
        let (events, handle, window) = (state.events_tx.clone(), app.handle(), state.window);
        self.host.runtime().spawn(async move {
            let result = accounts.create_dashboard_handoff().await;
            let _ = events.send(MenuEvent::Handoff { user_id, result });
            handle.send(move |app| {
                let _ = app.request_repaint(window);
            });
        });
    }

    /// Opens the dashboard URL, unless the account it was requested for is gone.
    fn finish_handoff(
        &self,
        app: &mut App,
        state: &mut State,
        user_id: &str,
        result: Result<String, kytrix_service_provider::auth::model::AuthError>,
    ) -> Result<(), EngineError> {
        state.handoff = None;
        let is_current = state
            .snapshot
            .account
            .as_ref()
            .is_some_and(|account| account.user_id == user_id);
        if !is_current {
            log::debug!("dropped a dashboard handoff for an account that moved on");

            return Ok(());
        }
        match result {
            Ok(code) => {
                self.host
                    .open_url(format_smolstr!("{ACCOUNT_ORIGIN}/dashboard#{code}").as_str());

                Ok(())
            }
            Err(error) => {
                let translate = self.host.translate();
                self.show_alert(
                    app,
                    state,
                    AlertContent::Report {
                        title: t_smol!(translate, "window.account.open_account"),
                        message: AccountFailure::Service(&error).translated_message(translate),
                        confirm: t_smol!(translate, "common.confirm"),
                        on_confirm: None,
                    },
                )
            }
        }
    }

    /// Asks whether the account should be signed out.
    fn confirm_sign_out(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        if state.snapshot.account.is_none() {
            return Ok(());
        }
        let translate = self.host.translate();
        let request = state.signals.request;
        let window = state.window;
        let on_confirm: AlertAction = Rc::new(move |app: &mut App| {
            let _ = app.write(&request, Some(MenuRequest::ConfirmSignOut));
            let _ = app.request_repaint(window);
        });
        self.show_alert(
            app,
            state,
            AlertContent::Question {
                title: t_smol!(translate, "window.account.sign_out"),
                message: t_smol!(translate, "window.account.sign_out_confirm"),
                confirm: t_smol!(translate, "window.account.sign_out"),
                cancel: t_smol!(translate, "common.cancel"),
                on_confirm,
            },
        )
    }

    /// Ends the account session and forgets the account, here and now.
    ///
    /// Nothing is asked of the account service first and nothing is waited for: the credential
    /// this machine holds is removed, the session behind it is retired, and the account is
    /// dropped from the settings, all before this returns. Telling the service is a report that
    /// follows, on a task nobody waits for — which is what makes a sign-out impossible to fail.
    ///
    /// A sign-in still waiting for its browser is ended with the session: a callback that arrived
    /// after this would sign the user back into the account they just left.
    fn sign_out(&self, app: &mut App, state: &mut State) -> Result<(), EngineError> {
        if state.snapshot.account.is_none() {
            return Ok(());
        }
        self.clear_plan(state);
        self.close_menu(app, state);
        self.cancel_sign_in(app, state);
        let revocation = self.host.accounts().discard_local_session();
        self.host.settings().clear_account(app);
        if let Some(revocation) = revocation {
            let accounts = Arc::clone(self.host.accounts());
            // Telling the account service is a request, and the frame thread must not wait for
            // one: it is handed to the application runtime, like every other call this control
            // makes to the service.
            self.host.runtime().spawn(async move {
                accounts.revoke_session(revocation).await;
            });
        }

        Ok(())
    }

    /// Forgets an account whose credential the account session has invalidated.
    fn forget_invalidated_account(&self, app: &mut App, state: &mut State) {
        if state.snapshot.account.is_none() {
            return;
        }
        self.host.settings().clear_account(app);
    }

    /// Starts watching the account session for the moment the credential stops being valid.
    ///
    /// The query tells us what the credential on disk says now; the watch tells us what happens
    /// next. They are reconciled by [`SessionWatch`], because neither has seen the other's news.
    fn start_session_watch(&self, app: &mut App, state: &mut State) {
        let accounts = Arc::clone(self.host.accounts());
        let (cancel, cancelled) = oneshot::channel();
        let (events, handle, window) = (state.events_tx.clone(), app.handle(), state.window);
        self.host.runtime().spawn(async move {
            let queried = accounts.local_session().await;
            let mut watch = SessionWatch::new(accounts.session_changes());
            let mut clears_account = watch.reconcile(queried);
            let mut cancelled = cancelled;
            loop {
                if clears_account {
                    let _ = events.send(MenuEvent::Invalidated);
                    handle.send(move |app| {
                        let _ = app.request_repaint(window);
                    });
                }
                let next = tokio::select! {
                    _ = &mut cancelled => return,
                    next = watch.changed() => next,
                };
                match next {
                    Some(clears) => clears_account = clears,
                    // The account session is gone: nothing more will be published.
                    None => return,
                }
            }
        });
        state.session = Some(cancel);
    }

    // The read points and drives below are test-only: a shell drives this control through its
    // public stages (`new`/`build`/`sync`/`release`) and reads it through the public accessors
    // above, while a test also needs to *look inside* the built state (the hint's content, the
    // menu's content, the face it holds) and to hand the control a background result without a
    // browser or a blocking pool.
    /// The hint under the trigger.
    #[cfg(test)]
    pub(crate) fn hint(&self) -> Option<Tooltip> {
        self.state.as_ref().map(|state| state.tooltip.clone())
    }

    /// The menu's overlay (its panel and whether it is open).
    #[cfg(test)]
    pub(crate) fn menu_overlay(&self) -> Option<&Overlay> {
        self.state.as_ref().map(|state| &state.overlay)
    }

    /// The menu's content, while the menu is open.
    #[cfg(test)]
    pub(crate) fn content(&self) -> Option<&MenuContent> {
        self.state.as_ref().and_then(|state| state.menu.as_ref())
    }

    /// The alert that is up, if one is.
    #[cfg(test)]
    pub(crate) fn alert(&self) -> Option<&Dialog> {
        self.state.as_ref().and_then(|state| state.alert.as_ref())
    }

    /// The alert's confirming button (the one that says a report was read, or answers yes).
    #[cfg(test)]
    pub(crate) fn alert_confirm(&self) -> Option<Button> {
        self.alert()?.with_alert(|alert| alert.ok_button().clone())
    }

    /// The alert's declining button, when it asks a question.
    #[cfg(test)]
    pub(crate) fn alert_cancel(&self) -> Option<Button> {
        self.alert()?
            .with_alert(|alert| alert.cancel_button().cloned())
            .flatten()
    }

    /// The face the control holds, as `(the SVG it was rasterized from, its generation)`.
    #[cfg(test)]
    pub(crate) fn face(&self) -> Option<(&SmolStr, u64)> {
        self.state
            .as_ref()
            .and_then(|state| state.face.as_ref())
            .map(|face| (&face.svg, face.generation))
    }

    /// The tier's label as the menu's badge reads it.
    #[cfg(test)]
    pub(crate) fn plan_label(&self, app: &App) -> Option<SmolStr> {
        self.state
            .as_ref()
            .map(|state| app.read(&state.signals.plan).clone())
    }

    /// Hands the control a background result, the way a spawned task would.
    #[cfg(test)]
    pub(crate) fn deliver(&self, event: MenuEvent) {
        if let Some(state) = self.state.as_ref() {
            let _ = state.events_tx.send(event);
        }
    }

    /// Delivers the end of a sign-in attempt, the way the task waiting on the browser does.
    #[cfg(test)]
    pub(crate) fn finish_sign_in_for_test(
        &mut self,
        app: &mut App,
        outcome: Option<Result<AccountConfig, LoginError>>,
    ) {
        self.deliver(MenuEvent::SignIn { outcome });
        let _ = self.sync(app);
    }

    /// Starts a sign-in from the control, the way a shell command might.
    #[cfg(test)]
    pub(crate) fn start_sign_in_for_test(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(mut state) = self.state.take() else {
            return Ok(());
        };
        let result = self.start_sign_in(app, &mut state);
        self.state = Some(state);

        result
    }

    /// Asks the sign-out question from the control, the way the menu's entry does.
    #[cfg(test)]
    pub(crate) fn confirm_sign_out_for_test(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(mut state) = self.state.take() else {
            return Ok(());
        };
        let result = self.confirm_sign_out(app, &mut state);
        self.state = Some(state);

        result
    }

    /// Signs out from the control, the way the confirmed question does.
    #[cfg(test)]
    pub(crate) fn sign_out_for_test(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(mut state) = self.state.take() else {
            return Ok(());
        };
        let result = self.sign_out(app, &mut state);
        self.state = Some(state);

        result
    }

    /// The tier query generation the control believes in (a stale answer is recognised by it).
    #[cfg(test)]
    pub(crate) fn plan_generation(&self) -> Option<u64> {
        self.state.as_ref().map(|state| state.plan_generation)
    }
}

/// The values one trigger build works from.
///
/// The trigger is built twice in a control's life (once at `build`, and again when the face it
/// draws or the language it speaks changes), and it is the same square both times: the shape it
/// was built from is exactly what a rebuild compares.
struct TriggerBuild<'a> {
    host: &'a ProfileHost,
    window: WindowId,
    parent: NodeId,
    key: &'a str,
    snapshot: &'a crate::host::AccountSnapshot,
    face: Option<&'a FaceTexture>,
    requests: Signal<Option<MenuRequest>>,
}

impl TriggerBuild<'_> {
    /// Builds the trigger: the title bar's square, holding the account's face — or, while nobody
    /// is signed in, the mark that says pressing it starts a sign-in.
    fn build(&self, app: &mut App) -> Result<Trigger, EngineError> {
        let language = self.host.language(app);
        let button = Button::new(
            app,
            self.host.theme().clone(),
            format_smolstr!("{key}:trigger", key = self.key),
        )
        .ghost()
        .small()
        .a11y_label(trigger_a11y_label(self.snapshot, self.host.translate()))
        .on_click(trigger_on_click(self.requests));
        let node = button.build(app, self.window, self.parent)?;
        let face_size = if self.face.is_some() {
            // The account's own picture takes more of the square than a mark does.
            TRIGGER_ACCOUNT_AVATAR_SIZE
        } else {
            TRIGGER_AVATAR_SIZE
        };
        let face = NodeBuilder::boxed()
            .id(TRIGGER_AVATAR_ID)
            .style(
                base_ui::style_builder::StyleBuilder::new()
                    .size(face_size, face_size)
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(
                app.window_mut(self.window)
                    .ok_or(EngineError::UnknownWindow {
                        window: self.window,
                    })?
                    .tree_mut(),
                node,
            )?;
        match self.face {
            Some(texture) => {
                NodeBuilder::image_pending(texture.pending, ui_core::node::image::ImageFit::Cover)
                    .style(
                        base_ui::style_builder::StyleBuilder::new()
                            .size(face_size, face_size)
                            .build(),
                    )
                    .build(
                        app.window_mut(self.window)
                            .ok_or(EngineError::UnknownWindow {
                                window: self.window,
                            })?
                            .tree_mut(),
                        face,
                    )?;
            }
            None => {
                base_ui::kit::icon::IconSpec::new(base_ui::kit::icon::IconKind::User)
                    .with_size(face_size)
                    .build_themed(
                        app,
                        self.window,
                        face,
                        self.host.theme(),
                        base_ui::theme::role::Role::FgMuted,
                    )
                    .map_err(|error| trigger_icon_error(self.window, error))?;
            }
        }

        Ok(Trigger {
            button,
            node,
            shape: TriggerShape {
                signed_in: self.snapshot.account.is_some(),
                language,
                face: self.face.as_ref().map(|face| face.generation),
            },
        })
    }
}

/// The trigger's press: the control takes the request in its own `sync`.
fn trigger_on_click(
    request: Signal<Option<MenuRequest>>,
) -> impl Fn(&mut ui_core::interaction::ActionCtx<'_>) + 'static {
    move |ctx| {
        ctx.write(&request, Some(MenuRequest::Trigger));
        let window = ctx.window();
        let _ = ctx.app_mut().request_repaint(window);
    }
}

/// An icon's engine failure, as the engine's own error.
fn trigger_icon_error(window: WindowId, error: base_ui::kit::icon::IconError) -> EngineError {
    match error {
        base_ui::kit::icon::IconError::Node(error) => error,
        _ => EngineError::UnknownWindow { window },
    }
}

/// The label the trigger carries in the a11y tree and (signed out) in its hint.
fn trigger_a11y_label(snapshot: &crate::host::AccountSnapshot, translate: &Translate) -> SmolStr {
    match snapshot.account.as_ref() {
        // The address is shortened by the rule the menu's own line uses, so the hint and the menu
        // cannot disagree about which account is signed in.
        Some(account) => SignedInAccount::from(account).shortened_address().clone(),
        None => t_smol!(translate, "window.account.sign_in"),
    }
}

/// What the hint under the trigger says pressing it does.
fn hint_label(snapshot: &crate::host::AccountSnapshot, translate: &Translate) -> SmolStr {
    trigger_a11y_label(snapshot, translate)
}

/// The key a face job is identified by.
fn face_job_key(wallet: &str, stored: Option<&str>) -> SmolStr {
    match stored {
        Some(svg) => format_smolstr!("{wallet}|{svg}"),
        None => format_smolstr!("{wallet}|"),
    }
}

/// Removes one of this control's own nodes, tolerating a node that is already gone (the engine
/// removed it with the overlay it lived in, or the caller removed the subtree first).
fn remove_if_present(app: &mut App, window: WindowId, node: NodeId) {
    let present = app
        .window(window)
        .is_some_and(|target| target.tree().get(node).is_ok());
    if present && let Err(error) = app.remove_node(window, node) {
        log::warn!("failed to remove an account control node: {error}");
    }
}

/// A dialog failure, as the engine's own error.
fn dialog_error(window: WindowId, error: DialogError) -> EngineError {
    match error {
        DialogError::Engine(error) => error,
        DialogError::Overlay(OverlayError::Engine(error)) => error,
        DialogError::Overlay(OverlayError::PlacementConfig { .. }) | DialogError::NotOpen => {
            EngineError::UnknownWindow { window }
        }
    }
}

/// An overlay failure, as the engine's own error.
fn overlay_error(window: WindowId, error: OverlayError) -> EngineError {
    match error {
        OverlayError::Engine(error) => error,
        OverlayError::PlacementConfig { .. } => EngineError::UnknownWindow { window },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_ui::kit::{layer::Band, size::UnifiedSize};
    use i18n::{language::codes::LanguageCode, t_smol};
    use kytrix_service_provider::auth::{
        constants::ACCOUNT_ORIGIN,
        model::{PlanCode, SessionAvailability},
    };
    use smol_str::SmolStr;
    use ui_core::{math::Size, node::tree::NodeKind};
    use widget::error::TranslatedError;

    use super::{MenuEvent, ProfileMenu};
    use crate::{
        account::shortened_wallet_address,
        host::{AccountSettings, AccountSnapshot},
        menu::{ACCOUNT_AVATAR_SIZE, MENU_WIDTH},
        oauth::{AccountFailure, LoginError},
        profile::TriggerShape,
        test_support::{
            AVATAR_SVG, ETHEREUM, HANDOFF_CODE, Harness, SOLANA, Step, configured,
            ethereum_account, other_account, translate,
        },
    };

    /// The translation context the harness renders through.
    fn en() -> i18n::translate::Translate {
        translate(LanguageCode::En)
    }

    /// The Chinese catalogue, for the tests that switch the language.
    fn zh() -> i18n::translate::Translate {
        translate(LanguageCode::ZhCn)
    }

    /// The hint's text, or a panic naming what is missing.
    fn hint_text(harness: &Harness) -> SmolStr {
        harness
            .menu()
            .hint()
            .expect("the control builds a hint")
            .content()
            .expect("the hint says what pressing the control does")
            .text
    }

    /// The text the subtree under the first node whose debug id starts with `prefix` draws.
    fn drawn(harness: &Harness, prefix: &str) -> String {
        let node = harness
            .find(prefix)
            .unwrap_or_else(|| panic!("nothing draws {prefix}; ids: {:?}", harness.debug_ids()));
        harness.text_of(node)
    }

    /// The layout box of the first node whose debug id starts with `prefix`.
    fn bounds(harness: &Harness, prefix: &str) -> ui_core::math::Rect {
        let node = harness
            .find(prefix)
            .unwrap_or_else(|| panic!("nothing draws {prefix}; ids: {:?}", harness.debug_ids()));
        harness
            .bounds(node)
            .unwrap_or_else(|| panic!("{prefix} is not laid out"))
    }

    /// The screen centre of a layout box.
    fn centre(rect: ui_core::math::Rect) -> ui_core::math::Point {
        ui_core::math::Point::new(
            rect.origin.x + rect.size.width / 2.0,
            rect.origin.y + rect.size.height / 2.0,
        )
    }

    /// Opens the account menu and asserts it opened.
    fn show_menu(harness: &mut Harness) {
        harness.open_menu();
        assert!(
            harness.menu().is_menu_open(harness.app()),
            "the menu opens on a press; ids: {:?}",
            harness.debug_ids()
        );
    }

    /// The shortened address of the fixture account.
    fn shortened() -> SmolStr {
        shortened_wallet_address(ETHEREUM, app_config::settings::AccountWalletKind::Ethereum)
    }

    /// A control whose settings hold no account and whose account service answers as configured.
    fn signed_out_with(
        configure: impl FnOnce(&crate::test_support::FakeAccountService),
    ) -> Harness {
        let accounts = crate::test_support::FakeAccountService::new(ethereum_account());
        configure(&accounts);

        Harness::with(AccountSnapshot::default(), accounts)
    }

    /// A control whose settings hold the fixture account and whose service is configured.
    fn signed_in_with(configure: impl FnOnce(&crate::test_support::FakeAccountService)) -> Harness {
        let accounts = crate::test_support::FakeAccountService::new(ethereum_account());
        configure(&accounts);

        Harness::with(configured(ethereum_account()), accounts)
    }

    /* ── the control ─────────────────────────────────────────────────────── */

    /// Signed out, the control is the title bar's own square, and its placeholder face is drawn
    /// at the size the bar's other marks take.
    #[test]
    fn the_control_should_be_the_title_bars_square() {
        let harness = Harness::new();
        let trigger = harness
            .menu()
            .trigger_node()
            .expect("the control builds a trigger");

        assert_eq!(
            harness
                .bounds(trigger)
                .expect("the trigger is laid out")
                .size
                .height,
            UnifiedSize::Small.h(),
            "the control is the title bar's control height"
        );
        assert_eq!(
            bounds(&harness, "TEST_PROFILE_TRIGGER_AVATAR").size,
            Size::new(16.0, 16.0),
            "the placeholder face is the size the title bar's other marks take"
        );
    }

    /// Signed out, the press is the sign-in itself: the control asks the account service for an
    /// attempt, opens the browser and waits, and there is no menu between the two.
    #[test]
    fn pressing_the_control_with_nobody_signed_in_should_start_a_sign_in() {
        let mut harness = Harness::new();

        harness.click_trigger();
        harness.settle(3);

        assert_eq!(
            harness.accounts().count(|state| state.prepared),
            1,
            "the press asks the account service for a sign-in"
        );
        assert!(
            harness.menu().is_waiting_for_browser(),
            "the control waits for the browser the press opened"
        );
        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the wait is reported where the user is"
        );
        assert!(
            !harness.menu().is_menu_open(harness.app()),
            "a signed-out control opens no menu between the press and the sign-in"
        );
        assert!(
            harness
                .opened_url()
                .is_some_and(|url| url.contains("/login?")),
            "the press opens the authorization page: {:?}",
            harness.opened_url()
        );
    }

    /// Signed out, the hint on the control is the one thing its press does.
    #[test]
    fn the_hint_on_a_signed_out_control_should_be_the_sign_in() {
        let harness = Harness::new();

        assert_eq!(
            hint_text(&harness),
            t_smol!(en(), "window.account.sign_in"),
            "the hint is the sign-in the control performs"
        );
    }

    /// Signed in, the hint is the account whose menu the press opens, shortened by the rule the
    /// menu's own address line uses.
    #[test]
    fn the_hint_on_a_signed_in_control_should_be_the_shortened_address() {
        let harness = Harness::signed_in();

        assert_eq!(
            hint_text(&harness),
            shortened(),
            "the hint names the account the menu is about"
        );
    }

    /// The hint is a real tooltip: it waits for its delay and then shows, and it goes away when
    /// the pointer does.
    #[test]
    fn hovering_the_control_should_show_the_hint() {
        let mut harness = Harness::new();
        harness.hover_trigger();
        harness.settle(2);
        let hint = harness.menu().hint().expect("the control builds a hint");
        assert!(
            !hint.is_visible(harness.app()),
            "a hint shows after its delay, not on the frame the pointer arrives"
        );

        harness.advance(Duration::from_millis(600));
        // The advance dispatches the delay timer in its first turn; the turns after it are what
        // the control's own sync needs to open the hint the timer asked for.
        harness.settle(2);

        assert!(
            hint.is_visible(harness.app()),
            "the hint shows once its delay passed"
        );
    }

    /// The menu occupies the space under the face, so the hint that would be drawn there is
    /// withdrawn while the menu is down.
    #[test]
    fn an_open_menu_should_take_the_hint_away() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);

        assert_eq!(
            harness.menu().hint().expect("a hint").content(),
            None,
            "a hint over the menu's first entry would take the presses meant for it"
        );
    }

    /* ── the menu ────────────────────────────────────────────────────────── */

    /// The menu is the account's: the face with the wallet family's mark, the tier, the address
    /// and the two entries, at the one width the account block is centred on.
    #[test]
    fn the_menu_should_show_the_signed_in_account() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);

        let panel = harness
            .menu()
            .menu_overlay()
            .and_then(|overlay| overlay.panel_node())
            .expect("the menu draws a panel");
        assert_eq!(
            harness
                .bounds(panel)
                .expect("the panel is laid out")
                .size
                .width,
            MENU_WIDTH,
            "the menu is the one width the account block is centred on"
        );
        assert_eq!(
            harness
                .menu()
                .menu_overlay()
                .expect("the menu")
                .options()
                .band,
            Band::Popup,
            "a menu belongs in the popup band"
        );
        assert_eq!(
            harness.overlay_count(),
            1,
            "the menu is one overlay in the window"
        );
        for (id, what) in [
            ("TEST_PROFILE_MENU", "the menu's content"),
            ("TEST_PROFILE_ACCOUNT", "the account block"),
            ("TEST_PROFILE_ACCOUNT_AVATAR", "the account's face"),
            ("TEST_PROFILE_BRAND", "the wallet family's mark"),
            ("TEST_PROFILE_PLAN", "the subscription tier"),
            ("TEST_PROFILE_ADDRESS", "the account's address"),
        ] {
            assert!(harness.find(id).is_some(), "the menu does not draw {what}");
        }
        let address = harness
            .find("TEST_PROFILE_ADDRESS")
            .expect("the account address is drawn");
        assert_eq!(harness.node_text(address), shortened());
        let menu_root = harness
            .find("TEST_PROFILE_MENU")
            .expect("the menu is drawn");
        assert!(
            harness.has_text(
                menu_root,
                t_smol!(en(), "window.account.open_account").as_str()
            ),
            "the menu offers the account entry"
        );
        assert!(
            harness.has_text(menu_root, t_smol!(en(), "window.account.sign_out").as_str()),
            "the menu offers the sign-out entry"
        );
        assert_eq!(drawn(&harness, "TEST_PROFILE_PLAN"), "Plus");
        let copies = harness
            .debug_ids()
            .into_iter()
            .filter(|id| id.ends_with(":copy:interactive"))
            .count();
        assert_eq!(
            copies, 1,
            "the menu draws one copy control, not one for every place that could build it"
        );
    }

    /// The account block is a column: the face with the wallet family's mark on its lower-right
    /// corner, the tier, and the address, each line under the one above it.
    #[test]
    fn a_signed_in_menu_should_stack_the_account_block_top_to_bottom() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);

        let panel = bounds(&harness, "TEST_PROFILE_ACCOUNT");
        let face = bounds(&harness, "TEST_PROFILE_ACCOUNT_AVATAR");
        let badge = bounds(&harness, "TEST_PROFILE_BRAND");
        let tier = bounds(&harness, "TEST_PROFILE_PLAN");
        let address = bounds(&harness, "TEST_PROFILE_ADDRESS");

        assert_eq!(
            face.size,
            Size::new(ACCOUNT_AVATAR_SIZE, ACCOUNT_AVATAR_SIZE),
            "the face of the account block is the largest thing in it"
        );
        assert!(
            face.origin.y + face.size.height <= tier.origin.y,
            "the tier belongs under the face"
        );
        assert!(
            tier.origin.y + tier.size.height <= address.origin.y,
            "the address belongs under the tier"
        );
        assert_eq!(
            badge.size,
            Size::new(20.0, 20.0),
            "the wallet family is a corner mark, not a line of its own"
        );
        assert!(
            centre(badge).x > centre(face).x && centre(badge).y > centre(face).y,
            "the badge belongs on the lower-right corner of the face"
        );
        assert!(
            badge.origin.x + badge.size.width <= panel.origin.x + panel.size.width,
            "a badge on the face may not hang out of the menu"
        );
    }

    /// The address on the line is the shortened one, so hovering it is how the user reads the
    /// whole thing without copying it.
    #[test]
    fn hovering_the_address_should_offer_the_complete_one() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);
        let hint = harness
            .menu()
            .content()
            .expect("the menu is open")
            .address_hint
            .clone();
        assert_eq!(
            hint.content().map(|content| content.text),
            Some(SmolStr::new(ETHEREUM)),
            "the hint carries the complete address"
        );
        assert!(
            !hint.is_visible(harness.app()),
            "a tooltip nobody hovered is a tooltip in the way of what is behind it"
        );

        let address = harness
            .find("TEST_PROFILE_ADDRESS")
            .expect("the account block draws the shortened address");
        let address = harness.bounds(address).expect("the address is laid out");
        harness.move_pointer(centre(address));
        harness.settle(2);
        harness.advance(Duration::from_millis(600));
        harness.settle(2);

        assert!(
            hint.is_visible(harness.app()),
            "hovering the address shows the complete one"
        );
    }

    /// A tier the service did not answer is said with the stand-in sentence rather than left
    /// blank: a badge with no text reads as a badge with no tier, which is not what "unknown"
    /// means.
    #[test]
    fn an_unknown_tier_should_be_shown_as_a_stand_in_rather_than_left_blank() {
        let mut harness = signed_in_with(|accounts| accounts.with(|state| state.plan = Some(None)));
        harness.settle_until(20, |harness| harness.menu().plan().is_none());
        show_menu(&mut harness);

        assert_eq!(
            drawn(&harness, "TEST_PROFILE_PLAN"),
            t_smol!(en(), "window.account.plan_unavailable")
        );
    }

    /// The copy control asks for the complete address, which is what the application puts on the
    /// clipboard (a headless host has no clipboard, so the request is what a test reads).
    #[test]
    fn the_copy_entry_should_copy_the_complete_address() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);
        let copy = harness
            .menu()
            .content()
            .expect("the menu is open")
            .copy
            .node()
            .expect("the copy control is built");

        harness.click_node(copy);
        harness.settle(2);

        assert_eq!(
            harness.menu().copied_address(harness.app()),
            Some(SmolStr::new(ETHEREUM)),
            "the copy control asks for the complete address"
        );
        assert_ne!(
            shortened_wallet_address(ETHEREUM, app_config::settings::AccountWalletKind::Ethereum),
            ETHEREUM,
            "the address on the line is not the complete one"
        );
    }

    /// The menu belongs to the account that opened it: a signed-out control is a button, and the
    /// state that said a menu was open is not the next account's to inherit.
    #[test]
    fn a_menu_left_open_when_the_account_goes_away_should_not_open_for_the_next_one() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);

        harness.clear_account();
        harness.settle(3);
        assert!(
            !harness.menu().is_menu_open(harness.app()),
            "the menu goes with the account it was about"
        );

        harness.store_account(ethereum_account());
        harness.settle(3);
        assert!(
            !harness.menu().is_menu_open(harness.app()),
            "the next account opens with its menu closed"
        );
    }

    /// Choosing the sign-out entry takes the menu down before the question it asks opens.
    #[test]
    fn choosing_the_sign_out_entry_should_close_the_menu_and_ask() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);
        let entry = harness
            .menu()
            .content()
            .expect("the menu is open")
            .sign_out
            .node()
            .expect("the sign-out entry is built");

        harness.click_node(entry);
        harness.settle(3);

        assert!(
            !harness.menu().is_menu_open(harness.app()),
            "choosing an entry closes the menu"
        );
        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the sign-out entry asks for confirmation"
        );
        assert_eq!(
            harness.accounts().count(|state| state.discards),
            0,
            "asking is not signing out"
        );
    }

    /// A click outside the menu dismisses it, and the press that dismissed it does not also open
    /// the thing behind it.
    #[test]
    fn an_outside_click_should_close_the_menu() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);

        harness.click_at(ui_core::math::Point::new(4.0, 4.0));
        harness.settle(3);

        assert!(
            !harness.menu().is_menu_open(harness.app()),
            "a press outside the menu closes it"
        );
    }

    /* ── the dashboard handoff ───────────────────────────────────────────── */

    /// The account entry opens the dashboard with a one-time code.
    #[test]
    fn the_account_entry_should_open_the_dashboard_with_a_one_time_code() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);
        let entry = harness
            .menu()
            .content()
            .expect("the menu is open")
            .open_account
            .node()
            .expect("the account entry is built");

        harness.click_node(entry);
        harness.settle_until(40, |harness| {
            harness.accounts().count(|state| state.handoffs) > 0
        });
        harness.settle(3);

        assert_eq!(
            harness.opened_url(),
            Some(SmolStr::new(format!(
                "{ACCOUNT_ORIGIN}/dashboard#{HANDOFF_CODE}"
            ))),
            "the entry opens the dashboard with the one-time code"
        );
        assert_eq!(harness.accounts().count(|state| state.handoffs), 1);
    }

    /// A handoff that failed reports itself and opens nothing.
    #[test]
    fn a_failed_account_handoff_should_not_open_the_browser() {
        let mut harness =
            signed_in_with(|accounts| accounts.with(|state| state.handoff = Step::Fail));
        show_menu(&mut harness);
        let entry = harness
            .menu()
            .content()
            .expect("the menu is open")
            .open_account
            .node()
            .expect("the account entry is built");

        harness.click_node(entry);
        harness.settle_until(40, |harness| {
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app()))
        });
        harness.settle(2);

        assert!(harness.opened_urls().is_empty(), "nothing is opened");
        assert_eq!(harness.accounts().count(|state| state.handoffs), 1);
    }

    /* ── signing in ──────────────────────────────────────────────────────── */

    /// A sign-in that cannot even be prepared says so in the alert the wait would have used.
    #[test]
    fn a_sign_in_that_cannot_be_prepared_should_say_so_in_an_alert() {
        let mut harness =
            signed_out_with(|accounts| accounts.with(|state| state.prepare = Step::Fail));

        harness.click_trigger();
        harness.settle(3);

        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the reason is reported where the user asked"
        );
        assert!(
            !harness.menu().is_waiting_for_browser(),
            "a sign-in that was never opened has nothing to wait for"
        );
        assert!(harness.menu().account().is_none());
        assert_eq!(harness.opened_url(), None, "no browser is opened");
    }

    /// Starting a sign-in opens the browser and waits for the callback.
    #[test]
    fn starting_a_sign_in_should_open_the_browser_and_wait_for_it() {
        let mut harness = Harness::new();

        harness.start_sign_in();

        assert_eq!(harness.accounts().count(|state| state.prepared), 1);
        assert!(
            harness.menu().is_waiting_for_browser(),
            "the alert reports a wait, it does not end one"
        );
        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the wait is reported in an alert"
        );
    }

    /// Reading the waiting alert is the user's answer to the whole attempt: the alert is what
    /// reports the wait, so the wait ends where the report was read.
    #[test]
    fn reading_the_waiting_alert_should_end_the_attempt() {
        let mut harness = Harness::new();
        harness.start_sign_in();

        harness.click_alert_confirm();

        assert!(
            harness.settle_until(60, |harness| !harness.menu().is_waiting_for_browser()),
            "the attempt ends where its report was read"
        );
        harness.settle(2);
        assert!(
            !harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the alert goes with the attempt it reported"
        );
        assert!(harness.menu().account().is_none());
    }

    /// A sign-in whose alert was read must not leave the control unable to start another one: the
    /// second sign-in is a fresh attempt, not a report over an attempt still running.
    #[test]
    fn a_sign_in_should_be_offered_again_once_its_alert_was_read() {
        let mut harness = Harness::new();
        harness.start_sign_in();
        harness.click_alert_confirm();
        assert!(harness.settle_until(60, |harness| { !harness.menu().is_waiting_for_browser() }));

        harness.start_sign_in();

        assert_eq!(
            harness.accounts().count(|state| state.prepared),
            2,
            "the second sign-in has to be a new attempt, not the first one still running"
        );
        assert!(harness.menu().is_waiting_for_browser());
        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the attempt the second sign-in started reports itself in its own alert"
        );
    }

    /// An authorized sign-in stores the account and reports it.
    #[test]
    fn an_authorized_sign_in_should_store_the_account_and_report_it() {
        let mut harness = Harness::new();
        harness.start_sign_in();

        harness.finish_sign_in(Some(Ok(ethereum_account())));

        assert_eq!(
            harness.settings().read(harness.app()).account.as_ref(),
            Some(&ethereum_account())
        );
        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the account that came back is reported"
        );
        assert!(
            !harness.menu().is_waiting_for_browser(),
            "the alert that reported the account is the end of the wait"
        );
    }

    /// A rejected sign-in is reported where the user is watching, and stores nothing.
    #[test]
    fn a_rejected_sign_in_should_be_reported_where_the_user_is_watching() {
        let mut harness = Harness::new();
        harness.start_sign_in();

        harness.finish_sign_in(Some(Err(LoginError::Expired)));

        assert!(harness.menu().account().is_none());
        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the reason replaces the alert the user was watching"
        );
        assert!(!harness.menu().is_waiting_for_browser());
    }

    /// The failure report is closed by its one button.
    #[test]
    fn a_failure_report_should_be_closed_by_its_button() {
        let mut harness = Harness::new();
        harness.start_sign_in();
        harness.finish_sign_in(Some(Err(LoginError::Expired)));

        harness.click_alert_confirm();
        harness.settle(3);

        assert!(
            !harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "reading the report closes it"
        );
    }

    /* ── signing out ─────────────────────────────────────────────────────── */

    /// Signing out asks first, and declining keeps the account and the session.
    #[test]
    fn signing_out_should_ask_first_and_only_then_forget_the_account() {
        let mut harness = Harness::signed_in();

        harness.confirm_sign_out();

        assert!(
            harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "the sign-out question is asked in an alert"
        );
        assert_eq!(harness.accounts().count(|state| state.discards), 0);

        harness.click_alert_cancel();
        harness.settle(3);

        assert!(
            !harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "declining closes the question"
        );
        assert_eq!(
            harness.settings().read(harness.app()).account.as_ref(),
            Some(&ethereum_account())
        );
        assert_eq!(harness.accounts().count(|state| state.discards), 0);
        assert_eq!(harness.accounts().count(|state| state.revocations), 0);
    }

    /// Confirming a sign-out ends the session, forgets the account, and tells the service.
    #[test]
    fn confirming_a_sign_out_should_end_the_session_and_forget_the_account() {
        let mut harness = Harness::signed_in();
        let report = harness.watch_sign_out_report();
        harness.confirm_sign_out();

        harness.click_alert_confirm();
        harness.settle(3);

        assert_eq!(harness.accounts().count(|state| state.discards), 1);
        assert!(harness.settings().read(harness.app()).account.is_none());
        let _ = Harness::wait_for_sign_out_report(report);
        assert_eq!(
            harness.accounts().count(|state| state.revocations),
            1,
            "the service is told about the sign-out it did not have to allow"
        );
    }

    /// Reporting the sign-out is a request to the account service, and the frame thread must not
    /// wait for one: the report is driven on a tokio runtime.
    #[test]
    fn a_sign_out_should_report_it_on_the_application_runtime() {
        let mut harness = Harness::signed_in();
        let report = harness.watch_sign_out_report();

        harness.sign_out();
        let driven = harness.settle_until(80, |_harness| {
            // The report is made from the spawned task; a turn of the application runtime is
            // what runs it.
            true
        });
        assert!(driven);
        for _ in 0..80 {
            if let Ok(driven) = report.try_recv() {
                assert!(
                    driven,
                    "the report has to be driven by the application runtime"
                );

                return;
            }
            harness.drain();
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the sign-out report was never made");
    }

    /// A sign-out with nothing to report forgets the account without telling the service.
    #[test]
    fn a_confirmed_sign_out_should_forget_the_account_without_the_service() {
        let mut harness = signed_in_with(|accounts| accounts.with(|state| state.revocation = None));

        harness.sign_out();
        harness.settle(3);

        assert_eq!(harness.accounts().count(|state| state.discards), 1);
        assert!(harness.settings().read(harness.app()).account.is_none());
        assert_eq!(harness.accounts().count(|state| state.revocations), 0);
        assert!(
            !harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "a sign-out reports nothing: there is no failure left for it to report"
        );
        assert_eq!(
            hint_text(&harness),
            t_smol!(en(), "window.account.sign_in"),
            "the signed-out control offers a sign-in again"
        );
    }

    /// Signing out ends a sign-in that is still waiting: a callback that arrived afterwards would
    /// sign the user back into the account they just left.
    #[test]
    fn signing_out_should_end_a_sign_in_that_is_still_waiting() {
        let mut harness = Harness::signed_in();
        harness.start_sign_in();
        assert!(harness.menu().is_waiting_for_browser());

        harness.sign_out();

        assert!(harness.settle_until(60, |harness| { !harness.menu().is_waiting_for_browser() }));
        harness.settle(3);
        assert!(harness.settings().read(harness.app()).account.is_none());
        assert!(
            !harness
                .menu()
                .alert()
                .is_some_and(|alert| alert.is_open(harness.app())),
            "a sign-out the user asked for is not a failed sign-in to report"
        );
    }

    /* ── the account session ─────────────────────────────────────────────── */

    /// A credential the account session says is no longer valid forgets the account.
    #[test]
    fn a_credential_that_is_no_longer_valid_should_forget_the_account() {
        let mut harness = signed_in_with(|accounts| {
            accounts.with(|state| state.local = Some(SessionAvailability::Unavailable));
        });

        assert!(
            harness.settle_until(40, |harness| harness
                .settings()
                .read(harness.app())
                .account
                .is_none()),
            "an invalidated credential forgets the account"
        );
    }

    /// A credential query that failed keeps the account: a failed read is not a sign-out.
    #[test]
    fn a_credential_query_that_fails_should_keep_the_account() {
        let mut harness = signed_in_with(|accounts| accounts.with(|state| state.local = None));

        harness.settle(5);

        assert_eq!(
            harness.settings().read(harness.app()).account.as_ref(),
            Some(&ethereum_account())
        );
    }

    /* ── the subscription tier ───────────────────────────────────────────── */

    /// The tier of the signed-in account is read and kept.
    #[test]
    fn the_tier_of_the_signed_in_account_should_be_read_and_kept() {
        let mut harness = Harness::signed_in();

        assert!(
            harness.settle_until(40, |harness| harness.menu().plan() == Some(PlanCode::Plus)),
            "the tier is read for the signed-in account"
        );
    }

    /// Switching accounts reads the tier of the new one.
    #[test]
    fn switching_accounts_should_read_the_tier_of_the_new_one() {
        let mut harness = Harness::signed_in();
        assert!(harness.settle_until(40, |harness| harness.menu().plan() == Some(PlanCode::Plus)));

        harness
            .accounts()
            .with(|state| state.plan = Some(Some(PlanCode::Free)));
        harness.store_account(other_account());

        assert!(
            harness.settle_until(40, |harness| harness.menu().plan() == Some(PlanCode::Free)),
            "the new account's tier is the one shown"
        );
        assert_eq!(
            harness
                .menu()
                .account()
                .map(|account| account.wallet_address.as_str()),
            Some(SOLANA)
        );
    }

    /// A tier that could not be read leaves the menu saying it is unknown.
    #[test]
    fn a_tier_that_could_not_be_read_should_leave_the_menu_saying_it_is_unknown() {
        let mut harness =
            signed_in_with(|accounts| accounts.with(|state| state.plan_step = Step::Fail));

        assert!(harness.settle_until(40, |harness| { harness.menu().plan().is_none() }));
        assert_eq!(
            harness.menu().plan_label(harness.app()),
            Some(t_smol!(en(), "window.account.plan_unavailable"))
        );
    }

    /// An answer to a tier query that is no longer current is dropped: a query that outlived the
    /// account it was about would otherwise label the next account.
    #[test]
    fn a_stale_tier_answer_should_not_label_the_next_account() {
        let mut harness = Harness::signed_in();
        assert!(harness.settle_until(40, |harness| harness.menu().plan() == Some(PlanCode::Plus)));
        let generation = harness
            .menu()
            .plan_generation()
            .expect("the control is built");

        // A query generation the control has moved past.
        harness.menu().deliver(MenuEvent::Plan {
            generation: generation.wrapping_sub(1),
            user_id: SmolStr::new(ethereum_account().user_id),
            result: Ok(Some(PlanCode::Pro)),
        });
        harness.settle(2);
        assert_eq!(
            harness.menu().plan(),
            Some(PlanCode::Plus),
            "an answer from a superseded query is not applied"
        );

        // The current generation, but for an account that is not the signed-in one.
        harness.menu().deliver(MenuEvent::Plan {
            generation,
            user_id: SmolStr::new(other_account().user_id),
            result: Ok(Some(PlanCode::Pro)),
        });
        harness.settle(2);
        assert_eq!(
            harness.menu().plan(),
            Some(PlanCode::Plus),
            "an answer about another account is not applied"
        );
    }

    /* ── the avatar ──────────────────────────────────────────────────────── */

    /// A stored avatar is rasterized for the control, and the generated face it provides fills
    /// the trigger at the larger size the account's own picture takes.
    #[test]
    fn a_stored_avatar_should_be_drawn_at_the_accounts_size() {
        let snapshot = AccountSnapshot {
            account: Some(ethereum_account()),
            avatar: Some(SmolStr::new(AVATAR_SVG)),
        };
        let mut harness = Harness::with(
            snapshot,
            crate::test_support::FakeAccountService::new(ethereum_account()),
        );

        assert!(
            harness.settle_until(200, |harness| harness.menu().face().is_some()),
            "the stored avatar is rasterized"
        );
        harness.settle(3);

        assert_eq!(
            bounds(&harness, "TEST_PROFILE_TRIGGER_AVATAR").size,
            Size::new(24.0, 24.0),
            "the account's own picture is drawn larger than the placeholder"
        );
        let face = harness
            .find("TEST_PROFILE_TRIGGER_AVATAR")
            .expect("the trigger draws a face");
        let tree = harness
            .app()
            .window(harness.window())
            .expect("the harness window")
            .tree();
        let child = tree
            .get(face)
            .expect("the face box exists")
            .children()
            .first()
            .copied()
            .expect("the face box holds the picture");
        assert!(
            matches!(
                tree.get(child).expect("the picture exists").kind(),
                NodeKind::Image(_)
            ),
            "the face is an engine image node"
        );
    }

    /// A signed-in account without a stored avatar gets one generated for it, and the settings
    /// keep what was generated.
    #[test]
    fn a_generated_avatar_should_be_stored_for_the_signed_in_account() {
        let mut harness = Harness::signed_in();

        assert!(
            harness.settle_until(200, |harness| harness
                .settings()
                .read(harness.app())
                .avatar
                .is_some()),
            "a face is generated for the signed-in wallet"
        );
        let stored = harness
            .settings()
            .read(harness.app())
            .avatar
            .expect("the generated face is stored");
        assert!(
            stored.starts_with("<svg"),
            "what is stored is the generated SVG"
        );
    }

    /// A different avatar is a different face: the control rasterizes the new one and rebuilds
    /// what draws it.
    #[test]
    fn the_face_should_be_rebuilt_when_the_avatar_changes() {
        let snapshot = AccountSnapshot {
            account: Some(ethereum_account()),
            avatar: Some(SmolStr::new(AVATAR_SVG)),
        };
        let mut harness = Harness::with(
            snapshot,
            crate::test_support::FakeAccountService::new(ethereum_account()),
        );
        assert!(harness.settle_until(200, |harness| harness.menu().face().is_some()));
        let first = harness
            .menu()
            .face()
            .map(|(_, generation)| generation)
            .expect("the first face is ready");

        let other_svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#00ff00"/></svg>"##;
        assert!(harness.store_avatar(ETHEREUM, other_svg));
        assert!(
            harness.settle_until(200, |harness| harness
                .menu()
                .face()
                .is_some_and(|(svg, _)| svg.as_str() == other_svg)),
            "the new avatar is rasterized"
        );
        let second = harness
            .menu()
            .face()
            .map(|(_, generation)| generation)
            .expect("the second face is ready");
        assert!(
            second > first,
            "the control tells the new face from the old one"
        );
    }

    /* ── language, frames, instances, release ────────────────────────────── */

    /// The menu speaks the language the application shows: a language change rebuilds the block
    /// whose sentences are build-time values.
    #[test]
    fn the_menu_should_speak_the_language_showing_now() {
        let mut harness = Harness::signed_in();
        show_menu(&mut harness);

        harness.switch_language(LanguageCode::ZhCn);
        harness.settle(4);

        assert!(
            harness.menu().is_menu_open(harness.app()),
            "a language change does not close the menu"
        );
        assert!(
            drawn(&harness, "TEST_PROFILE_MENU")
                .contains(t_smol!(zh(), "window.account.open_account").as_str()),
            "the menu's copy follows the language: {}",
            drawn(&harness, "TEST_PROFILE_MENU")
        );
    }

    /// A control with nothing to say asks for no frame: the steady state is zero frames (I3).
    #[test]
    fn a_steady_control_should_ask_for_no_frame() {
        let mut harness = Harness::signed_in();
        harness.settle(4);
        let requests = harness.frame_requests();

        harness.settle(4);

        assert_eq!(
            harness.frame_requests(),
            requests,
            "a control whose state did not move asks for no frame"
        );
    }

    /// Two controls in one application do not collide: different keys, different messages, and
    /// releasing one leaves the other working.
    #[test]
    fn two_controls_should_live_side_by_side() {
        let mut harness = Harness::signed_in();
        let (mut second, second_slot) = harness.mount_second();
        harness.settle(3);

        let first_trigger = harness.menu().trigger_node().expect("the first trigger");
        let second_trigger = second.trigger_node().expect("the second trigger");
        assert_ne!(first_trigger, second_trigger);
        let keys: Vec<SmolStr> = harness
            .debug_ids()
            .into_iter()
            .filter(|id| id.contains(":trigger"))
            .filter_map(|id| id.split_once(":trigger").map(|(key, _)| SmolStr::new(key)))
            .collect();
        assert_eq!(
            keys.len(),
            2,
            "each control carries its own key prefix: {keys:?}"
        );
        assert_ne!(keys[0], keys[1], "two controls do not share a key prefix");

        let window = harness.window();
        let parent = harness.parent();
        harness
            .build_control(&mut second, window, parent)
            .expect("a second build is a no-op");
        let second_trigger_node = second.trigger_node().expect("the second trigger");
        assert_eq!(second_trigger_node, second_trigger);

        harness
            .remove_node(second_slot)
            .expect("the caller removes the control's node first");
        harness.release_control(&mut second);
        harness.settle(3);

        assert_eq!(
            harness.menu().trigger_node(),
            Some(first_trigger),
            "releasing one control leaves the other's trigger"
        );
        assert!(harness.menu().account().is_some());
    }

    /// Releasing a control gives back everything it built: its nodes and its value slots.
    #[test]
    fn releasing_the_control_should_reclaim_what_it_built() {
        let mut harness = Harness::signed_in();
        // The first control's generated face lands asynchronously; the baseline is taken once it
        // has, so the measurement is about the second control and nothing else.
        assert!(harness.settle_until(200, |harness| harness.menu().face().is_some()));
        harness.settle(2);
        let baseline_tree = harness.tree_len();
        let baseline_slots = harness.signal_slots();

        let (mut second, second_slot) = harness.mount_second();
        harness.settle(2);
        assert!(
            harness.tree_len() > baseline_tree,
            "the second control drew nodes"
        );
        assert!(
            harness.signal_slots().live > baseline_slots.live,
            "the second control minted values"
        );

        harness
            .remove_node(second_slot)
            .expect("the caller removes the control's node first");
        harness.release_control(&mut second);

        assert_eq!(
            harness.tree_len(),
            baseline_tree,
            "the released control leaves no nodes behind"
        );
        assert_eq!(
            harness.signal_slots().live,
            baseline_slots.live,
            "the released control gives its value slots back"
        );
    }

    /// Releasing a control twice is harmless (the stages are idempotent).
    #[test]
    fn releasing_the_control_twice_should_be_harmless() {
        let mut harness = Harness::signed_in();
        let (mut second, second_slot) = harness.mount_second();
        harness
            .remove_node(second_slot)
            .expect("the caller removes the control's node first");

        harness.release_control(&mut second);
        harness.release_control(&mut second);

        assert!(second.trigger_node().is_none());
        assert!(!second.is_menu_open(harness.app()));
    }

    /* ── copy ────────────────────────────────────────────────────────────── */

    /// Every key this feature reads has copy, in both languages. A missing key translates to an
    /// empty string, which is how this feature would fail quietly: a menu entry with no label, an
    /// alert that says nothing, a button that cannot be read.
    #[test]
    fn every_key_this_feature_reads_should_resolve_to_text() {
        let keys = [
            "window.account.authorization_complete",
            "window.account.authorization_complete_message",
            "window.account.awaiting_authorization",
            "window.account.cancel_sign_in",
            "window.account.copy_wallet_address",
            "window.account.open_account",
            "window.account.plan_unavailable",
            "window.account.sign_in",
            "window.account.sign_out",
            "window.account.sign_out_confirm",
            "common.cancel",
            "common.confirm",
            "error.account.credential_store",
            "error.account.invalid_credential",
            "error.account.invalid_grant",
            "error.account.login_callback",
            "error.account.login_expired",
            "error.account.login_listener",
            "error.account.missing_credential",
            "error.account.refresh_unknown",
            "error.account.request",
            "error.account.response",
            "error.account.route",
            "error.account.secure_random",
            "error.account.sequence_exhausted",
            "error.account.session_closed",
            "error.account.status",
            "error.account.system_time",
        ];
        for translate in [en(), zh()] {
            for key in keys {
                assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
            }
            // Every failure the account service can report has to say something. The variant that
            // takes a payload this crate cannot build (`Status` carries a `reqwest` status code)
            // is covered by the key list above; the rest carry an `io` / `rand` / `url` error.
            for error in [
                kytrix_service_provider::auth::model::AuthError::CredentialStore,
                kytrix_service_provider::auth::model::AuthError::MissingCredential,
                kytrix_service_provider::auth::model::AuthError::InvalidCredential,
                kytrix_service_provider::auth::model::AuthError::HttpClient,
                kytrix_service_provider::auth::model::AuthError::Route,
                kytrix_service_provider::auth::model::AuthError::Request,
                kytrix_service_provider::auth::model::AuthError::RefreshOutcomeUnknown,
                kytrix_service_provider::auth::model::AuthError::InvalidResponse,
                kytrix_service_provider::auth::model::AuthError::InvalidGrant,
                kytrix_service_provider::auth::model::AuthError::SystemTime,
                kytrix_service_provider::auth::model::AuthError::SecureRandom,
                kytrix_service_provider::auth::model::AuthError::SessionClosed,
                kytrix_service_provider::auth::model::AuthError::SequenceExhausted,
            ] {
                assert!(
                    !AccountFailure::Service(&error)
                        .translated_message(&translate)
                        .is_empty(),
                    "{error:?} has no copy"
                );
            }
            for error in [LoginError::Expired, LoginError::InvalidCallback] {
                assert!(
                    !error.translated_message(&translate).is_empty(),
                    "{error:?} has no copy"
                );
            }
        }
    }

    /// The account a sign-in just authorized is reported in the middle of its alert, which is
    /// what the message says about itself with a `<p align="center">` tag pair.
    #[test]
    fn the_authorization_report_should_be_centred_copy_in_both_languages() {
        const KEY: &str = "window.account.authorization_complete_message";

        let wallet = SmolStr::new_static("the-wallet");
        let family = SmolStr::new_static("the-family");
        for translate in [en(), zh()] {
            let filling = t_smol!(translate, KEY, wallet = wallet, kind = family);

            assert!(
                filling.contains("the-family") && filling.contains("the-wallet"),
                "the report writes the account where its placeholders are: {filling}"
            );
            assert!(
                !filling.contains("%{"),
                "the report has no unread placeholder: {filling}"
            );
        }
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate(language);
            let message = t_smol!(
                translate,
                KEY,
                wallet = SmolStr::new(ETHEREUM),
                kind = crate::account::wallet_kind_label(
                    app_config::settings::AccountWalletKind::Ethereum
                )
            );
            assert!(
                message.starts_with("<p align=\"center\">") && message.ends_with("</p>"),
                "the report of {language:?} should be centred copy: {message}"
            );
        }
    }

    /* ── what an app shell reads ─────────────────────────────────────────── */

    /// The trigger's shape records what it was built from, so a rebuild happens exactly when one
    /// of those values moved.
    #[test]
    fn the_trigger_s_identity_is_what_it_was_built_from() {
        let shape = TriggerShape {
            signed_in: true,
            language: LanguageCode::En,
            face: None,
        };
        assert_eq!(
            shape,
            TriggerShape {
                signed_in: true,
                language: LanguageCode::En,
                face: None,
            }
        );
        assert_ne!(
            shape,
            TriggerShape {
                signed_in: false,
                language: LanguageCode::En,
                face: None,
            }
        );
        assert_ne!(
            shape,
            TriggerShape {
                signed_in: true,
                language: LanguageCode::ZhCn,
                face: None,
            }
        );
        assert_ne!(
            shape,
            TriggerShape {
                signed_in: true,
                language: LanguageCode::En,
                face: Some(1),
            }
        );
    }

    /// The control is only a face and no menu until the caller mounts it: `new` starts nothing.
    #[test]
    fn a_control_that_was_never_built_draws_nothing() {
        let harness = Harness::new();
        let menu = ProfileMenu::new(harness.host());

        assert!(!menu.is_menu_open(harness.app()));
        assert_eq!(menu.trigger_node(), None);
        assert_eq!(menu.copied_address(harness.app()), None);
        assert_eq!(menu.account(), None);
        assert_eq!(menu.plan(), None);
        assert!(!menu.is_waiting_for_browser());
    }

    /// `build` is idempotent: the first call owns the nodes, a second one changes nothing.
    #[test]
    fn building_a_control_twice_should_change_nothing() {
        let mut harness = Harness::signed_in();
        let trigger = harness.menu().trigger_node().expect("the trigger is built");
        let window = harness.window();
        let parent = harness.parent();

        harness
            .rebuild_control(window, parent)
            .expect("a second build is a no-op");

        assert_eq!(harness.menu().trigger_node(), Some(trigger));
    }

    /// The framework's release order (E16): the caller removes the control's node first, then
    /// gives the control back — which is what makes a teardown that closed the window first
    /// harmless.
    #[test]
    fn releasing_after_the_caller_removed_the_control_should_be_harmless() {
        let mut harness = Harness::signed_in();
        let parent = harness.parent();
        harness
            .remove_node(parent)
            .expect("the caller removes the control's slot");

        harness.release_menu();

        assert!(harness.menu().trigger_node().is_none());
        assert!(!harness.menu().is_menu_open(harness.app()));
    }

    /// The crate only exposes what a shell needs: the control's public read points are the ones
    /// it mounts and reads.
    #[test]
    fn the_shell_s_public_read_points_answer() {
        let harness = Harness::signed_in();
        let menu = harness.menu();

        assert!(menu.account().is_some());
        assert!(!menu.is_menu_open(harness.app()));
        assert_eq!(menu.copied_address(harness.app()), None);
        assert!(menu.trigger_node().is_some());
    }

    /// The alert's title names the question a sign-out asks, and its declining entry says cancel:
    /// the two answers are the whole decision.
    #[test]
    fn the_sign_out_question_should_offer_both_answers() {
        let mut harness = Harness::signed_in();
        harness.confirm_sign_out();
        let alert = harness.menu().alert().expect("the question is up");

        assert_eq!(
            alert.title(harness.app()),
            t_smol!(en(), "window.account.sign_out")
        );
        assert!(
            harness.menu().alert_cancel().is_some(),
            "the question offers the way of saying no"
        );
    }
}
