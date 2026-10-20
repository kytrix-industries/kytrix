use gpui_kit::SharedString;

use super::{builder::Builder, types::I18n};
use crate::{error::types::Error, language::codes::LanguageCode};

const EN: &str = r#"
menu:
  market: Market
greeting: "Hello, %{name}!"
items: "You have %{count} items in %{name}."
price: "%{amount} USD"
credential: The stored sign-in credential is invalid
blank: ""
"#;
const ZH_CN: &str = r#"
menu:
  market: 行情
greeting: "你好，%{name}！"
price: "%{amount} 美元"
"#;

/// The text of `credential`, which is longer than the 23 bytes a `SmolStr` stores
/// inline. Above that capacity the text is heap-backed, so the address of the
/// returned string is comparable from one call to the next.
const LONG_MESSAGE: &str = "The stored sign-in credential is invalid";

fn i18n() -> I18n {
    Builder::default()
        .locale_yaml(LanguageCode::En, EN)
        .expect("valid English locale")
        .locale_yaml(LanguageCode::ZhCn, ZH_CN)
        .expect("valid Chinese locale")
        .fallback(LanguageCode::En)
        .build()
        .expect("both locales are loaded")
}

#[test]
fn translates_in_the_language_it_is_given() {
    let i18n = i18n();
    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.market").as_ref(),
        "Market"
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "menu.market").as_ref(),
        "行情"
    );
}

#[test]
fn interpolates_arguments() {
    let i18n = i18n();
    let name = "Ada";

    assert_eq!(
        i18n.translate_with(LanguageCode::En, "greeting", &[("name", &name)])
            .as_ref(),
        "Hello, Ada!"
    );
}

#[test]
fn interpolation_uses_the_template_of_the_language_it_is_given() {
    let i18n = i18n();
    let name = "Ada";

    assert_eq!(
        i18n.translate_with(LanguageCode::ZhCn, "greeting", &[("name", &name)])
            .as_ref(),
        "你好，Ada！"
    );
}

#[test]
fn repeated_translation_reuses_the_rendered_message() {
    let i18n = i18n();
    let count = 3;
    let name = "Cart";

    let first = i18n.translate_with(
        LanguageCode::En,
        "items",
        &[("count", &count), ("name", &name)],
    );
    let second = i18n.translate_with(
        LanguageCode::En,
        "items",
        &[("count", &count), ("name", &name)],
    );

    assert_eq!(first.as_ref(), "You have 3 items in Cart.");
    assert!(std::ptr::eq(first.as_ref(), second.as_ref()));
}

#[test]
fn different_argument_values_get_distinct_renderings() {
    let i18n = i18n();

    assert_eq!(
        i18n.translate_with(LanguageCode::En, "price", &[("amount", &"10")])
            .as_ref(),
        "10 USD"
    );
    assert_eq!(
        i18n.translate_with(LanguageCode::En, "price", &[("amount", &"20")])
            .as_ref(),
        "20 USD"
    );
    assert_eq!(
        i18n.translate_with(LanguageCode::En, "price", &[("amount", &"10")])
            .as_ref(),
        "10 USD"
    );
}

#[test]
fn arguments_in_a_different_order_reuse_the_rendering() {
    let i18n = i18n();
    let count = 3;
    let name = "Cart";

    let first = i18n.translate_with(
        LanguageCode::En,
        "items",
        &[("count", &count), ("name", &name)],
    );
    let second = i18n.translate_with(
        LanguageCode::En,
        "items",
        &[("name", &name), ("count", &count)],
    );

    assert!(std::ptr::eq(first.as_ref(), second.as_ref()));
}

#[test]
fn unused_arguments_do_not_change_the_rendering() {
    let i18n = i18n();
    let name = "Ada";

    assert_eq!(
        i18n.translate_with(
            LanguageCode::En,
            "greeting",
            &[("name", &name), ("unused", &1)]
        )
        .as_ref(),
        "Hello, Ada!"
    );
}

#[test]
fn missing_arguments_render_as_empty_text() {
    let i18n = i18n();
    assert_eq!(
        i18n.translate(LanguageCode::En, "greeting").as_ref(),
        "Hello, !"
    );
}

#[test]
fn messages_missing_everywhere_translate_to_empty_text() {
    let i18n = i18n();

    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "menu.quant").as_ref(),
        ""
    );
    assert!(
        i18n.try_translate(LanguageCode::ZhCn, "menu.quant")
            .is_none()
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "never.defined").as_ref(),
        ""
    );
    assert!(
        i18n.try_translate(LanguageCode::ZhCn, "never.defined")
            .is_none()
    );
}

#[test]
fn messages_of_the_given_language_win_over_the_fallback() {
    let i18n = i18n();
    let name = "Ada";

    assert_eq!(
        i18n.translate_with(LanguageCode::ZhCn, "greeting", &[("name", &name)])
            .as_ref(),
        "你好，Ada！"
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "menu.market").as_ref(),
        "行情"
    );
}

