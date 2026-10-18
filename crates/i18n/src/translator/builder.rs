use std::{path::Path, sync::Arc};

use ahash::{AHashMap, RandomState};

use super::types::I18n;
use crate::{
    error::types::Error,
    language::codes::LanguageCode,
    loader,
    store::{DataStore, MessageStore},
};

/// Builds an [`I18n`] instance from YAML locale files.
///
/// The builder starts empty, has no fallback and selects [`LanguageCode::En`].
#[derive(Debug, Default)]
pub struct Builder {
    locales: Vec<(LanguageCode, MessageStore)>,
    fallback: Option<LanguageCode>,
    language: Option<LanguageCode>,
}

impl Builder {
    /// Loads every `.yaml` and `.yml` file in `directory`.
    ///
    /// Each file is named after its language code, e.g. `en.yaml` or
    /// `zh-CN.yaml`. Files that do not have a YAML extension are ignored.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be listed, when a file is not valid YAML
    /// or not valid UTF-8, when a file name is not a
    /// [language code](LanguageCode), when a message is not a valid template, and
    /// when the same key is defined twice.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use i18n::translator::types::I18n;
    /// let i18n = I18n::builder().locales("locales")?.build()?;
    /// # Ok::<(), i18n::error::types::Error>(())
    /// ```
    pub fn locales(mut self, directory: impl AsRef<Path>) -> Result<Self, Error> {
        self.locales
            .extend(loader::load_directory(directory.as_ref())?);

        Ok(self)
    }

    /// Loads one locale file.
    ///
    /// # Errors
    ///
    /// See [`Builder::locales`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use i18n::translator::types::I18n;
    /// let i18n = I18n::builder()
    ///     .locale_file("en.yaml")?
    ///     .locale_file("zh-CN.yaml")?
    ///     .build()?;
    /// # Ok::<(), i18n::error::types::Error>(())
    /// ```
    pub fn locale_file(mut self, path: impl AsRef<Path>) -> Result<Self, Error> {
        self.locales.push(loader::load_file(path.as_ref())?);

        Ok(self)
    }

    /// Adds the messages of a locale document written in `language`.
    ///
    /// # Errors
    ///
    /// Fails when `yaml` is not valid YAML, when a message is not a valid
    /// template, and when the same key is defined twice.
    ///
    /// # Examples
    ///
    /// ```
    /// # use i18n::{language::codes::LanguageCode, translator::types::I18n};
    /// let i18n = I18n::builder()
    ///     .locale_yaml(LanguageCode::En, "menu:\n  market: Market")?
    ///     .build()?;
    ///
    /// assert_eq!(
    ///     i18n.translate(LanguageCode::En, "menu.market").as_ref(),
    ///     "Market"
    /// );
    /// # Ok::<(), i18n::error::types::Error>(())
    /// ```
    pub fn locale_yaml(mut self, language: LanguageCode, yaml: &str) -> Result<Self, Error> {
        let store = loader::load_str(yaml, Path::new(&format!("{language}.yaml")))?;
        self.locales.push((language, store));

        Ok(self)
    }

    /// Sets the language used for messages the selected language does not
    /// define.
    pub fn fallback(mut self, language: LanguageCode) -> Self {
        self.fallback = Some(language);
        self
    }

    /// Sets the language the builder checks against the loaded locales.
    ///
    /// Defaults to [`LanguageCode::En`]. The language to render is not stored in
    /// the built instance — it is passed to [`I18n::translate`] — so this only
    /// rejects a language nothing loaded defines.
    pub fn language(mut self, language: LanguageCode) -> Self {
        self.language = Some(language);
        self
    }

    /// Builds the instance.
    ///
    /// # Errors
    ///
    /// Fails when the same language was loaded more than once, and when the
    /// selected language or the fallback language has no loaded locale.
    pub fn build(self) -> Result<I18n, Error> {
        let mut locales =
            AHashMap::with_capacity_and_hasher(self.locales.len(), RandomState::new());
        for (language, store) in self.locales {
            if locales.insert(language, Arc::new(store)).is_some() {
                return Err(Error::LanguageLoadedTwice { language });
            }
        }

        let language = self.language.unwrap_or_default();
        let fallback = self.fallback;

        if !locales.contains_key(&language) {
            return Err(Error::UnknownLanguage { language });
        }
        if let Some(fallback) = fallback
            && !locales.contains_key(&fallback)
        {
            return Err(Error::UnknownLanguage { language: fallback });
        }

        Ok(I18n::new(
            fallback,
            DataStore::new(locales.into_iter().collect()),
        ))
    }
}
