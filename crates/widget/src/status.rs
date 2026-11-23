use std::{fmt, rc::Rc, sync::Arc};

use i18n::{t_smol, translate::Translate};
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    node::signal::{Signal, SignalScope},
};

use crate::{error::TranslatedError, venue};

#[derive(Clone, Debug, PartialEq)]
pub enum WidgetState {
    Connecting,
    Loading,
    Ready,
    Waiting(Notice),
    Failed(Failure),
}

impl WidgetState {
    pub const fn waiting(notice: Notice) -> Self {
        Self::Waiting(notice)
    }

    pub fn failed(error: Arc<dyn TranslatedError>, action: WidgetAction) -> Self {
        Self::Failed(Failure { error, action })
    }

    pub const fn is_busy(&self) -> bool {
        matches!(self, Self::Connecting | Self::Loading)
    }

    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    pub const fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Failed(failure) => Some(failure),
            _ => None,
        }
    }

    pub const fn notice(&self) -> Option<&Notice> {
        match self {
            Self::Waiting(notice) => Some(notice),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    Message(&'static str),
    MarketNotLoaded {
        venues: Vec<VenueId>,
        supported: Vec<VenueId>,
    },
}

impl Notice {
    pub const fn message(key: &'static str) -> Self {
        Self::Message(key)
    }

    pub fn is_markdown(&self, translate: &Translate) -> bool {
        is_rich_text(&self.resolve(translate))
    }

    pub fn resolve(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Message(key) => t_smol!(translate, *key),
            Self::MarketNotLoaded { supported, .. } => {
                venue::market_not_loaded_sentence(translate, supported)
            }
        }
    }
}

fn is_rich_text(source: &str) -> bool {
    has_html_tag(source) || source.contains("](") || source.contains("**") || source.contains('`')
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetStatus(Signal<WidgetState>);

impl WidgetStatus {
    pub fn new(app: &mut App, scope: SignalScope) -> Self {
        Self(app.signal_scoped(scope, WidgetState::Connecting))
    }

    pub fn state(&self, app: &App) -> WidgetState {
        app.read_with(&self.0, |state| state.clone())
    }

    pub fn report(&self, app: &mut App, next: WidgetState) -> bool {
        app.write(&self.0, next)
    }

    pub fn is_failed(&self, app: &App) -> bool {
        app.read_with(&self.0, WidgetState::is_failed)
    }

    pub fn is_busy(&self, app: &App) -> bool {
        app.read_with(&self.0, WidgetState::is_busy)
    }

    pub const fn signal(&self) -> Signal<WidgetState> {
        self.0
    }
}

#[derive(Clone)]
pub struct Failure {
    error: Arc<dyn TranslatedError>,
    action: WidgetAction,
}

impl Failure {
    pub fn text(&self, translate: &Translate) -> SmolStr {
        self.error.translated_message(translate)
    }

    pub const fn action(&self) -> &WidgetAction {
        &self.action
    }
}

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
            .field("error", &"<typed error>")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct WidgetAction {
    label_key: &'static str,
    run: Rc<dyn Fn(&mut App) + 'static>,
    enabled: bool,
}

impl WidgetAction {
    pub fn new(label_key: &'static str, run: impl Fn(&mut App) + 'static) -> Self {
        Self {
            label_key,
            run: Rc::new(run),
            enabled: true,
        }
    }

    pub fn label(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, self.label_key)
    }

    pub const fn label_key(&self) -> &'static str {
        self.label_key
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn run(&self, app: &mut App) {
        (self.run)(app);
    }

    pub(crate) fn run_handle(&self) -> Rc<dyn Fn(&mut App) + 'static> {
        self.run.clone()
    }
}

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

    struct SampleError;

    impl TranslatedError for SampleError {
        fn translated_message(&self, translate: &Translate) -> SmolStr {
            i18n::t_smol!(translate, "sample.failure")
        }
    }

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

    fn app() -> ui_core::app::App {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        ui_core::app::App::new(runtime.handle().clone())
    }

    fn status(app: &mut ui_core::app::App) -> WidgetStatus {
        let scope = app.signal_scope();
        WidgetStatus::new(app, scope)
    }

    fn retry() -> WidgetAction {
        WidgetAction::new("common.retry", |_| {})
    }

    fn failed() -> WidgetState {
        WidgetState::failed(Arc::new(SampleError), retry())
    }

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