#[test]
fn the_fallback_supplies_messages_the_language_lacks() {
    let i18n = Builder::default()
        .locale_yaml(LanguageCode::En, "a: en\nshared: shared")
        .expect("valid English locale")
        .locale_yaml(LanguageCode::ZhCn, "b: zh")
        .expect("valid Chinese locale")
        .fallback(LanguageCode::En)
        .build()
        .expect("both locales are loaded");

    assert_eq!(i18n.translate(LanguageCode::ZhCn, "b").as_ref(), "zh");
    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "shared").as_ref(),
        "shared"
    );
    assert_eq!(i18n.translate(LanguageCode::ZhCn, "a").as_ref(), "en");
}

#[test]
fn without_a_fallback_missing_messages_stay_empty() {
    let i18n = Builder::default()
        .locale_yaml(LanguageCode::En, "a: en\nshared: shared")
        .expect("valid English locale")
        .locale_yaml(LanguageCode::ZhCn, "b: zh")
        .expect("valid Chinese locale")
        .build()
        .expect("both locales are loaded");

    assert_eq!(i18n.translate(LanguageCode::ZhCn, "b").as_ref(), "zh");
    assert_eq!(i18n.translate(LanguageCode::ZhCn, "shared").as_ref(), "");
    assert_eq!(i18n.fallback(), None);
}

/// Pins that a long static message reaches callers as one stored value: above
/// the 23 bytes a short string keeps inline the text is heap-backed, so two calls
/// that share it return one address while two that build it separately do not.
///
/// This alone does not guard the fast path — `RenderedCache` also hands out the
/// same `Arc` — which is what
/// `tests/static_translation_allocations.rs` is for.
#[test]
fn a_static_message_longer_than_the_inline_capacity_is_handed_out_as_one_shared_value() {
    let i18n = i18n();

    let first = i18n.try_translate(LanguageCode::En, "credential");
    let second = i18n.try_translate(LanguageCode::En, "credential");

    let (Some(first), Some(second)) = (first, second) else {
        panic!("the message is defined in the English locale");
    };
    assert_eq!(first.as_ref(), LONG_MESSAGE);
    assert!(
        std::ptr::eq(first.as_ref(), second.as_ref()),
        "both translations return the text stored in the template"
    );
}

/// A message short enough for the inline buffer is copied on the way out, so its
/// address says nothing; the inline path is only observable through the text it
/// produces.
#[test]
fn a_static_message_within_the_inline_capacity_keeps_its_text() {
    let i18n = i18n();

    assert_eq!(
        i18n.try_translate(LanguageCode::En, "menu.market")
            .as_ref()
            .map(SharedString::as_str),
        Some("Market")
    );
    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.market").as_ref(),
        "Market"
    );
}

/// Pins the same for the fallback lookup, which resolves the message through a
/// different store than the requested language.
#[test]
fn the_fallback_language_is_handed_out_as_one_shared_value() {
    // `credential` is English-only, so Chinese resolves it through the fallback.
    let i18n = i18n();

    let first = i18n.try_translate(LanguageCode::ZhCn, "credential");
    let second = i18n.try_translate(LanguageCode::ZhCn, "credential");

    let (Some(first), Some(second)) = (first, second) else {
        panic!("the fallback locale defines the message");
    };
    assert_eq!(first.as_ref(), LONG_MESSAGE);
    assert!(
        std::ptr::eq(first.as_ref(), second.as_ref()),
        "both translations return the fallback text stored in the template"
    );
}

#[test]
fn an_empty_message_stays_empty_and_a_missing_key_stays_missing() {
    let i18n = i18n();

    let empty = i18n.try_translate(LanguageCode::En, "blank");
    assert_eq!(
        empty.as_ref().map(SharedString::as_str),
        Some(""),
        "a deliberately empty message is not a missing one"
    );
    assert_eq!(i18n.translate(LanguageCode::En, "blank").as_ref(), "");
    assert_eq!(i18n.try_translate(LanguageCode::En, "never.defined"), None);
    assert_eq!(
        i18n.translate(LanguageCode::En, "never.defined").as_ref(),
        ""
    );
}

/// Guards the thread-safety of the text this diff put inside [`Template`]:
/// `Template::Static` held a `Box<str>` and now holds a `SharedString`, whose
/// heap representation is an `Arc<str>` today. A representation change to
/// something `Rc`-backed would leave the translator no longer `Send + Sync`, and
/// nothing else would notice: [`gpui_kit::Global`] requires only `'static`, so
/// installing the translator in GPUI state compiles either way. Cross-thread use
/// is what would then start failing, at a distance from the cause.
#[test]
fn the_translator_stays_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<I18n>();
}

