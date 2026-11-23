use ahash::{AHashMap, RandomState};

use crate::template::interpolate::Template;

#[derive(Debug)]
pub(crate) struct MessageStore {
    messages: AHashMap<Box<str>, Template>,
}

impl MessageStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            messages: AHashMap::with_capacity_and_hasher(capacity, RandomState::new()),
        }
    }

    pub fn insert(&mut self, key: Box<str>, template: Template) -> bool {
        match self.messages.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(template);
                true
            }
        }
    }

    #[inline]
    pub fn get(&self, key: &str) -> Option<&Template> {
        self.messages.get(key)
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.messages.keys().map(Box::as_ref)
    }
}
