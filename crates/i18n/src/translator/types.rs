use std::{fmt::Display, sync::Mutex};

use smol_str::SmolStr;

use crate::{
    cache::{RenderedCache, bind_arguments},
    language::codes::LanguageCode,
    store::DataStore,
    template::interpolate::Template,
};

#[derive(Debug)]
pub struct I18n {
    fallback: Option<LanguageCode>,
    stores: DataStore,
    cache: Mutex<RenderedCache>,
}

impl I18n {
    pub(super) fn new(fallback: Option<LanguageCode>, stores: DataStore) -> Self {
        Self {
            fallback,
            stores,
            cache: Mutex::new(RenderedCache::default()),
        }
    }

    pub fn builder() -> super::builder::Builder {
        super::builder::Builder::default()
    }

    pub fn fallback(&self) -> Option<LanguageCode> {
        self.fallback
    }

    pub fn languages(&self) -> Vec<LanguageCode> {
        let mut languages: Vec<_> = self.stores.languages().collect();
        languages.sort_by_key(|language| {
            LanguageCode::all()
                .position(|candidate| candidate == *language)
                .unwrap_or(usize::MAX)
        });

        languages
    }

    pub fn has_language(&self, language: LanguageCode) -> bool {
        self.stores.get(language).is_some()
    }

    pub fn keys(&self, language: LanguageCode) -> Option<impl Iterator<Item = &str>> {
        self.stores.get(language).map(|store| store.keys())
    }

    #[inline]
    pub fn translate(&self, language: LanguageCode, key: &str) -> SmolStr {
        self.try_translate(language, key).unwrap_or_default()
    }

    #[inline]
    pub fn try_translate(&self, language: LanguageCode, key: &str) -> Option<SmolStr> {
        self.try_translate_with(language, key, &[])
    }

    pub fn translate_with(
        &self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> SmolStr {
        self.try_translate_with(language, key, arguments)
            .unwrap_or_default()
    }

    pub fn try_translate_with(
        &self,
        language: LanguageCode,
        key: &str,
        arguments: &[(&str, &dyn Display)],
    ) -> Option<SmolStr> {
        let template = self.template(language, key)?;

        if let Some(text) = template.text() {
            return Some(text.clone());
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
        let rendered = SmolStr::from(text);

        self.cache
            .lock()
            .expect("the message cache is never poisoned")
            .insert(language, key, &arguments, rendered.clone());

        Some(rendered)
    }

    #[inline]
    fn template(&self, language: LanguageCode, key: &str) -> Option<&Template> {
        if let Some(template) = self.stores.get(language).and_then(|store| store.get(key)) {
            return Some(template);
        }

        let fallback = self.fallback.filter(|fallback| *fallback != language)?;

        self.stores.get(fallback).and_then(|store| store.get(key))
    }
}
