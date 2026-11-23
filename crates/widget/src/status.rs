//! The state of one widget instance, and its single point of publication.
//!
//! A widget reports what it is doing, what it has to say while it has nothing to
//! show, and what went wrong; it never decides where any of that appears, and it
//! never assembles a sentence for it. A report is one value of [`WidgetState`], so
//! no two of those can be true at once and a write replaces the state whole.
//!
//! The state lives in an engine [`Signal`]: a write goes through
//! [`WidgetStatus::report`], which is the deduplication rule
//! (`App::write` short-circuits an equal value) and the invalidation rule (a signal
//! write marks every node that read the signal in its `bind_*` closure, which is
//! what requests the frame that redraws the presentation).
//!
//! # Nothing here is language-dependent
//!
//! What a widget hands over is a reference to the catalogue ([`Notice`]) or a typed
//! error ([`Failure`]); the sentence is resolved by the surface, on the frame that
//! draws it, through the [`Translate`] context that frame was handed. A language
//! change is therefore not something a widget has to report: the composition root
//! switches the translator and writes the language signal, and the surface — which
//! reads both — redraws the sentences it draws.
//!
//! # Deduplication
//!
//! Writing a state equal to the current one neither stores it nor notifies. This is
//! a hard requirement rather than an optimization: a streaming widget publishes on
//! every snapshot it receives, and without deduplication each of those writes would
//! schedule a frame. The comparison is the state's own `PartialEq`, and there is no
//! second one. Unlike the version this module replaces, the rule is implemented
//! by the engine's [`App::write`] (equal value ⇒ `false`), so there is no separate
//! write path to keep in step.

use std::{fmt, rc::Rc, sync::Arc};

use i18n::{t_smol, translate::Translate};
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    node::signal::{Signal, SignalScope},
};

use crate::{error::TranslatedError, venue};

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
    pub fn failed(error: Arc<dyn TranslatedError>, action: WidgetAction) -> Self {
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
///
/// The catalogue does not mark which values are plain sentences and which are
/// fragments of markdown/HTML: the product authors some of a widget's own messages
/// as aligned paragraphs (`widget.news.logged_out` is one), and
/// [`common.market_not_loaded`](Self::MarketNotLoaded) as a paragraph with a
/// `<kbd>` icon and a markdown line per venue. [`Notice::is_markdown`] answers the
/// question from the resolved value, so a message is drawn richly in one language
/// and as a sentence in another exactly as the catalogue authors it.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// One message of the catalogue that takes no arguments.
    ///
    /// The value may be a plain sentence or rich text; the surface asks
    /// [`Notice::is_markdown`] rather than assuming either.
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

    /// Whether [`resolve`](Self::resolve) answers rich text rather than plain text.
    ///
    /// The catalogue has no markup flag, so the question is asked of the resolved
    /// value: a notice is rich when the value the catalogue holds in `translate`
    /// carries an HTML tag, a markdown link/image, strong emphasis or inline code.
    /// That covers both shapes the product ships — `common.market_not_loaded`'s
    /// aligned paragraph with a `<kbd>` icon and one markdown line per venue, and a
    /// widget's own message such as `widget.news.logged_out`, whose value is a
    /// `<p align="center">` paragraph. A plain sentence answers `false` and stays a
    /// sentence.
    ///
    /// The question is asked where the notice is drawn, in the language showing
    /// then, because the same key can be rich in one language and plain in another.
    /// See [`is_rich_text`] for the markers.
    pub fn is_markdown(&self, translate: &Translate) -> bool {
        is_rich_text(&self.resolve(translate))
    }

    /// Resolves the reference into the sentence the catalogue holds in the language
    /// showing now.
    pub fn resolve(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Message(key) => t_smol!(translate, *key),
            Self::MarketNotLoaded { supported, .. } => {
                venue::market_not_loaded_sentence(translate, supported)
            }
        }
    }
}

/// Whether a resolved catalogue value is authored as rich text.
///
/// The product's copy uses four kinds of marker, and a value carrying any of them
/// is a fragment of markdown/HTML rather than one sentence:
///
/// - an HTML tag (`<p align="center">`, `</p>`, `<kbd>`), which is how the aligned
///   paragraphs and the keyboard icons are written;
/// - a markdown link or image (`[text](target)`, `![](file://…)`);
/// - strong emphasis (`**bold**`);
/// - inline code (`` `code` ``).
///
/// Everything else — every ordinary sentence of the catalogue — answers `false` and
/// is drawn by the empty state. The test deliberately works on the resolved text
/// rather than on a key list: which values are rich is the catalogue's business, and
/// a key list would have to be kept in step with the locales.
fn is_rich_text(source: &str) -> bool {
    has_html_tag(source) || source.contains("](") || source.contains("**") || source.contains('`')
}

