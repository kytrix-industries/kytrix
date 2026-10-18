//! The state of one widget instance, and its single point of publication.
//!
//! A widget reports what it is doing, what it has to say while it has nothing to
//! show, and what went wrong; it never decides where any of that appears, and it
//! never assembles a sentence for it. A report is one value of [`WidgetState`], so
//! no two of those can be true at once and a write replaces the state whole.
//!
//! [`WidgetStatus`] **is not a view**: it does not implement `Render`, and it is
//! only ever read by [`WidgetSurface`](crate::surface::WidgetSurface), so an
//! `Entity<WidgetStatus>` cannot be placed in an element tree.
//!
//! # Nothing here is language-dependent
//!
//! What a widget hands over is a reference to the catalogue ([`Notice`]) or a typed
//! error ([`Failure`]); the sentence is resolved by the surface, on the frame that
//! draws it, in the language showing then. A language change is therefore not
//! something a widget has to report: the application switches the language with
//! `App::set_global` followed by `App::refresh_windows`, and that frame re-resolves
//! every sentence. The framework registers no observer of the language, and a
//! widget neither.
//!
//! # Deduplication
//!
//! Writing a state equal to the current one neither stores it nor notifies. This is
//! a hard requirement rather than an optimization: a streaming widget publishes on
//! every snapshot it receives, and without deduplication each of those writes would
//! schedule a frame. The comparison is the state's own `PartialEq`, and there is no
//! second one.

use std::{fmt, rc::Rc, sync::Arc};

use gpui_kit::{App, AppContext, Context, Entity, SharedString};
use i18n::t;
use market_participants_model::identifier::VenueId;

use crate::{error::LocalizedError, venue};

/// What a widget instance reports. Mutually exclusive situations, as a sum type.
#[derive(Clone, Debug, PartialEq)]
pub enum WidgetState {
    /// Establishing the connection the widget's work depends on.
    Connecting,
    /// Fetching or refreshing data over an established connection.
    Loading,
    /// Not working, and with nothing to say.
    Ready,
    /// Not working, and with a reference to the copy that stands in for content.
    Waiting(Notice),
    /// Failed, with the control that recovers from it.
    Failed(Failure),
}

impl WidgetState {
    /// The state of a widget that has nothing to show and something to say about it.
    pub const fn waiting(notice: Notice) -> Self {
        Self::Waiting(notice)
    }

    /// The state of a widget that failed.
    ///
    /// The error is handed over as a shared handle: a widget that reports the same
    /// failure again through the same handle reports no change at all.
    pub fn failed(error: Arc<dyn LocalizedError>, action: WidgetAction) -> Self {
        Self::Failed(Failure { error, action })
    }

    /// Returns whether the widget is working.
    ///
    /// This is the only answer to "busy or idle" in the framework: the state itself
    /// says it, so a separate progress field cannot disagree with the state.
    pub const fn is_busy(&self) -> bool {
        matches!(self, Self::Connecting | Self::Loading)
    }

    /// Returns whether the widget is in a failure.
    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// Returns the failure, with the recovery it offers.
    pub const fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Failed(failure) => Some(failure),
            _ => None,
        }
    }

    /// Returns the catalogue reference a widget with nothing to show reports.
    pub const fn notice(&self) -> Option<&Notice> {
        match self {
            Self::Waiting(notice) => Some(notice),
            _ => None,
        }
    }
}

/// What a widget with no content of its own says in place of it.
///
/// It is a **reference into the catalogue**, not a sentence: the key and its
/// arguments are here, and the surface resolves them on the frame that draws them,
/// in the language showing then. That is why no language is stored with the state,
/// and why nothing has to be re-reported when the language moves.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// One message of the catalogue that takes no arguments.
    Message(&'static str),
    /// `common.market_not_loaded`: this widget needs the metadata of these
    /// markets loaded first.
    ///
    /// `venues` is the set the reporting widget waits for, and it is the whole of
    /// what the state's identity is built from: two reports that wait for the same
    /// markets are one state, so a pane that keeps waiting reports nothing again.
    /// The sentence does not read it — it names the venues that are supported, not
    /// the ones that are missing — so this field is the report's identity alone.
    ///
    /// `supported` is every venue the widget can read at all, which is what the
    /// sentence lists, and it is the widget's own answer, because which markets a
    /// widget may read is a fact about its capability rather than about the markets
    /// that happen to be loaded. The sentence's two arguments are supplied by
    /// [`crate::venue`]: `markets` is those venues' names, one per line, and
    /// `control` is the key of the control the sentence points at. Both lists are
    /// built where the state is reported rather than where it is drawn, which is why
    /// a `Vec` is the right shape for them.
    MarketNotLoaded {
        venues: Vec<VenueId>,
        supported: Vec<VenueId>,
    },
}

