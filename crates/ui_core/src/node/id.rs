use std::{fmt, num::NonZeroU32};

use crate::window::WindowId;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    index: u32,
    scope: u32,
    generation: NonZeroU32,
}

impl NodeId {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    #[must_use]
    pub fn window(self) -> Option<WindowId> {
        (self.scope != UNBOUND_SCOPE).then(|| WindowId::from_raw(self.scope))
    }

    #[must_use]
    pub const fn window_tag(self) -> u32 {
        self.scope
    }

    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    #[must_use]
    pub(crate) const fn from_bits(bits: u64) -> Option<Self> {
        let generation = (bits >> 32) as u32;
        match NonZeroU32::new(generation) {
            Some(generation) => Some(Self {
                index: bits as u32,
                scope: UNBOUND_SCOPE,
                generation,
            }),
            None => None,
        }
    }

    pub(crate) const fn with_window_tag(mut self, scope: u32) -> Self {
        self.scope = scope;
        self
    }

    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        match NonZeroU32::new(generation) {
            Some(generation) => Self {
                index,
                scope: UNBOUND_SCOPE,
                generation,
            },
            None => panic!("NodeId 的代际不得为 0"),
        }
    }
}

const UNBOUND_SCOPE: u32 = 0;

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scope == UNBOUND_SCOPE {
            write!(f, "NodeId(n{}.{})", self.index, self.generation)
        } else {
            write!(
                f,
                "NodeId(w{}.n{}.{})",
                self.scope, self.index, self.generation
            )
        }
    }
}

pub(crate) trait ArenaKey: Copy {
    fn from_parts(index: u32, generation: u32) -> Self;
    fn parts(self) -> (u32, u32);

    fn scope(self) -> u32 {
        let _ = self;
        UNBOUND_SCOPE
    }

    fn with_scope(self, scope: u32) -> Self {
        let _ = scope;
        self
    }
}

impl ArenaKey for NodeId {
    fn from_parts(index: u32, generation: u32) -> Self {
        let generation =
            NonZeroU32::new(generation).expect("arena 的代际恒在 1..=u32::MAX（引擎不变量）");
        Self {
            index,
            scope: UNBOUND_SCOPE,
            generation,
        }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation.get())
    }

    fn scope(self) -> u32 {
        self.scope
    }

    fn with_scope(mut self, scope: u32) -> Self {
        self.scope = scope;
        self
    }
}

pub(crate) fn next_generation(generation: u32) -> Option<u32> {
    generation.checked_add(1)
}

#[derive(Debug)]
pub(crate) struct Arena<T, K: ArenaKey = NodeId> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    retired: usize,
    scope: u32,
    _marker: std::marker::PhantomData<fn() -> K>,
}

#[derive(Debug)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

impl<T, K: ArenaKey> Arena<T, K> {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            retired: 0,
            scope: UNBOUND_SCOPE,
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) fn scope(&self) -> u32 {
        self.scope
    }

    pub(crate) fn set_scope(&mut self, scope: u32) {
        debug_assert!(
            self.slots.len() <= 1,
            "arena 的作用域只能在铸造句柄之前绑定"
        );
        self.scope = scope;
    }

    fn accept(&self, key: K) -> bool {
        let scope = key.scope();
        scope == UNBOUND_SCOPE || scope == self.scope
    }

    pub(crate) fn insert_with(&mut self, make: impl FnOnce(K) -> T) -> K {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            debug_assert!(slot.value.is_none(), "free list 指向了存活槽位");
            let key = K::from_parts(index, slot.generation).with_scope(self.scope);
            slot.value = Some(make(key));
            key
        } else {
            let index = u32::try_from(self.slots.len()).expect("arena 槽位溢出 u32");
            let key = K::from_parts(index, 1).with_scope(self.scope);
            self.slots.push(Slot {
                generation: 1,
                value: Some(make(key)),
            });
            key
        }
    }

    pub(crate) fn remove(&mut self, key: K) -> Option<T> {
        if !self.accept(key) {
            return None;
        }
        let (index, generation) = key.parts();
        let slot = self.slots.get_mut(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        let value = slot.value.take()?;
        match next_generation(slot.generation) {
            Some(next) => {
                slot.generation = next;
                self.free.push(index);
            }
            None => {
                slot.generation = 0;
                self.retired += 1;
            }
        }
        Some(value)
    }

    pub(crate) fn get(&self, key: K) -> Option<&T> {
        if !self.accept(key) {
            return None;
        }
        let (index, generation) = key.parts();
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.value.as_ref()
    }

    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut T> {
        if !self.accept(key) {
            return None;
        }
        let (index, generation) = key.parts();
        let slot = self.slots.get_mut(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.value.as_mut()
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|slot| slot.value.as_mut())
    }

    pub(crate) fn len(&self) -> usize {
        self.slots
            .len()
            .saturating_sub(self.free.len() + self.retired)
    }

    #[cfg(test)]
    pub(crate) fn retired_slots(&self) -> usize {
        self.retired
    }

    #[cfg(test)]
    pub(crate) fn force_generation_for_test(&mut self, key: K, generation: u32) -> K {
        let (index, _) = key.parts();
        let slot = &mut self.slots[index as usize];
        slot.generation = generation;
        K::from_parts(index, generation)
    }
}

