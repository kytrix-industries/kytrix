use std::{path::Path, sync::Arc};

use ahash::{AHashMap, RandomState};

use super::types::I18n;
use crate::{
    error::types::Error,
    language::codes::LanguageCode,
    loader,
    store::{DataStore, MessageStore},
};

#[derive(Debug, Default)]
pub struct Builder {
    locales: Vec<(LanguageCode, MessageStore)>,
    fallback: Option<LanguageCode>,
    language: Option<LanguageCode>,
}

impl Builder {
    pub fn locales(mut self, directory: impl AsRef<Path>) -> Result<Self, Error> {
        self.locales
            .extend(loader::load_directory(directory.as_ref())?);

        Ok(self)
    }

    pub fn locale_file(mut self, path: impl AsRef<Path>) -> Result<Self, Error> {
        self.locales.push(loader::load_file(path.as_ref())?);

        Ok(self)
    }

    pub fn locale_yaml(mut self, language: LanguageCode, yaml: &str) -> Result<Self, Error> {
        let store = loader::load_str(yaml, Path::new(&format!("{language}.yaml")))?;
        self.locales.push((language, store));

        Ok(self)
    }

    pub fn fallback(mut self, language: LanguageCode) -> Self {
        self.fallback = Some(language);
        self
    }

    pub fn language(mut self, language: LanguageCode) -> Self {
        self.language = Some(language);
        self
    }

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