impl Notice {
    /// A message of the catalogue that takes no arguments.
    pub const fn message(key: &'static str) -> Self {
        Self::Message(key)
    }

    /// Resolves the reference into the sentence the catalogue holds in the language
    /// showing now.
    pub fn resolve(&self, cx: &App) -> SharedString {
        match self {
            Self::Message(key) => t!(cx, *key),
            Self::MarketNotLoaded { supported, .. } => {
                venue::market_not_loaded_sentence(cx, supported)
            }
        }
    }
}

/// The state of a widget instance: its single point of writing, of deduplication,
/// and of notification.
///
/// It **is not a view**: it does not implement `Render` and is only read by
/// [`WidgetSurface`](crate::surface::WidgetSurface), so an `Entity<WidgetStatus>`
/// cannot be placed in an element tree.
pub struct WidgetStatus {
    state: WidgetState,
}

impl WidgetStatus {
    /// The state of a new instance: someone asked for this widget, so the first
    /// thing it does is establish its connection.
    pub const fn new() -> Self {
        Self {
            state: WidgetState::Connecting,
        }
    }

    /// Returns the state the widget published.
    pub const fn state(&self) -> &WidgetState {
        &self.state
    }

    /// Moves to `next`. A state equal to the current one is neither stored nor
    /// notified.
    pub fn set(&mut self, next: WidgetState, cx: &mut Context<Self>) {
        if self.state == next {
            return;
        }

        self.state = next;
        cx.notify();
    }
}

impl Default for WidgetStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// The four ways in and out of a widget's state.
///
/// `Entity` is a type of another crate, so an inherent impl on
/// `Entity<WidgetStatus>` is rejected (E0116) whatever its type argument is. The
/// entry points are therefore an extension trait, exactly as `VenueDisplay` and
/// `ThemeExt` are in this workspace; a file that reports state has to
/// `use widget::status::WidgetStatusExt;`, or the call is E0599.
pub trait WidgetStatusExt {
    /// Reports a state. A state equal to the current one does not notify.
    fn report<C: AppContext>(&self, state: WidgetState, cx: &mut C);

    /// Returns whether the widget is in a failure.
    ///
    /// Every message entry point asks this before applying what arrived: a failure
    /// is terminal, and nothing but the user's recovery leaves it.
    fn is_failed<C: AppContext>(&self, cx: &C) -> bool;

    /// Returns whether the widget is working.
    fn is_busy<C: AppContext>(&self, cx: &C) -> bool;

    /// Returns a snapshot of the state.
    fn state<C: AppContext>(&self, cx: &C) -> WidgetState;
}

impl WidgetStatusExt for Entity<WidgetStatus> {
    fn report<C: AppContext>(&self, state: WidgetState, cx: &mut C) {
        self.update(cx, |status, cx| status.set(state, cx));
    }

    fn is_failed<C: AppContext>(&self, cx: &C) -> bool {
        self.read_with(cx, |status, _| status.state().is_failed())
    }

    fn is_busy<C: AppContext>(&self, cx: &C) -> bool {
        self.read_with(cx, |status, _| status.state().is_busy())
    }

    fn state<C: AppContext>(&self, cx: &C) -> WidgetState {
        self.read_with(cx, |status, _| status.state().clone())
    }
}

/// A failure a widget reports, and the recovery it offers for it.
///
/// The text is resolved where it is drawn rather than where it is reported, so a
/// failure follows a language change like every other sentence of the interface.
#[derive(Clone)]
pub struct Failure {
    error: Arc<dyn LocalizedError>,
    action: WidgetAction,
}