/// Whether the text contains an HTML tag: `<name …>` or `</name>`.
///
/// A `<` is a tag when it is followed by an ASCII letter (an element name) or by
/// `/` and an ASCII letter (a closing tag), and a `>` follows somewhere after it.
/// That keeps a sentence that happens to compare numbers (`3 < 5`) plain text.
fn has_html_tag(source: &str) -> bool {
    source.match_indices('<').any(|(index, _)| {
        let rest = &source[index + 1..];
        let mut characters = rest.chars();
        let named = match characters.next() {
            Some('/') => characters.next().is_some_and(|c| c.is_ascii_alphabetic()),
            Some(c) => c.is_ascii_alphabetic(),
            None => false,
        };
        named && rest.contains('>')
    })
}

/// The state cell of one widget instance: the single point of reading and writing.
///
/// It is a handle over a scoped engine [`Signal`], not an entity: `Copy`, no
/// borrow, no notification obligation. The framework mints it when the widget is
/// constructed and hands it to both the widget and the surface that presents it,
/// so the two cannot disagree about which cell is the truth.
///
/// The signal belongs to the widget's own [`SignalScope`], which the framework
/// releases after the surface's nodes are removed — see [`App::remove_node_with_scope`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetStatus(Signal<WidgetState>);

impl WidgetStatus {
    /// Creates the state cell of a new instance in `scope`: someone asked for this
    /// widget, so the first thing it does is establish its connection.
    pub fn new(app: &mut App, scope: SignalScope) -> Self {
        Self(app.signal_scoped(scope, WidgetState::Connecting))
    }

    /// Returns the state the widget published.
    pub fn state(&self, app: &App) -> WidgetState {
        app.read_with(&self.0, |state| state.clone())
    }

    /// Moves to `next`. A state equal to the current one is neither stored nor
    /// notified, and the return value says which happened.
    pub fn report(&self, app: &mut App, next: WidgetState) -> bool {
        app.write(&self.0, next)
    }

    /// Returns whether the widget is in a failure.
    ///
    /// Every message entry point asks this before applying what arrived: a failure
    /// is terminal, and nothing but the user's recovery leaves it.
    pub fn is_failed(&self, app: &App) -> bool {
        app.read_with(&self.0, WidgetState::is_failed)
    }

    /// Returns whether the widget is working.
    pub fn is_busy(&self, app: &App) -> bool {
        app.read_with(&self.0, WidgetState::is_busy)
    }

    /// The signal itself, for the surface's `bind_*` closures: reading it there is
    /// what registers the dependency that a report invalidates.
    pub const fn signal(&self) -> Signal<WidgetState> {
        self.0
    }
}

/// A failure a widget reports, and the recovery it offers for it.
///
/// The text is resolved where it is drawn rather than where it is reported, so a
/// failure follows a language change like every other sentence of the interface.
#[derive(Clone)]
pub struct Failure {
    error: Arc<dyn TranslatedError>,
    action: WidgetAction,
}

impl Failure {
    /// The failure's detail, in the language `translate` renders.
    ///
    /// It is shown exactly as the error type produces it: neither of the two places
    /// a failure can appear adds a prefix or any other wrapper, so the same failure
    /// reads the same whether it lands in a banner or in a placeholder.
    pub fn text(&self, translate: &Translate) -> SmolStr {
        self.error.translated_message(translate)
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
/// rule the state's own `PartialEq` applies. A widget that wants a repeated report
/// of the same failure to cost nothing holds on to the handle it reported (most of
/// them keep it in their own failure enum already); reporting it through a new
/// handle costs one frame, and a failure is terminal with every message entry point
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
            // The text needs a translation context to resolve, and a `Debug` render
            // has none.
            .field("error", &"<typed error>")
            .finish_non_exhaustive()
    }
}

