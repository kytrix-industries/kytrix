use ahash::{AHashMap, RandomState};

use crate::template::interpolate::Template;

/// The messages of a single locale, keyed by their flattened dotted key.
///
/// A store is immutable once built, so [`MessageStore::get`] never allocates and
/// only ever reads a `&str`.
#[derive(Debug)]
pub(crate) struct MessageStore {
    messages: AHashMap<Box<str>, Template>,
}

impl MessageStore {
    /// Builds a store sized for `capacity` messages.
    pub fn new(capacity: usize) -> Self {
        Self {
            messages: AHashMap::with_capacity_and_hasher(capacity, RandomState::new()),
        }
    }

    /// Inserts a message, returning `false` when the key already exists.
    pub fn insert(&mut self, key: Box<str>, template: Template) -> bool {
        match self.messages.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(template);
                true
            }
        }
    }

    /// Returns the template registered for `key`.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&Template> {
        self.messages.get(key)
    }

    /// Returns the number of messages in this store.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns the message keys of this store in unspecified order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.messages.keys().map(Box::as_ref)
    }
}