#[cfg(test)]
mod tests {
    use super::{Arena, NodeId, next_generation};

    #[test]
    fn node_id_is_12_bytes_with_window_scope() {
        assert_eq!(size_of::<NodeId>(), 12);
        assert_eq!(size_of::<Option<NodeId>>(), 12);
    }

    #[test]
    fn node_id_to_bits_layout() {
        let id = NodeId::for_test(3, 7);

        assert_eq!(id.to_bits(), (7u64 << 32) | 3);
        assert_eq!(id.index(), 3);
        assert_eq!(id.generation(), 7);
        assert_eq!(
            size_of::<NodeId>(),
            size_of::<u32>() * 3,
            "index + 窗口 tag + 代际"
        );
    }

    #[test]
    fn node_id_from_bits_roundtrip() {
        let id = NodeId::for_test(3, 7);

        assert_eq!(NodeId::from_bits(id.to_bits()), Some(id));
        assert_eq!(NodeId::from_bits(3), None, "代际 0 的编码非法");
        assert_eq!(
            NodeId::from_bits(id.to_bits()).map(NodeId::to_bits),
            Some(id.to_bits())
        );
    }

    #[test]
    fn arena_insert_mints_generation_one() {
        let mut arena: Arena<u32, NodeId> = Arena::new();
        let first = arena.insert_with(|_| 10);
        let second = arena.insert_with(|_| 20);

        assert_eq!(first.index(), 0);
        assert_eq!(second.index(), 1);
        assert_eq!((first.generation(), second.generation()), (1, 1));
        assert_eq!(arena.len(), 2);
    }

    #[test]
    fn arena_get_rejects_stale_after_remove() {
        let mut arena: Arena<u32, NodeId> = Arena::new();
        let id = arena.insert_with(|_| 7);

        assert_eq!(arena.remove(id), Some(7));
        assert_eq!(arena.get(id), None);
        assert_eq!(arena.get_mut(id), None);
        assert_eq!(arena.remove(id), None, "重复释放必须幂等");
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn arena_reuses_index_with_bumped_generation() {
        let mut arena: Arena<u32, NodeId> = Arena::new();
        let old = arena.insert_with(|_| 1);
        arena.remove(old);
        let new = arena.insert_with(|_| 2);

        assert_eq!(new.index(), old.index(), "index 必须复用");
        assert_eq!(new.generation(), old.generation() + 1);
        assert_eq!(arena.get(old), None, "复用后旧句柄必须失效");
        assert_eq!(arena.get(new), Some(&2));
    }

    #[test]
    fn arena_generation_exhaustion_retires_instead_of_wrapping() {
        assert_eq!(next_generation(1), Some(2));
        assert_eq!(next_generation(u32::MAX - 1), Some(u32::MAX));
        assert_eq!(next_generation(u32::MAX), None, "代际空间用尽");
    }

    #[test]
    fn retired_slot_never_revives_a_stale_handle() {
        let mut arena: Arena<u32, NodeId> = Arena::new();
        let first = arena.insert_with(|_| 1);
        let almost = arena.force_generation_for_test(first, u32::MAX);

        assert_eq!(arena.remove(almost), Some(1));
        assert_eq!(arena.get(almost), None, "刚释放的句柄必须失效");
        assert_eq!(arena.retired_slots(), 1, "用尽的槽位必须退休");
        assert_eq!(arena.len(), 0);

        for value in 2..=5 {
            let next = arena.insert_with(|_| value);
            assert_ne!(next.index(), first.index(), "退休槽位不得被复用");
        }
        assert_eq!(arena.get(first), None, "旧代际的句柄不得复活");
    }

    #[test]
    fn arena_values_mut_covers_only_live() {
        let mut arena: Arena<u32, NodeId> = Arena::new();
        let a = arena.insert_with(|_| 1);
        let b = arena.insert_with(|_| 2);
        let c = arena.insert_with(|_| 3);
        arena.remove(b);

        let live: Vec<u32> = arena.values_mut().map(|value| *value).collect();
        assert_eq!(live, vec![1, 3]);
        assert_eq!(arena.len(), 2);
        assert!(arena.get(a).is_some() && arena.get(c).is_some());
    }
}
