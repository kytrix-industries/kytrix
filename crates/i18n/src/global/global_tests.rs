use std::{cell::Cell, rc::Rc};

use gpui_kit::{App, TestAppContext};

use crate::{language::codes::LanguageCode, t, translator::types::I18n};

const MESSAGES: &str =
    "menu:\n  market: Market\ngreeting: \"Hello, %{name}!\"\ninbox: \"%{count} messages\"";

/// Installs a catalogue and the language it is rendered in, which is what
/// [`embedded::install`](crate::global::embedded::install) does in the
/// application.
fn install(cx: &mut App) {
    let i18n = I18n::builder()
        .locale_yaml(LanguageCode::En, MESSAGES)
        .expect("valid locale")
        .build()
        .expect("the locale is loaded");

    cx.set_global(i18n);
    cx.set_global(LanguageCode::En);
}

fn install_bilingual(cx: &mut App) {
    let i18n = I18n::builder()
        .locale_yaml(LanguageCode::En, "menu:\n  market: Market")
        .expect("valid English locale")
        .locale_yaml(LanguageCode::ZhCn, "menu:\n  market: 行情")
        .expect("valid Chinese locale")
        .fallback(LanguageCode::En)
        .build()
        .expect("both locales are loaded");

    cx.set_global(i18n);
    cx.set_global(LanguageCode::En);
}

#[test]
fn installed_translator_answers_the_macro() {
    let cx = TestAppContext::single();

    cx.update(|cx| {
        install(cx);

        assert_eq!(t!(cx, "menu.market").as_ref(), "Market");
    });
}

#[test]
fn macro_accepts_explicit_and_implicit_arguments() {
    let cx = TestAppContext::single();

    cx.update(|cx| {
        install(cx);

        let name = "Ada";
        assert_eq!(t!(cx, "greeting", name = name).as_ref(), "Hello, Ada!");
        assert_eq!(t!(cx, "greeting", name).as_ref(), "Hello, Ada!");
    });
}

#[test]
fn macro_reports_a_missing_message_as_empty_text() {
    let cx = TestAppContext::single();

    cx.update(|cx| {
        install(cx);

        assert_eq!(t!(cx, "never.defined").as_ref(), "");
    });
}

#[test]
fn macro_follows_the_installed_language() {
    let cx = TestAppContext::single();

    cx.update(|cx| {
        install_bilingual(cx);
        assert_eq!(t!(cx, "menu.market").as_ref(), "Market");

        cx.set_global(LanguageCode::ZhCn);
        assert_eq!(t!(cx, "menu.market").as_ref(), "行情");
    });
}

#[test]
fn language_code_is_a_global() {
    let cx = TestAppContext::single();

    cx.update(|cx| {
        install(cx);
        assert_eq!(*cx.global::<LanguageCode>(), LanguageCode::En);

        cx.set_global(LanguageCode::ZhCn);
        assert_eq!(*cx.global::<LanguageCode>(), LanguageCode::ZhCn);
    });
}

/// Pins the mechanism the interface switches languages through: a component that
/// holds text read in a language it has left is only able to follow the change if
/// setting the global notifies its observer.
#[test]
fn observe_global_is_notified_when_the_language_changes() {
    let cx = TestAppContext::single();
    let calls = Rc::new(Cell::new(0));

    cx.update(install);

    // Registered in an update of its own, because a global set in the same update
    // is still pending when the observer arrives and would notify it.
    let calls_in_observer = calls.clone();
    let _subscription = cx.update(|cx| {
        cx.observe_global::<LanguageCode>(move |_| {
            calls_in_observer.set(calls_in_observer.get() + 1);
        })
    });

    cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

    assert_eq!(
        calls.get(),
        1,
        "switching the language should notify the observer exactly once"
    );
}
