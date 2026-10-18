use std::sync::Arc;

use ahash::{AHashMap, RandomState};

use super::MessageStore;
use crate::language::codes::LanguageCode;

/// The locale stores of an [`I18n`](crate::translator::types::I18n) instance.
///
/// Holding each store behind an [`Arc`] keeps a language switch to a single
/// atomic store, and a lookup to one hash probe.
#[derive(Debug)]
pub(crate) struct DataStore {
    locales: AHashMap<LanguageCode, Arc<MessageStore>>,
}

impl DataStore {
    /// Builds a data store from the loaded locales.
    pub fn new(locales: Vec<(LanguageCode, Arc<MessageStore>)>) -> Self {
        let mut stores = AHashMap::with_capacity_and_hasher(locales.len(), RandomState::new());
        stores.extend(locales);

        Self { locales: stores }
    }

    /// Returns the store of `language`, if that locale was loaded.
    #[inline]
    pub fn get(&self, language: LanguageCode) -> Option<&Arc<MessageStore>> {
        self.locales.get(&language)
    }

    /// Returns the loaded language codes.
    pub fn languages(&self) -> impl Iterator<Item = LanguageCode> + '_ {
        self.locales.keys().copied()
    }
}
