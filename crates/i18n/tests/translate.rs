use i18n::{
    language::codes::LanguageCode,
    t,
    translator::{builder::Builder, types::I18n},
};

/// Locale files loaded from disk, exercising the YAML pipeline end to end.
fn i18n() -> I18n {
    Builder::default()
        .locales("tests/locales")
        .expect("the locale directory loads")
        .fallback(LanguageCode::ZhCn)
        .build()
        .expect("English and Chinese are loaded")
}

#[test]
fn loads_every_locale_file_in_the_directory() {
    let i18n = i18n();

    assert_eq!(
        i18n.languages(),
        [LanguageCode::ZhCn, LanguageCode::ZhTw, LanguageCode::En]
    );
    assert!(i18n.has_language(LanguageCode::En));
    assert!(!i18n.has_language(LanguageCode::Ja));
}

#[test]
fn flattens_nested_sections_into_dotted_keys() {
    let i18n = i18n();

    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.market").as_ref(),
        "Market"
    );
    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.portfolio").as_ref(),
        "Portfolio"
    );
    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.sub.quant").as_ref(),
        "Quant"
    );
    assert_eq!(
        i18n.translate(LanguageCode::En, "window.close").as_ref(),
        "Close"
    );
}

#[test]
fn renders_every_language_at_runtime() {
    let i18n = i18n();

    assert_eq!(
        i18n.translate(LanguageCode::En, "menu.market").as_ref(),
        "Market"
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "menu.market").as_ref(),
        "行情"
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhTw, "menu.market").as_ref(),
        "行情"
    );
}

#[test]
fn interpolates_every_argument_kind() {
    let i18n = i18n();
    let name = "Ada";
    let count = 3;
    let ratio = 0.5;

    assert_eq!(
        i18n.translate_with(LanguageCode::En, "greeting", &[("name", &name)])
            .as_ref(),
        "Hello, Ada!"
    );
    assert_eq!(
        i18n.translate_with(
            LanguageCode::En,
            "inbox",
            &[("count", &count), ("name", &name)]
        )
        .as_ref(),
        "Ada has 3 unread messages."
    );
    assert_eq!(
        i18n.translate_with(LanguageCode::En, "ratio", &[("ratio", &ratio)])
            .as_ref(),
        "Ratio: 0.5"
    );
}

#[test]
fn interpolated_messages_follow_the_language_they_are_given() {
    let i18n = i18n();
    let name = "Ada";

    assert_eq!(
        i18n.translate_with(LanguageCode::En, "greeting", &[("name", &name)])
            .as_ref(),
        "Hello, Ada!"
    );
    assert_eq!(
        i18n.translate_with(LanguageCode::ZhCn, "greeting", &[("name", &name)])
            .as_ref(),
        "你好，Ada！"
    );
}

#[test]
fn translations_are_shared_strings() {
    let i18n = i18n();
    let translated = i18n.translate(LanguageCode::En, "menu.market");

    let shared: gpui_kit::SharedString = translated.clone();
    assert_eq!(shared, translated);
    assert_eq!(shared.as_ref(), "Market");
}

#[test]
fn repeated_translation_returns_the_same_text_without_rerendering() {
    let i18n = i18n();
    let count = 7;
    let name = "Inbox";

    let first = i18n.translate_with(
        LanguageCode::En,
        "inbox",
        &[("count", &count), ("name", &name)],
    );
    let second = i18n.translate_with(
        LanguageCode::En,
        "inbox",
        &[("count", &count), ("name", &name)],
    );

    assert_eq!(first.as_ref(), "Inbox has 7 unread messages.");
    assert!(std::ptr::eq(first.as_ref(), second.as_ref()));
}

#[test]
fn fallback_covers_messages_a_language_lacks() {
    let i18n = i18n();

    // `zh-TW` only translates `menu.market`, and `%{name}` is not supplied, so the
    // rest of the message comes from `zh-CN` and renders without argument.
    assert_eq!(
        i18n.translate(LanguageCode::ZhTw, "menu.market").as_ref(),
        "行情"
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhTw, "greeting").as_ref(),
        "你好，！"
    );
    assert_eq!(
        i18n.translate(LanguageCode::ZhTw, "menu.sub.quant")
            .as_ref(),
        "量化"
    );

    // `zh-CN` lacks `ratio`, which only English defines, and English is not the
    // fallback of this instance.
    assert_eq!(i18n.translate(LanguageCode::ZhCn, "ratio").as_ref(), "");
    assert_eq!(
        i18n.translate(LanguageCode::ZhCn, "nothing.here").as_ref(),
        ""
    );
}

#[test]
fn messages_missing_from_every_loaded_locale_translate_to_empty_text() {
    let i18n = i18n();

    assert_eq!(i18n.translate(LanguageCode::En, "menu.quant").as_ref(), "");
    assert!(i18n.try_translate(LanguageCode::En, "menu.quant").is_none());
    assert_eq!(
        i18n.translate(LanguageCode::En, "window.close").as_ref(),
        "Close"
    );
}

#[test]
fn t_macro_translates_the_installed_translator() {
    let cx = gpui_kit::TestAppContext::single();

    cx.update(|cx| {
        cx.set_global(i18n());
        cx.set_global(LanguageCode::En);

        assert_eq!(t!(cx, "menu.market").as_ref(), "Market");

        cx.set_global(LanguageCode::ZhCn);
        assert_eq!(t!(cx, "menu.market").as_ref(), "行情");

        let name = "Ada";
        assert_eq!(t!(cx, "greeting", name = name).as_ref(), "你好，Ada！");

        cx.set_global(LanguageCode::En);
        assert_eq!(t!(cx, "greeting", name = name).as_ref(), "Hello, Ada!");
    });
}