impl Failure {
    /// The failure's detail, in the language showing now.
    ///
    /// It is shown exactly as the error type produces it: neither of the two places
    /// a failure can appear adds a prefix or any other wrapper, so the same failure
    /// reads the same whether it lands in a banner or in a placeholder.
    pub fn text(&self, cx: &App) -> SharedString {
        self.error.localized_message(cx)
    }

    /// The control that recovers from this failure.
    pub const fn action(&self) -> &WidgetAction {
        &self.action
    }
}

/// Two failures are the same when they carry the same error handle and the same
/// recovery.
///
/// This is the only rule by which a failure counts as a change, because it is the
/// rule [`WidgetStatus::set`] applies. A widget that wants a repeated report of the
/// same failure to cost nothing holds on to the handle it reported (most of them
/// keep it in their own failure enum already); reporting it through a new handle
/// costs one frame, and a failure is terminal with every message entry point
/// guarding on `is_failed`.
impl PartialEq for Failure {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.error, &other.error) && self.action == other.action
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Failure")
            .field("action", &self.action)
            // The text needs an `App` to resolve, and a `Debug` render has none.
            .field("error", &"<typed error>")
            .finish_non_exhaustive()
    }
}

/// The control a widget offers for recovering from a failure.
///
/// The label is a catalogue key rather than a resolved sentence: the control is
/// drawn by the framework on every frame of a failure, and the language it reads in
/// is the one showing then.
#[derive(Clone)]
pub struct WidgetAction {
    label_key: &'static str,
    run: Rc<dyn Fn(&mut App) + 'static>,
    enabled: bool,
}

impl WidgetAction {
    /// Creates an action that accepts input, labelled by `label_key`.
    pub fn new(label_key: &'static str, run: impl Fn(&mut App) + 'static) -> Self {
        Self {
            label_key,
            run: Rc::new(run),
            enabled: true,
        }
    }

    /// The control's label, in the language showing now.
    pub fn label(&self, cx: &App) -> SharedString {
        t!(cx, self.label_key)
    }

    /// The catalogue key the label is read from.
    pub const fn label_key(&self) -> &'static str {
        self.label_key
    }

    /// Sets whether the control accepts input.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Returns whether the control accepts input.
    ///
    /// A control that does not is given no click handler at all: `disabled` only
    /// draws that state, it does not stop a click.
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Runs the recovery.
    pub fn run(&self, cx: &mut App) {
        (self.run)(cx);
    }

    /// The recovery itself, as a handle the presentation can move into a click
    /// handler.
    ///
    /// A handler outlives the frame that built it, so it cannot borrow the action;
    /// this is the `pub(crate)` way to hand the handle over without exposing the
    /// field.
    pub(crate) fn run_handle(&self) -> Rc<dyn Fn(&mut App) + 'static> {
        self.run.clone()
    }
}

/// Two actions are one action when they would read and behave the same: the same
/// label key, the same enabled state, and the same recovery behind them.
impl PartialEq for WidgetAction {
    fn eq(&self, other: &Self) -> bool {
        self.label_key == other.label_key
            && self.enabled == other.enabled
            && Rc::ptr_eq(&self.run, &other.run)
    }
}

