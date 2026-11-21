//! The translation context handed to code that renders localized text.
//!
//! [`Translate`] replaces reading the translator and the language out of GPUI global
//! state: the host builds the handle once and hands it to the code that renders, so
//! nothing about translating a message depends on a process-wide installation.
//! [`t_smol!`](crate::t_smol) is the macro that pairs with it.

use std::{cell::Cell, fmt::Display, rc::Rc};

use smol_str::SmolStr;

use crate::{language::codes::LanguageCode, translator::types::I18n};

/// A cloneable handle to a translator and the language it renders.
///
/// Cloning shares both the catalogue and the selected language, so a host that switches
/// the language through one clone switches it for every clone it has handed out. The
/// handle is the replacement for the GPUI globals [`t!`](crate::t) reads: a renderer
/// receives it as an argument instead of reaching into application state.
///
/// # Examples
///
/// ```
/// use std::rc::Rc;
///
/// use i18n::{language::codes::LanguageCode, translate::Translate, translator::types::I18n};
///
/// let i18n = I18n::builder()
///     .locale_yaml(
///         LanguageCode::En,
///         "menu:\n  market: Market\ngreeting: \"Hello, %{name}!\"",
///     )?
///     .build()?;
/// let translate = Translate::new(Rc::new(i18n), LanguageCode::En);
///
/// assert_eq!(translate.translate("menu.market"), "Market");
///
/// let name = "Ada";
/// assert_eq!(
///     translate.translate_with("greeting", &[("name", &name)]),
///     "Hello, Ada!"
/// );
/// # Ok::<(), i18n::error::types::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Translate {
    i18n: Rc<I18n>,
    language: Rc<Cell<LanguageCode>>,
}

impl Translate {
    /// Wraps `i18n` so it renders `language`.
    pub fn new(i18n: Rc<I18n>, language: LanguageCode) -> Self {
        Self {
            i18n,
            language: Rc::new(Cell::new(language)),
        }
    }

    /// Returns the language messages are rendered in.
    pub fn language(&self) -> LanguageCode {
        self.language.get()
    }

    /// Selects the language, returning whether it changed.
    pub fn set_language(&self, language: LanguageCode) -> bool {
        self.language.replace(language) != language
    }

    /// Returns the message registered for `key`, or an empty string when no loaded
    /// locale defines it.
    ///
    /// A message missing from every locale renders as nothing rather than failing, the
    /// same rule [`t!`](crate::t) follows, so a broken key shows up as a gap instead of
    /// a crash. Use [`Translate::try_translate`] to tell a missing message from one that
    /// is deliberately empty.
    pub fn translate(&self, key: &str) -> SmolStr {
        self.try_translate(key).unwrap_or_default()
    }

    /// Returns the message registered for `key`, or `None` when no loaded locale
    /// defines it.
    pub fn try_translate(&self, key: &str) -> Option<SmolStr> {
        self.i18n
            .try_translate(self.language(), key)
            .map(|text| SmolStr::new(text.as_str()))
    }

    /// Returns the message registered for `key`, replacing its `%{name}` placeholders
    /// with `arguments`, or an empty string when no loaded locale defines it.
    pub fn translate_with(&self, key: &str, arguments: &[(&str, &dyn Display)]) -> SmolStr {
        self.try_translate_with(key, arguments).unwrap_or_default()
    }