/// Guards the branch the fast path sits in: only a template without placeholders
/// returns its text, and a message with a placeholder still has to render and be
/// cached.
#[test]
fn an_interpolated_message_still_renders_through_the_cache() {
    let i18n = i18n();
    let count = 3;
    let name = "Cart";

    // Renders to "You have 3 items in Cart.", which is longer than the inline
    // capacity, so a cache hit is observable as a shared address.
    let first = i18n.translate_with(
        LanguageCode::En,
        "items",
        &[("count", &count), ("name", &name)],
    );
    let cached = i18n.translate_with(
        LanguageCode::En,
        "items",
        &[("count", &count), ("name", &name)],
    );
    assert_eq!(first.as_ref(), "You have 3 items in Cart.");
    assert!(
        std::ptr::eq(first.as_ref(), cached.as_ref()),
        "a repeated rendering should come from the cache"
    );

    assert_eq!(
        i18n.translate_with(LanguageCode::En, "greeting", &[("name", &"Grace")])
            .as_ref(),
        "Hello, Grace!"
    );
    assert_eq!(
        i18n.translate(LanguageCode::En, "greeting").as_ref(),
        "Hello, !",
        "a placeholder without an argument renders as empty text"
    );
}

#[test]
fn languages_follow_the_language_table_order() {
    let i18n = i18n();
    let expected: Vec<_> = LanguageCode::all()
        .filter(|language| i18n.has_language(*language))
        .collect();

    assert_eq!(i18n.languages().len(), 2);
    assert_eq!(i18n.languages(), expected);
}

#[test]
fn the_builder_checks_the_language_it_is_given() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::ZhCn, "a: zh")
        .expect("valid Chinese locale")
        .build()
        .expect_err("the language checked by default is English");

    match error {
        Error::UnknownLanguage { language } => assert_eq!(language, LanguageCode::En),
        other => panic!("expected an unknown language error, got {other}"),
    }

    let i18n = Builder::default()
        .locale_yaml(LanguageCode::En, "a: en")
        .expect("valid English locale")
        .locale_yaml(LanguageCode::ZhCn, "b: zh")
        .expect("valid Chinese locale")
        .language(LanguageCode::ZhCn)
        .build()
        .expect("both locales are loaded");

    assert_eq!(i18n.translate(LanguageCode::ZhCn, "b").as_ref(), "zh");
    assert_eq!(i18n.translate(LanguageCode::En, "a").as_ref(), "en");
    assert_eq!(i18n.fallback(), None);
}

#[test]
fn a_builder_without_locales_is_rejected() {
    let error = Builder::default()
        .build()
        .expect_err("an instance without a locale cannot translate anything");

    match error {
        Error::UnknownLanguage { language } => assert_eq!(language, LanguageCode::En),
        other => panic!("expected an unknown language error, got {other}"),
    }
}

#[test]
fn empty_locale_document_is_rejected() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::En, "\n")
        .expect_err("an empty locale file is rejected");

    assert!(matches!(error, Error::EmptyLocaleFile { .. }), "{error}");
}

#[test]
fn invalid_template_is_rejected_with_its_key() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::En, "greeting: \"Hello, %{name\"")
        .expect_err("an unclosed placeholder is rejected");

    match error {
        Error::InvalidTemplate { key, .. } => assert_eq!(key, "greeting"),
        other => panic!("expected an invalid template error, got {other}"),
    }
}

#[test]
fn nested_keys_are_flattened() {
    let i18n = Builder::default()
        .locale_yaml(LanguageCode::En, "a:\n  b:\n    c: deep")
        .expect("valid locale")
        .build()
        .expect("a locale is loaded");

    assert_eq!(i18n.translate(LanguageCode::En, "a.b.c").as_ref(), "deep");
}

#[test]
fn non_message_entries_are_rejected() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::En, "count: 3")
        .expect_err("numbers are not messages");

    match error {
        Error::NotAMessage { key, .. } => assert_eq!(key, "count"),
        other => panic!("expected a not-a-message error, got {other}"),
    }
}

#[test]
fn loading_one_language_twice_is_rejected() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::En, "a: one")
        .expect("valid locale")
        .locale_yaml(LanguageCode::En, "a: two")
        .expect("valid locale")
        .build()
        .expect_err("the same locale cannot be loaded twice");

    assert!(
        matches!(error, Error::LanguageLoadedTwice { .. }),
        "{error}"
    );
}

#[test]
fn selecting_a_language_without_a_locale_is_rejected() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::En, "a: one")
        .expect("valid locale")
        .language(LanguageCode::Ja)
        .build()
        .expect_err("the selected language must have a locale");

    match error {
        Error::UnknownLanguage { language } => assert_eq!(language, LanguageCode::Ja),
        other => panic!("expected an unknown language error, got {other}"),
    }
}

#[test]
fn a_fallback_without_a_locale_is_rejected() {
    let error = Builder::default()
        .locale_yaml(LanguageCode::En, "a: one")
        .expect("valid locale")
        .fallback(LanguageCode::Ja)
        .build()
        .expect_err("the fallback language must have a locale");

    match error {
        Error::UnknownLanguage { language } => assert_eq!(language, LanguageCode::Ja),
        other => panic!("expected an unknown language error, got {other}"),
    }
}

#[test]
fn a_locale_file_loads_from_disk() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/locales/en.yaml");
    let i18n = Builder::default()
        .locale_file(&path)
        .expect("the fixture file loads")
        .build()
        .expect("the locale is loaded");

    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.market").as_ref(),
        "Market"
    );
}
