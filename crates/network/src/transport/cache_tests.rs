use crate::transport::cache::InsertionOrderedCache;

const CAPACITY: usize = 4;

#[test]
fn reinserting_a_key_replaces_its_value_without_moving_it() {
    let mut cache = InsertionOrderedCache::new(CAPACITY);
    cache.insert(1_u32, "first");
    cache.insert(2_u32, "second");
    cache.insert(1_u32, "replaced");

    assert_eq!(cache.get(&1), Some(&"replaced"));
    assert_eq!(cache.len(), 2);

    for key in 3..=(CAPACITY as u32 + 1) {
        cache.insert(key, "later");
    }
    assert_eq!(cache.len(), CAPACITY);
    assert_eq!(cache.get(&1), None, "the oldest key is evicted first");
    assert_eq!(cache.get(&2), Some(&"second"));
    assert_eq!(cache.get(&(CAPACITY as u32 + 1)), Some(&"later"));
}

#[test]
fn the_cache_never_grows_past_its_capacity() {
    let mut cache = InsertionOrderedCache::new(CAPACITY);
    for key in 0..(CAPACITY as u32 * 8) {
        cache.insert(key, key);
        assert!(cache.len() <= CAPACITY);
    }
    assert_eq!(cache.len(), CAPACITY);
}