impl fmt::Debug for WidgetAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WidgetAction")
            .field("label_key", &self.label_key)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui_kit::{
        App, AppContext, Context, Entity, InteractiveElement, IntoElement, Render, Subscription,
        TestAppContext, Window, div,
    };
    use i18n::{language::codes::LanguageCode, translator::types::I18n};

    use super::{
        Failure, LocalizedError, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt,
    };
    use crate::status::Notice;

    /// The messages the fixture translator is built from.
    ///
    /// `test.waiting` is the copy a widget of these tests reports as its own, and
    /// `sample.failure` is the detail of the fixture error type.
    const MESSAGES_EN: &str = r#"
    common:
      retry: Retry
      resume: Continue
    sample:
      failure: Sample failure
    test:
      waiting: Waiting for orders
    "#;

    const MESSAGES_ZH: &str = r#"
    common:
      retry: 重试
      resume: 继续
    sample:
      failure: 示例失败
    test:
      waiting: 正在等待订单
    "#;

    /// A typed error whose text is resolved in the language showing now.
    struct SampleError;

    impl LocalizedError for SampleError {
        fn localized_message(&self, cx: &App) -> gpui_kit::SharedString {
            i18n::t!(cx, "sample.failure")
        }
    }

    /// Installs the fixture catalogue in both languages, so a test can move between
    /// them.
    fn install_both(cx: &mut App) {
        let i18n = I18n::builder()
            .locale_yaml(LanguageCode::En, MESSAGES_EN)
            .expect("test locale should parse")
            .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
            .expect("test locale should parse")
            .language(LanguageCode::En)
            .fallback(LanguageCode::En)
            .build()
            .expect("test translator should build");

        cx.set_global(i18n);
        cx.set_global(LanguageCode::En);
    }

    /// The state cell of a new widget, over the fixture catalogue.
    fn status(cx: &mut App) -> Entity<WidgetStatus> {
        install_both(cx);

        cx.new(|_| WidgetStatus::new())
    }

    /// The default recovery: the key every widget of the workspace uses for it.
    fn retry() -> WidgetAction {
        WidgetAction::new("common.retry", |_| {})
    }

    /// A failure of the fixture error type.
    fn failed() -> WidgetState {
        WidgetState::failed(std::sync::Arc::new(SampleError), retry())
    }

    /// Counts the notifications one pane receives from a status it reads.
    struct PaneProbe {
        notifications: Rc<Cell<usize>>,
        _observation: Subscription,
    }

    impl Render for PaneProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().debug_selector(|| "STATUS_PANE_PROBE".into())
        }
    }

    /// Reads `cell` from a pane-shaped view, so a write's notification is observable
    /// from the outside.
    ///
    /// The probe is a view a window hosts, which is the shape a pane has and what keeps
    /// it alive for as long as the test runs; the delivery does not depend on that. GPUI
    /// notifies the observers of an entity whether or not a window is displaying it — with
    /// none displaying it, `App::notify` queues its `Effect::Notify` itself and the flush
    /// runs every observer, and one displaying it only decides that the window is dirty as
    /// well. What every write here does need is an update of its own, because GPUI
    /// coalesces the notifications that pile up inside one.
    fn observe(cx: &mut TestAppContext, cell: &Entity<WidgetStatus>) -> Rc<Cell<usize>> {
        let notifications = Rc::new(Cell::new(0));
        let (probe, _) = cx.add_window_view({
            let notifications = notifications.clone();
            let observed = cell.clone();
            move |_, cx| PaneProbe {
                notifications: notifications.clone(),
                _observation: cx.observe(&observed, |probe: &mut PaneProbe, _, _| {
                    probe.notifications.set(probe.notifications.get() + 1);
                }),
            }
        });
        probe.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        notifications
    }

    /// A widget is created because someone asked for it, so the first state it reports is
    /// that it is establishing the connection its work depends on.
    #[test]
    fn a_new_status_should_start_connecting() {
        let status = WidgetStatus::new();

        assert_eq!(status.state(), &WidgetState::Connecting);
    }

    #[test]
    fn only_a_busy_state_should_report_the_widget_as_busy() {
        for busy in [WidgetState::Connecting, WidgetState::Loading] {
            assert!(busy.is_busy(), "{busy:?} is work in progress");
        }
        for idle in [
            WidgetState::Ready,
            WidgetState::waiting(Notice::message("test.waiting")),
            failed(),
        ] {
            assert!(!idle.is_busy(), "{idle:?} is not work in progress");
        }
    }

    /// A report of exactly what the cell holds is no change: it neither notifies
    /// nor is it a write at all. Two reports of the same words, and two reports of
    /// the same failure handle, are the two halves of that.
    ///
    /// Every write is made in an update of its own, because GPUI coalesces the
    /// notifications that pile up inside one.
    #[gpui_kit::test]
    fn repeating_a_state_should_not_notify_the_pane(cx: &mut TestAppContext) {
        let cell = cx.update(status);
        let notifications = observe(cx, &cell);
        let error = std::sync::Arc::new(SampleError);
        let action = retry();

        let notice = || WidgetState::waiting(Notice::message("test.waiting"));
        cx.update(|cx| cell.report(notice(), cx));
        cx.update(|cx| cell.report(notice(), cx));
        cx.update(|cx| cell.report(WidgetState::failed(error.clone(), action.clone()), cx));
        cx.update(|cx| cell.report(WidgetState::failed(error, action), cx));

        assert_eq!(
            notifications.get(),
            2,
            "the repeated message and the repeated failure handle are no write, and the \
             two states themselves are"
        );
    }

    /// The other half of the rule, pinned so it cannot drift: a failure reported
    /// through a new handle is a new state, because the failure's equality is the
    /// identity of the handle it carries.
    ///
    /// The control is the same one both times, so the one thing the state can tell the
    /// two reports apart by is the error handle itself.
    #[gpui_kit::test]
    fn a_failure_through_a_new_handle_should_notify_the_pane(cx: &mut TestAppContext) {
        let cell = cx.update(status);
        let notifications = observe(cx, &cell);
        let action = retry();

        cx.update(|cx| {
            cell.report(
                WidgetState::failed(std::sync::Arc::new(SampleError), action.clone()),
                cx,
            );
        });
        cx.update(|cx| {
            cell.report(
                WidgetState::failed(std::sync::Arc::new(SampleError), action),
                cx,
            );
        });

        assert_eq!(
            notifications.get(),
            2,
            "the same failure behind another handle is another state"
        );
    }

    #[gpui_kit::test]
    fn each_distinct_transition_should_notify_the_pane_once(cx: &mut TestAppContext) {
        let cell = cx.update(status);
        let notifications = observe(cx, &cell);
        let failure = cx.update(|cx| {
            let failure = failed();
            cell.report(failure.clone(), cx);

            failure
        });

        cx.update(|cx| cell.report(WidgetState::waiting(Notice::message("test.waiting")), cx));

        assert_eq!(
            notifications.get(),
            2,
            "two distinct transitions, two frames"
        );
        cx.update(|cx| {
            let state = cell.state(cx);
            assert_eq!(state, WidgetState::waiting(Notice::message("test.waiting")));
        });

        cx.update(|cx| cell.report(failure.clone(), cx));

        assert_eq!(
            notifications.get(),
            3,
            "and the failure is a third, because it is not what the cell holds"
        );
        cx.update(|cx| assert!(cell.is_failed(cx)));
    }

    /// The last thing a widget does before it fails is often to report that it is
    /// loading; the state is replaced whole, so nothing of the earlier one survives.
    #[test]
    fn a_state_should_replace_the_one_before_it_whole() {
        let cx = TestAppContext::single();
        let cell = cx.update(status);

        cx.update(|cx| {
            cell.report(WidgetState::waiting(Notice::message("test.waiting")), cx);
            cell.report(
                WidgetState::failed(std::sync::Arc::new(SampleError), retry()),
                cx,
            );
            assert!(cell.is_failed(cx));
            assert!(
                cell.state(cx).notice().is_none(),
                "a failure does not carry the copy the widget waited with"
            );

            cell.report(WidgetState::waiting(Notice::message("test.waiting")), cx);
            assert!(!cell.is_failed(cx), "a neutral notice is not a failure");
            assert_eq!(
                cell.state(cx).notice(),
                Some(&Notice::message("test.waiting"))
            );

            cell.report(WidgetState::Connecting, cx);
            assert!(
                cell.state(cx).notice().is_none(),
                "working again clears the copy the widget waited with"
            );
            assert!(cell.state(cx).is_busy());
        });
    }

    /// A catalogue reference is published whole: what is compared is the key and its
    /// arguments, and what the surface resolves is the sentence they name.
    #[test]
    fn a_notice_should_publish_the_key_it_was_given_and_resolve_into_its_sentence() {
        let cx = TestAppContext::single();
        let cell = cx.update(status);

        cx.update(|cx| {
            cell.report(WidgetState::waiting(Notice::message("test.waiting")), cx);
            assert_eq!(
                cell.state(cx).notice(),
                Some(&Notice::message("test.waiting"))
            );

            let notice = Notice::message("test.waiting");
            assert_eq!(notice.resolve(cx).as_ref(), "Waiting for orders");

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                notice.resolve(cx).as_ref(),
                "正在等待订单",
                "the same reference resolves in the language showing now"
            );
        });
    }

    /// A failure's detail is resolved where it is read rather than where it was
    /// written, so it follows a language change with every other sentence.
    #[test]
    fn a_failure_should_resolve_its_text_in_the_language_showing_now() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            let cell = status(cx);
            cell.report(
                WidgetState::failed(std::sync::Arc::new(SampleError), retry()),
                cx,
            );

            let failure = cell
                .state(cx)
                .failure()
                .cloned()
                .expect("a failure should be published");
            assert_eq!(failure.text(cx).as_ref(), "Sample failure");

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                failure.text(cx).as_ref(),
                "示例失败",
                "the detail is read in the language showing now"
            );
        });
    }

    /// The control a failure offers is read the same way: its label key is resolved
    /// where the button is drawn.
    #[test]
    fn a_failure_should_carry_the_action_it_was_given() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            let cell = status(cx);
            let action = retry();
            cell.report(
                WidgetState::failed(std::sync::Arc::new(SampleError), action.clone()),
                cx,
            );

            let failure = cell
                .state(cx)
                .failure()
                .cloned()
                .expect("a failure should be published");
            assert_eq!(failure.action(), &action);
            assert_eq!(failure.action().label_key(), "common.retry");
            assert_eq!(failure.action().label(cx).as_ref(), "Retry");

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                failure.action().label(cx).as_ref(),
                "重试",
                "the label is read in the language showing now"
            );
        });
    }

    #[test]
    fn a_recovery_should_run_the_action_the_widget_published() {
        let cx = TestAppContext::single();
        let retries = Rc::new(Cell::new(0));

        cx.update(|cx| {
            let cell = status(cx);
            let counted = retries.clone();
            cell.report(
                WidgetState::failed(
                    std::sync::Arc::new(SampleError),
                    WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1)),
                ),
                cx,
            );

            let action = cell
                .state(cx)
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failure should offer its recovery");
            assert!(action.is_enabled());
            action.run(cx);

            assert_eq!(retries.get(), 1);
        });
    }

    #[test]
    fn a_recovery_can_refuse_input_while_the_widget_is_busy() {
        let action = WidgetAction::new("common.retry", |_| {}).enabled(false);

        assert!(!action.is_enabled());
    }

    /// The control a failure carries is part of the state, and what it reads is part of
    /// the control: a failure whose control was relabelled is another failure, and so is
    /// the same control with input refused.
    ///
    /// One error handle is behind all three states, so none of the differences here can
    /// hide behind a difference in identity — and the first assertion is the other half
    /// of the rule: the same handle and the same control are no change at all.
    #[test]
    fn a_relabelled_or_refused_recovery_should_be_a_change() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            install_both(cx);

            let error = std::sync::Arc::new(SampleError);
            let action = retry();
            let repeated = WidgetState::failed(error.clone(), action.clone());
            let relabelled =
                WidgetState::failed(error.clone(), WidgetAction::new("common.resume", |_| {}));
            let refused = WidgetState::failed(error.clone(), action.clone().enabled(false));

            assert_eq!(
                repeated,
                WidgetState::failed(error, action),
                "the same error handle and the same control are the same state"
            );
            assert_ne!(
                repeated, relabelled,
                "a control that reads differently is another state"
            );
            assert_ne!(
                repeated, refused,
                "the same control with input refused is another state"
            );

            let label = |state: &WidgetState| {
                state
                    .failure()
                    .expect("every state of this test is a failure")
                    .action()
                    .label(cx)
            };
            assert_ne!(
                label(&repeated),
                label(&relabelled),
                "and the two controls really do read differently"
            );
        });
    }

    /// The two exit conditions of the equality rule on its own, without a cell:
    /// the same handles are one failure, and the same action behind another closure
    /// is not.
    #[test]
    fn a_failure_should_be_one_failure_only_through_the_handles_it_carries() {
        let action = retry();
        let first = Failure {
            error: std::sync::Arc::new(SampleError),
            action,
        };
        let same = Failure {
            error: first.error.clone(),
            action: first.action.clone(),
        };
        let other_action = Failure {
            error: first.error.clone(),
            action: WidgetAction::new("common.retry", |_| {}),
        };

        assert_eq!(first, same);
        assert_ne!(first, other_action, "another closure is another control");
        assert!(
            format!("{first:?}").contains("<typed error>"),
            "a failure's debug render names the action and never resolves the error: {first:?}"
        );
    }
}
