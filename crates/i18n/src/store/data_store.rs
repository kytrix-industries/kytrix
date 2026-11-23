use std::sync::Arc;

use ahash::{AHashMap, RandomState};

use super::MessageStore;
use crate::language::codes::LanguageCode;

#[derive(Debug)]
pub(crate) struct DataStore {
    locales: AHashMap<LanguageCode, Arc<MessageStore>>,
}

impl DataStore {
    pub fn new(locales: Vec<(LanguageCode, Arc<MessageStore>)>) -> Self {
        let mut stores = AHashMap::with_capacity_and_hasher(locales.len(), RandomState::new());
        stores.extend(locales);

        Self { locales: stores }
    }

    #[inline]
    pub fn get(&self, language: LanguageCode) -> Option<&Arc<MessageStore>> {
        self.locales.get(&language)
    }

    pub fn languages(&self) -> impl Iterator<Item = LanguageCode> + '_ {
        self.locales.keys().copied()
    }
}
