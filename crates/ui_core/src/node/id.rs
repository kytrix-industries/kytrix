//! `NodeId`（带代际的句柄）与 `Arena`（槽位 + 自由表 + 代际的对象池）。
//!
//! 设计见 `design/phase-03-node-state.md` §6。

use std::{fmt, num::NonZeroU32};

/// 带代际的节点句柄（8 B）。
///
/// 字段私有：句柄只能由 `Arena` 铸出，应用层无法用结构体字面量伪造（P02 S-20 的同类理由）。
/// `generation` 防悬垂：节点卸载后旧句柄的 `get`/`get_mut` 一律返回 `None` → `EngineError::StaleNode`。
///
/// `generation` 用 `NonZeroU32` 表示「**0 永不是合法代际**」这条不变量：类型即证明，并且让
/// `Option<NodeId>` 保持 8 B（`Node.parent` 因此零开销）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    index: u32,
    generation: NonZeroU32,
}

impl NodeId {
    /// arena 槽位下标。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 代际，恒在 `1..=u32::MAX`（0 永不是合法代际，见 `next_generation`）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// `(generation as u64) << 32 | index as u64`。
    ///
    /// 消费者：结构化日志/帧记录需要单值可打印的节点标识（设计 §9.1）；同一节点的历次代际在数值上连续递增。
    /// P04 起它同时是 taffy `NodeId` 的编码（双向无损，见 `NodeId::from_bits`）。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// `NodeId::to_bits` 的逆运算；代际为 0 的编码非法（返回 `None`）。
    ///
    /// 消费者：`layout::taffy_tree::to_engine`（taffy 回调里把 `taffy::NodeId` 换回引擎句柄）。
    #[must_use]
    pub(crate) const fn from_bits(bits: u64) -> Option<Self> {
        let generation = (bits >> 32) as u32;
        match NonZeroU32::new(generation) {
            Some(generation) => Some(Self {
                index: bits as u32,
                generation,
            }),
            None => None,
        }
    }

    /// 仅测试用的构造器（不扩大公开面，S-20 的形态）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        match NonZeroU32::new(generation) {
            Some(generation) => Self { index, generation },
            None => panic!("NodeId 的代际不得为 0"),
        }
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId(n{}.{})", self.index, self.generation)
    }
}

/// arena 键：`NodeId` 与 `LaneId` 共用同一套槽位/自由表/代际规则（两处实例化，不是预防性抽象）。
pub(crate) trait ArenaKey: Copy {
    fn from_parts(index: u32, generation: u32) -> Self;
    fn parts(self) -> (u32, u32);
}

impl ArenaKey for NodeId {
    fn from_parts(index: u32, generation: u32) -> Self {
        let generation =
            NonZeroU32::new(generation).expect("arena 的代际恒在 1..=u32::MAX（引擎不变量）");
        Self { index, generation }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation.get())
    }
}

/// 代际固定为 `1..=u32::MAX`：**0 永不是合法代际**，因此释放后回绕也不会复活旧句柄。
pub(crate) fn next_generation(generation: u32) -> u32 {
    if generation == u32::MAX {
        1
    } else {
        generation + 1
    }
}

/// 槽位 + 自由表的对象池。
///
/// 不变量（phase-03 §6.2 A1–A5）：
/// - `free` 内无重复 index；
/// - 存活槽位的 `generation ∈ 1..=u32::MAX`，释放时立即 `next_generation`；
/// - 陈旧句柄的 `get`/`get_mut`/`remove` 一律 `None`，**不得**命中复用后的新值。
#[derive(Debug)]
pub(crate) struct Arena<T, K: ArenaKey = NodeId> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    /// K 只出现在方法签名里（键由 insert_with 的回调铸出），因此必须留一个 PhantomData。
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
            _marker: std::marker::PhantomData,
        }
    }

    /// 插入并由 arena 铸出 id。
    ///
    /// 用回调而不是「先插入再回填」：节点要把握手 id 存进自己的字段（`Node.id`），回调形式让
    /// `id` 与 `value` 同时可用，避免出现「id 尚未写入」的中间态。
    pub(crate) fn insert_with(&mut self, make: impl FnOnce(K) -> T) -> K {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            debug_assert!(slot.value.is_none(), "free list 指向了存活槽位");
            let key = K::from_parts(index, slot.generation);
            slot.value = Some(make(key));
            key
        } else {
            let index = u32::try_from(self.slots.len()).expect("arena 槽位溢出 u32");
            let key = K::from_parts(index, 1);
            self.slots.push(Slot {
                generation: 1,
                value: Some(make(key)),
            });
            key
        }
    }

    /// 释放并返回旧值；陈旧句柄 → `None`（幂等）。
    pub(crate) fn remove(&mut self, key: K) -> Option<T> {
        let (index, generation) = key.parts();
        let slot = self.slots.get_mut(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        let value = slot.value.take()?;
        slot.generation = next_generation(slot.generation);
        self.free.push(index);
        Some(value)
    }

    pub(crate) fn get(&self, key: K) -> Option<&T> {
        let (index, generation) = key.parts();
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.value.as_ref()
    }

    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut T> {
        let (index, generation) = key.parts();
        let slot = self.slots.get_mut(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.value.as_mut()
    }

    /// 遍历所有存活值（消费者：`Scene::clear_frame`）。
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|slot| slot.value.as_mut())
    }

    /// 存活数（消费者：`Scene::lane_count`）。
    ///
    /// 恒等于「已分配槽位数 − 自由表长度」，因此是 O(1)，不需要额外计数字段。
    pub(crate) fn len(&self) -> usize {
        self.slots.len().saturating_sub(self.free.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{Arena, NodeId, next_generation};

    #[test]
    fn node_id_is_8_bytes() {
        assert_eq!(size_of::<NodeId>(), 8);
    }

    #[test]
    fn node_id_to_bits_layout() {
        let id = NodeId::for_test(3, 7);

        assert_eq!(id.to_bits(), (7u64 << 32) | 3);
        assert_eq!(id.index(), 3);
        assert_eq!(id.generation(), 7);
        assert_eq!(size_of::<NodeId>(), size_of::<u64>());
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
    fn arena_generation_skips_zero_on_wrap() {
        assert_eq!(next_generation(u32::MAX), 1, "回绕不得产生 0");
        assert_eq!(next_generation(1), 2);
        assert_eq!(next_generation(u32::MAX - 1), u32::MAX);
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
