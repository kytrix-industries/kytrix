use std::{collections::VecDeque, hash::Hash};

use ahash::AHashMap;

pub(super) struct InsertionOrderedCache<K, V> {
    capacity: usize,
    entries: AHashMap<K, V>,
    insertion_order: VecDeque<K>,
}

impl<K, V> InsertionOrderedCache<K, V>
where
    K: Eq + Hash + Clone,
{
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: AHashMap::with_capacity(capacity),
            insertion_order: VecDeque::with_capacity(capacity),
        }
    }

    pub(super) fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    pub(super) fn insert(&mut self, key: K, value: V) {
        if let Some(existing) = self.entries.get_mut(&key) {
            *existing = value;
            return;
        }
        if self.entries.len() >= self.capacity
            && let Some(oldest) = self.insertion_order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.insertion_order.push_back(key.clone());
        self.entries.insert(key, value);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(feature = "test-support")]
    pub(super) fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }
}
