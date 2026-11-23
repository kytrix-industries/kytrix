use std::{cell::Cell, fmt::Display, rc::Rc};

use smol_str::SmolStr;

use crate::{language::codes::LanguageCode, translator::types::I18n};

#[derive(Clone, Debug)]
pub struct Translate {
    i18n: Rc<I18n>,
    language: Rc<Cell<LanguageCode>>,
}

impl Translate {
    pub fn new(i18n: Rc<I18n>, language: LanguageCode) -> Self {
        Self {
            i18n,
            language: Rc::new(Cell::new(language)),
        }
    }

    pub fn language(&self) -> LanguageCode {
        self.language.get()
    }

    pub fn set_language(&self, language: LanguageCode) -> bool {
        self.language.replace(language) != language
    }

    pub fn translate(&self, key: &str) -> SmolStr {
        self.try_translate(key).unwrap_or_default()
    }

    pub fn try_translate(&self, key: &str) -> Option<SmolStr> {
        self.i18n
            .try_translate(self.language(), key)
            .map(|text| SmolStr::new(text.as_str()))
    }

    pub fn translate_with(&self, key: &str, arguments: &[(&str, &dyn Display)]) -> SmolStr {
        self.try_translate_with(key, arguments).unwrap_or_default()
    }

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

#[doc(hidden)]
#[macro_export]
macro_rules! __i18n_value {
    ($name:ident = $value:expr) => {
        $value
    };
    ($name:ident) => {
        $name
    };
}

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
                .as_str()
        );
        assert_eq!(
            t_smol!(translate, "price", amount = amount).as_str(),
            shared.as_str()
        );
        assert_eq!(t_smol!(translate, "never.defined"), "");
    }
}
