use std::{fmt::Display, sync::Mutex};

use gpui_kit::SharedString;

use crate::{
    cache::{RenderedCache, bind_arguments},
    language::codes::LanguageCode,
    store::DataStore,
    template::interpolate::Template,
};

/// Renders localized text for a language.
///
/// The catalogue is language-agnostic: the language to render is passed to every
/// call, because the selected language is a global of its own.
///
/// Messages live in YAML locale files named after their language code, using the
/// codes from the
/// [Google Workspace language list](https://developers.google.com/workspace/admin/directory/v1/languages):
///
/// ```yaml
/// # locales/en.yaml
/// menu:
///   market: Market
/// greeting: "Hello, %{name}!"
/// ```
///
/// ```yaml
/// # locales/zh-CN.yaml
/// menu:
///   market: 行情
/// greeting: "你好，%{name}！"
/// ```
///
/// # Examples
///
/// ```no_run
/// use i18n::{language::codes::LanguageCode, translator::types::I18n};
///
/// let i18n = I18n::builder()
///     .locales("locales")?
///     .fallback(LanguageCode::En)
///     .build()?;
///
/// assert_eq!(
///     i18n.translate(LanguageCode::ZhCn, "menu.market").as_ref(),
///     "行情"
/// );
/// # Ok::<(), i18n::error::types::Error>(())
/// ```
#[derive(Debug)]
pub struct I18n {
    /// Language used for messages the selected language does not define.
    fallback: Option<LanguageCode>,
    /// Messages of every loaded locale.
    stores: DataStore,
    /// Cached renderings of messages with placeholders.
    cache: Mutex<RenderedCache>,
}

impl gpui_kit::Global for I18n {}

impl I18n {
    /// Assembles an instance from locales loaded by [`I18n::builder`].
    pub(super) fn new(fallback: Option<LanguageCode>, stores: DataStore) -> Self {
        Self {
            fallback,
            stores,
            cache: Mutex::new(RenderedCache::default()),
        }
    }

    /// Starts building an instance.
    pub fn builder() -> super::builder::Builder {
        super::builder::Builder::default()
    }

    /// Returns the language used for messages the selected language does not
    /// define.
    pub fn fallback(&self) -> Option<LanguageCode> {
        self.fallback
    }

    /// Returns the languages with a loaded locale, in the order of the
    /// [Google Workspace language list](https://developers.google.com/workspace/admin/directory/v1/languages).
    pub fn languages(&self) -> Vec<LanguageCode> {
        let mut languages: Vec<_> = self.stores.languages().collect();
        languages.sort_by_key(|language| {
            LanguageCode::all()
                .position(|candidate| candidate == *language)
                .unwrap_or(usize::MAX)
        });

        languages
    }

    /// Returns `true` when `language` has a loaded locale.
    pub fn has_language(&self, language: LanguageCode) -> bool {
        self.stores.get(language).is_some()
    }

    /// Returns the message keys defined by `language`.
    ///
    /// Returns `None` when `language` has no loaded locale. Keys are yielded in
    /// unspecified order.
    pub fn keys(&self, language: LanguageCode) -> Option<impl Iterator<Item = &str>> {
        self.stores.get(language).map(|store| store.keys())
    }

    /// Returns the message registered for `key` in `language`, or an empty
    /// string.
    ///
    /// Use [`I18n::try_translate`] to tell a missing message apart from one that
    /// is deliberately empty.
    #[inline]
    pub fn translate(&self, language: LanguageCode, key: &str) -> SharedString {
        self.try_translate(language, key).unwrap_or_default()
    }

    /// Returns the message registered for `key` in `language`.
    ///
    /// Messages without placeholders return a clone of the stored text. Messages
    /// with placeholders are rendered once per language, key and argument values,
    /// and reused afterwards.
    #[inline]
    pub fn try_translate(&self, language: LanguageCode, key: &str) -> Option<SharedString> {
        self.try_translate_with(language, key, &[])
    }

    /// Returns the message registered for `key` in `language`, replacing its
    /// `%{name}` placeholders with `arguments`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use i18n::{language::codes::LanguageCode, translator::types::I18n};
    /// let i18n = I18n::builder()
    ///     .locale_yaml(LanguageCode::En, "greeting: \"Hello, %{name}!\"")?
    ///     .build()?;
    ///
    /// let name = "Ada";
    /// assert_eq!(
    ///     i18n.translate_with(LanguageCode::En, "greeting", &[("name", &name)])
    ///         .as_ref(),
    ///     "Hello, Ada!"
    /// );
    /// # Ok::<(), i18n::error::types::Error>(())
    /// ```
    pub fn translate_with(
        &self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> SharedString {
        self.try_translate_with(language, key, arguments)
            .unwrap_or_default()
    }

    /// Returns the message registered for `key` in `language`, replacing its
    /// `%{name}` placeholders with `arguments`.
    pub fn try_translate_with(
        &self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> Option<SharedString> {
        let template = self.template(language, key)?;

        if let Some(text) = template.text() {
            return Some(text.into());
        }

        let arguments = bind_arguments(template, arguments);

        if let Some(rendered) = self
            .cache
            .lock()
            .expect("the message cache is never poisoned")
            .get(language, key, &arguments)
        {
            return Some(rendered);
        }

        let mut text = String::new();
        template.write_with(&arguments, &mut text);
        let rendered = SharedString::from(text);

        self.cache
            .lock()
            .expect("the message cache is never poisoned")
            .insert(language, key, &arguments, rendered.clone());

        Some(rendered)
    }

    /// Returns the template of `key` in `language`, then in the fallback
    /// language.
    #[inline]
    fn template(&self, language: LanguageCode, key: &str) -> Option<&Template> {
        if let Some(template) = self.stores.get(language).and_then(|store| store.get(key)) {
            return Some(template);
        }

        let fallback = self.fallback.filter(|fallback| *fallback != language)?;

        self.stores.get(fallback).and_then(|store| store.get(key))
    }
}