    /// Returns the message registered for `key` with its `%{name}` placeholders
    /// replaced, or `None` when no loaded locale defines it.
    pub fn try_translate_with(
        &self,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> Option<SmolStr> {
        self.i18n
            .try_translate_with(self.language(), key, arguments)
            .map(|text| SmolStr::new(text.as_str()))
    }
}

/// Translates a message through an explicitly injected [`Translate`] context, returning
/// a [`SmolStr`].
///
/// This is the macro new code uses in place of [`t!`](crate::t): the context is an
/// argument instead of a GPUI global, and the text needs no gpui type. Its arguments
/// work exactly like the ones [`t!`](crate::t) takes — a placeholder is bound either to
/// an expression or to a local variable of that name, and a message that is missing
/// from every loaded locale translates to an empty string.
///
/// ```text
/// t_smol!(translate, "menu.market")             // a message without placeholders
/// t_smol!(translate, "greeting", name = name)   // a placeholder bound to an expression
/// t_smol!(translate, "greeting", name)          // a placeholder bound to a local variable
/// ```
///
/// # Examples
///
/// ```
/// use std::rc::Rc;
///
/// use i18n::{language::codes::LanguageCode, t_smol, translate::Translate, translator::types::I18n};
/// use smol_str::SmolStr;
///
/// let i18n = I18n::builder()
///     .locale_yaml(
///         LanguageCode::En,
///         "menu:\n  market: Market\ngreeting: \"Hello, %{name}!\"",
///     )?
///     .build()?;
/// let translate = Translate::new(Rc::new(i18n), LanguageCode::En);
///
/// let market: SmolStr = t_smol!(translate, "menu.market");
/// assert_eq!(market, "Market");
///
/// let name = "Ada";
/// assert_eq!(t_smol!(translate, "greeting", name = name), "Hello, Ada!");
/// # Ok::<(), i18n::error::types::Error>(())
/// ```
///
/// # Migrating from the GPUI global
///
/// [`t!`](crate::t) reads the translator and the language out of GPUI globals and returns
/// a gpui `SharedString`. Two changes are needed to move a call site:
///
/// ```text
/// // Before: the renderer reached into process state.
/// let text = t!(cx, "error.network.timeout");
///
/// // After: the host hands the context in, and the text is a `SmolStr`.
/// let text = t_smol!(translate, "error.network.timeout");
/// ```
///
/// The host builds the handle where it builds the rest of the application, e.g. from the
/// catalogue embedded in the binary:
///
/// ```
/// use std::rc::Rc;
///
/// use i18n::{language::codes::LanguageCode, global::embedded, translate::Translate};
///
/// # fn build() -> Result<Translate, i18n::error::types::Error> {
/// let translate = Translate::new(Rc::new(embedded::load()?), LanguageCode::En);
/// # Ok(translate)
/// # }
/// # build().expect("the embedded locales load");
/// ```
///
/// [`t!`](crate::t) is removed in P5, together with the GPUI globals it reads; new code
/// uses this macro.
#[macro_export]
macro_rules! t_smol {
    ($translate:expr, $key:expr $(,)?) => {
        ($translate).translate($key)
    };
    ($translate:expr, $key:expr, $($argument:ident $(= $value:expr)?),+ $(,)?) => {
        ($translate).translate_with(
            $key,
            &[$(
                (
                    stringify!($argument),
                    &$crate::__i18n_value!($argument $(= $value)?) as &dyn ::std::fmt::Display,
                ),
            )+],
        )
    };
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use smol_str::SmolStr;

    use super::Translate;
    use crate::{language::codes::LanguageCode, translator::types::I18n};

    const MESSAGES_EN: &str = "\
menu:
  market: Market
greeting: \"Hello, %{name}!\"
inbox:
  one: \"One unread message\"
  other: \"%{count} unread messages\"
price: \"%{amount} USD\"";

    const MESSAGES_ZH: &str = "\
menu:
  market: 行情
greeting: \"你好，%{name}！\"
inbox:
  one: \"一封未读消息\"
  other: \"%{count} 封未读消息\"
price: \"%{amount} 美元\"";

    fn catalogue() -> I18n {
        I18n::builder()
            .locale_yaml(LanguageCode::En, MESSAGES_EN)
            .expect("valid English locale")
            .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
            .expect("valid Chinese locale")
            .fallback(LanguageCode::En)
            .build()
            .expect("both locales are loaded")
    }

    fn translate() -> Translate {
        Translate::new(Rc::new(catalogue()), LanguageCode::En)
    }

    #[test]
    fn a_message_without_placeholders_translates_to_a_short_string() {
        let text: SmolStr = t_smol!(translate(), "menu.market");

        assert_eq!(text, "Market");
    }

    #[test]
    fn placeholders_bind_expressions_and_local_variables() {
        let name = "Ada";

        assert_eq!(t_smol!(translate(), "greeting", name = name), "Hello, Ada!");
        assert_eq!(t_smol!(translate(), "greeting", name), "Hello, Ada!");
    }

    #[test]
    fn a_trailing_comma_is_accepted() {
        assert_eq!(t_smol!(translate(), "menu.market",), "Market");
        assert_eq!(
            t_smol!(translate(), "greeting", name = "Ada",),
            "Hello, Ada!"
        );
    }

    /// The catalogue has no plural rule: a count-sensitive sentence is one key per form,
    /// and the message takes the count like any other placeholder. This pins the shape —
    /// [`t!`](crate::t) has no plural capability either, so the new macro is equivalent.
    #[test]
    fn a_count_sensitive_message_takes_one_key_per_form() {
        let translate = translate();
        let count = 3;

        assert_eq!(t_smol!(translate, "inbox.one"), "One unread message");
        assert_eq!(
            t_smol!(translate, "inbox.other", count = count),
            "3 unread messages"
        );
    }

    #[test]
    fn a_message_missing_from_every_locale_translates_to_empty_text() {
        let translate = translate();

        assert_eq!(t_smol!(translate, "never.defined"), "");
        assert!(translate.try_translate("never.defined").is_none());
    }

    /// A catalogue that defines none of the keys a renderer asks for: every call still
    /// renders the empty string rather than panicking.
    #[test]
    fn a_catalogue_without_the_requested_keys_translates_them_to_empty_text() {
        let i18n = I18n::builder()
            .locale_yaml(LanguageCode::En, "unrelated: message")
            .expect("a locale document with one message is valid")
            .build()
            .expect("an English locale is loaded");
        let translate = Translate::new(Rc::new(i18n), LanguageCode::En);

        assert_eq!(t_smol!(translate, "menu.market"), "");
        assert_eq!(t_smol!(translate, "greeting", name = "Ada"), "");
    }

    #[test]
    fn a_clone_renders_the_language_selected_through_another_clone() {
        let translate = translate();
        let clone = translate.clone();

        assert_eq!(translate.language(), LanguageCode::En);
        assert!(clone.set_language(LanguageCode::ZhCn));
        assert_eq!(translate.language(), LanguageCode::ZhCn);
        assert_eq!(t_smol!(translate, "menu.market"), "行情");
        assert!(!clone.set_language(LanguageCode::ZhCn));
    }

    /// Both rendering paths share the catalogue and the template parser, so the text is
    /// the same; only the returned string type differs.
    #[test]
    fn the_macro_renders_what_the_shared_translate_returns() {
        let catalogue = Rc::new(catalogue());
        let translate = Translate::new(Rc::clone(&catalogue), LanguageCode::En);
        let amount = 42;
        let shared = catalogue.translate_with(LanguageCode::En, "price", &[("amount", &amount)]);

        assert_eq!(
            t_smol!(translate, "menu.market").as_str(),
            catalogue
                .translate(LanguageCode::En, "menu.market")
                .as_ref()
        );
        assert_eq!(
            t_smol!(translate, "price", amount = amount).as_str(),
            shared.as_ref()
        );
        assert_eq!(t_smol!(translate, "never.defined"), "");
    }
}