/// The control a widget offers for recovering from a failure.
///
/// The label is a catalogue key rather than a resolved sentence: the control is
/// drawn by the framework on every frame of a failure, and the language it reads in
/// is the one showing then. The recovery runs against the application context,
/// because that is what the framework has when a click arrives.
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

    /// The control's label, in the language `translate` renders.
    pub fn label(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, self.label_key)
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
    pub fn run(&self, app: &mut App) {
        (self.run)(app);
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
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use i18n::{language::codes::LanguageCode, translate::Translate, translator::types::I18n};
    use smol_str::SmolStr;

    use super::{Failure, Notice, WidgetAction, WidgetState, WidgetStatus};
    use crate::error::TranslatedError;

    const MESSAGES_EN: &str = r#"
    common:
      retry: Retry
      resume: Continue
    sample:
      failure: Sample failure
    test:
      waiting: Waiting for orders
      rich: '<p align="center">Click the <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> button.</p>'
    "#;

    const MESSAGES_ZH: &str = r#"
    common:
      retry: 重试
      resume: 继续
    sample:
      failure: 示例失败
    test:
      waiting: 正在等待订单
      rich: '<p align="center">点击 <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> 按钮</p>'
    "#;

    /// A typed error whose text is resolved in the language the context renders.
    struct SampleError;

    impl TranslatedError for SampleError {
        fn translated_message(&self, translate: &Translate) -> SmolStr {
            i18n::t_smol!(translate, "sample.failure")
        }
    }

    /// The fixture catalogue in both languages.
    fn catalogue() -> I18n {
        I18n::builder()
            .locale_yaml(LanguageCode::En, MESSAGES_EN)
            .expect("test locale should parse")
            .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
            .expect("test locale should parse")
            .fallback(LanguageCode::En)
            .build()
            .expect("test translator should build")
    }

    fn translate() -> Translate {
        Translate::new(Rc::new(catalogue()), LanguageCode::En)
    }

    /// A headless engine application: the state cell lives in its signal store, so
    /// every read and write in this module goes through the real `App`.
    fn app() -> ui_core::app::App {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        ui_core::app::App::new(runtime.handle().clone())
    }

    /// The state cell of a new widget.
    fn status(app: &mut ui_core::app::App) -> WidgetStatus {
        let scope = app.signal_scope();
        WidgetStatus::new(app, scope)
    }

    /// The default recovery: the key every widget of the workspace uses for it.
    fn retry() -> WidgetAction {
        WidgetAction::new("common.retry", |_| {})
    }

    /// A failure of the fixture error type.
    fn failed() -> WidgetState {
        WidgetState::failed(Arc::new(SampleError), retry())
    }

    /// A widget is created because someone asked for it, so the first state it reports is
    /// that it is establishing the connection its work depends on.
    #[test]
    fn a_new_status_should_start_connecting() {
        let mut app = app();
        let status = status(&mut app);

        assert_eq!(status.state(&app), WidgetState::Connecting);
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

    /// A report of exactly what the cell holds is no change: it neither stores nor
    /// is a write at all. Two reports of the same words, and two reports of the same
    /// failure handle, are the two halves of that. The write's own return value is the
    /// engine's equality short-circuit; whether a frame follows depends on a node
    /// having read the signal, which is the surface's business (surface tests).
    #[test]
    fn repeating_a_state_should_not_be_a_write() {
        let mut app = app();
        let status = status(&mut app);
        let error = Arc::new(SampleError);
        let action = retry();

        let notice = || WidgetState::waiting(Notice::message("test.waiting"));
        assert!(status.report(&mut app, notice()), "the first report writes");
        assert!(
            !status.report(&mut app, notice()),
            "the same notice is no write"
        );
        assert!(status.report(&mut app, WidgetState::failed(error.clone(), action.clone())));
        assert!(
            !status.report(&mut app, WidgetState::failed(error, action)),
            "the same failure handle and control are no write"
        );
    }

    /// The other half of the rule, pinned so it cannot drift: a failure reported
    /// through a new handle is a new state, because the failure's equality is the
    /// identity of the handle it carries.
    #[test]
    fn a_failure_through_a_new_handle_should_be_a_change() {
        let mut app = app();
        let status = status(&mut app);
        let action = retry();

        assert!(status.report(
            &mut app,
            WidgetState::failed(Arc::new(SampleError), action.clone())
        ));
        assert!(
            status.report(&mut app, WidgetState::failed(Arc::new(SampleError), action)),
            "the same failure behind another handle is another state"
        );
    }

    #[test]
    fn each_distinct_transition_should_be_reported_once() {
        let mut app = app();
        let status = status(&mut app);
        let failure = {
            let failure = failed();
            assert!(status.report(&mut app, failure.clone()));
            failure
        };

        assert!(status.report(
            &mut app,
            WidgetState::waiting(Notice::message("test.waiting"))
        ));
        assert_eq!(
            status.state(&app),
            WidgetState::waiting(Notice::message("test.waiting"))
        );
        assert!(
            status.report(&mut app, failure),
            "and the failure is a third, because it is not what the cell holds"
        );
        assert!(status.is_failed(&app));
    }

    /// The last thing a widget does before it fails is often to report that it is
    /// loading; the state is replaced whole, so nothing of the earlier one survives.
    #[test]
    fn a_state_should_replace_the_one_before_it_whole() {
        let mut app = app();
        let status = status(&mut app);

        assert!(status.report(
            &mut app,
            WidgetState::waiting(Notice::message("test.waiting"))
        ));
        assert!(status.report(
            &mut app,
            WidgetState::failed(Arc::new(SampleError), retry())
        ));
        assert!(status.is_failed(&app));
        assert!(
            status.state(&app).notice().is_none(),
            "a failure does not carry the copy the widget waited with"
        );

        assert!(status.report(
            &mut app,
            WidgetState::waiting(Notice::message("test.waiting"))
        ));
        assert!(!status.is_failed(&app), "a neutral notice is not a failure");
        assert_eq!(
            status.state(&app).notice(),
            Some(&Notice::message("test.waiting"))
        );
    }

    /// A catalogue reference is published whole: what is compared is the key and its
    /// arguments, and what the surface resolves is the sentence they name.
    #[test]
    fn a_notice_should_publish_the_key_it_was_given_and_resolve_into_its_sentence() {
        let notice = Notice::message("test.waiting");
        let translate = translate();

        assert_eq!(notice.resolve(&translate).as_str(), "Waiting for orders");

        let zh = translate;
        assert!(zh.set_language(LanguageCode::ZhCn));
        assert_eq!(
            notice.resolve(&zh).as_str(),
            "正在等待订单",
            "the same reference resolves in the language showing now"
        );
    }

    /// The rich/plain question is asked of the resolved value, not of the variant
    /// a widget reported: one `Message` can be a sentence and another can be the
    /// catalogue's aligned paragraph with a `<kbd>` icon, and the surface has to
    /// know which before it decides how to draw it.
    #[test]
    fn a_notice_is_rich_exactly_when_its_catalogue_value_carries_markup() {
        let translate = translate();

        assert!(
            !Notice::message("test.waiting").is_markdown(&translate),
            "a plain sentence is drawn by the empty state"
        );
        assert!(
            Notice::message("test.rich").is_markdown(&translate),
            "a message whose value carries a tag is rich text"
        );
    }

    /// A failure's detail is resolved where it is read rather than where it was
    /// written, so it follows a language change with every other sentence.
    #[test]
    fn a_failure_should_resolve_its_text_in_the_language_showing_now() {
        let translate = translate();
        let failure = Failure {
            error: Arc::new(SampleError),
            action: retry(),
        };

        assert_eq!(failure.text(&translate).as_str(), "Sample failure");

        let zh = translate;
        assert!(zh.set_language(LanguageCode::ZhCn));
        assert_eq!(
            failure.text(&zh).as_str(),
            "示例失败",
            "the detail is read in the language showing now"
        );
    }

    /// The control a failure offers is read the same way: its label key is resolved
    /// where the button is drawn.
    #[test]
    fn a_failure_should_carry_the_action_it_was_given() {
        let translate = translate();
        let action = retry();
        let failure = Failure {
            error: Arc::new(SampleError),
            action: action.clone(),
        };

        assert_eq!(failure.action(), &action);
        assert_eq!(failure.action().label_key(), "common.retry");
        assert_eq!(failure.action().label(&translate).as_str(), "Retry");

        let zh = translate;
        assert!(zh.set_language(LanguageCode::ZhCn));
        assert_eq!(
            failure.action().label(&zh).as_str(),
            "重试",
            "the label is read in the language showing now"
        );
    }

    #[test]
    fn a_recovery_should_run_the_action_the_widget_published() {
        let mut app = app();
        let retries = Rc::new(Cell::new(0));
        let counted = retries.clone();
        let action = WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1));

        assert!(action.is_enabled());
        action.run(&mut app);

        assert_eq!(retries.get(), 1);
    }

    #[test]
    fn a_recovery_can_refuse_input_while_the_widget_is_busy() {
        let action = WidgetAction::new("common.retry", |_| {}).enabled(false);

        assert!(!action.is_enabled());
    }

    /// The control a failure carries is part of the state, and what it reads is part
    /// of the control: a failure whose control was relabelled is another failure, and
    /// so is the same control with input refused. One error handle is behind all
    /// three states, so none of the differences here can hide behind a difference in
    /// identity — and the first assertion is the other half of the rule: the same
    /// handle and the same control are no change at all.
    #[test]
    fn a_relabelled_or_refused_recovery_should_be_a_change() {
        let translate = translate();
        let error = Arc::new(SampleError);
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
                .label(&translate)
        };
        assert_ne!(
            label(&repeated),
            label(&relabelled),
            "and the two controls really do read differently"
        );
    }

    /// The two exit conditions of the equality rule on its own, without a cell: the
    /// same handles are one failure, and the same action behind another closure is
    /// not.
    #[test]
    fn a_failure_should_be_one_failure_only_through_the_handles_it_carries() {
        let action = retry();
        let first = Failure {
            error: Arc::new(SampleError),
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
