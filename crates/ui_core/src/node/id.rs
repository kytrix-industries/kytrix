//! `NodeId`（带窗口归属与代际的句柄）与 `Arena`（槽位 + 自由表 + 代际的对象池）。
//!
//! 设计见 `design/phase-03-node-state.md` §6。

use std::{fmt, num::NonZeroU32};

use crate::window::WindowId;

/// 带窗口归属与代际的节点句柄（12 B）。
///
/// 字段私有：句柄只能由 `Arena` 铸出，应用层无法用结构体字面量伪造（P02 S-20 的同类理由）。
/// `generation` 防悬垂：节点卸载后旧句柄的 `get`/`get_mut` 一律返回 `None` → `EngineError::StaleNode`。
///
/// # 窗口作用域（R03-7）
///
/// 节点的 arena 是**每窗口一份**，槽位下标因此只在窗口内有意义：A 窗的 `n3.1` 与 B 窗的
/// `n3.1` 是两个不同节点，而 8 B 的 `(index, generation)` 无法区分它们 —— 旧实现里把一个
/// 窗口的节点句柄传给另一个窗口的 API 会**静默命中另一个节点**（命中/焦点/滚动全错）。
///
/// `scope` 记录铸造该句柄的窗口（`WindowId::to_bits()`；**0 = 未绑定**）。查找规则：
/// - 句柄带窗口 ⇒ 只在该窗口的树里有效（跨窗口一律 `StaleNode`，**确定性检测**）；
/// - 句柄未绑定（0）⇒ 通配（tree-local 编码用：taffy `NodeId` 往返、a11y 身份、
///   `NodeId::for_test`）—— 这些编码只存在于**单个树内部**，不存在跨窗口歧义。
///
/// `generation` 用 `NonZeroU32` 表示「**0 永不是合法代际**」这条不变量：类型即证明，并且让
/// `Option<NodeId>` 保持 12 B（`Node.parent` 因此零开销）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    index: u32,
    scope: u32,
    generation: NonZeroU32,
}

impl NodeId {
    /// arena 槽位下标（窗口内稠密）。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 代际，恒在 `1..=u32::MAX`（0 永不是合法代际，见 `next_generation`）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// 该句柄的窗口归属（`None` = 未绑定：只用于 tree-local 编码 / 测试构造）。
    #[must_use]
    pub fn window(self) -> Option<WindowId> {
        (self.scope != UNBOUND_SCOPE).then(|| WindowId::from_raw(self.scope))
    }

    /// 窗口归属的原始值（诊断 / 编码：`0` = 未绑定）。
    #[must_use]
    pub const fn window_tag(self) -> u32 {
        self.scope
    }

    /// `(generation as u64) << 32 | index as u64`（**tree-local** 编码）。
    ///
    /// 消费者：结构化日志/帧记录需要单值可打印的节点标识（设计 §9.1）；同一节点的历次代际在数值上连续递增。
    /// P04 起它同时是 taffy `NodeId` 的编码：窗口归属**不**进入编码（taffy id 只在单棵树内使用），
    /// `from_bits` 因此返回未绑定句柄（通配）。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// `NodeId::to_bits` 的逆运算；代际为 0 的编码非法（返回 `None`）。
    ///
    /// 消费者：`layout::taffy_tree::to_engine`（taffy 回调里把 `taffy::NodeId` 换回引擎句柄）、
    /// `a11y` 的稳定身份编码。
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

    /// 给句柄补上窗口归属（消费者：`NodeTree::bind_window` 重贴 root）。
    pub(crate) const fn with_window_tag(mut self, scope: u32) -> Self {
        self.scope = scope;
        self
    }

    /// 仅测试用的构造器（不扩大公开面，S-20 的形态）。窗口归属为**未绑定**（通配）。
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

/// 未绑定窗口的 scope 值（通配）。
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

/// arena 键：`NodeId` / `LaneId` / `ResourceId` 共用同一套槽位/自由表/代际规则。
pub(crate) trait ArenaKey: Copy {
    fn from_parts(index: u32, generation: u32) -> Self;
    fn parts(self) -> (u32, u32);

    /// 该键的窗口作用域（`0` = 无作用域/通配）。只有 `NodeId` 覆写。
    fn scope(self) -> u32 {
        let _ = self;
        UNBOUND_SCOPE
    }

    /// 给键补上窗口作用域（默认原样返回；只有 `NodeId` 覆写）。
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

/// 代际固定为 `1..=u32::MAX`：**0 永不是合法代际**。
///
/// 返回 `None` = 该槽位的代际空间**已用尽**（`u32::MAX` 之后无值可分配给新句柄）。
/// 用尽的槽位被**永久退休**（不再进入自由表）：旧句柄因此永远不会"复活"（ABA），
/// 代价是每个槽位在 2^32−1 次复用后泄漏一个槽位（一个 `Node` 的存储）。
///
/// 旧实现把 `u32::MAX → 1` 回绕，注释声称"回绕也不会复活旧句柄"——**这是错的**：
/// 回绕后的代际与早已发放过的旧句柄逐位相同，`get` 会把旧句柄命中的新节点返回（R03-8 / §5.8）。
pub(crate) fn next_generation(generation: u32) -> Option<u32> {
    generation.checked_add(1)
}

/// 槽位 + 自由表的对象池。
///
/// 不变量（phase-03 §6.2 A1–A5）：
/// - `free` 内无重复 index；
/// - 存活槽位的 `generation ∈ 1..=u32::MAX`，释放时立即 `next_generation`；
/// - 陈旧句柄的 `get`/`get_mut`/`remove` 一律 `None`，**不得**命中复用后的新值；
/// - **代际用尽的槽位永久退休**：`next_generation == None` 时槽位标 `generation = 0`
///   （非法代际 ⇒ 任何句柄都命中不了）且**不进自由表** —— 句柄不会复活（A5 / R03-8）。
#[derive(Debug)]
pub(crate) struct Arena<T, K: ArenaKey = NodeId> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    /// 因代际用尽而永久退休的槽位数（诊断：句柄空间的耗尽是可观测的）。
    retired: usize,
    /// 该 arena 的窗口作用域（`0` = 无作用域；`NodeTree` 在绑定窗口后写入 `WindowId::to_bits()`）。
    ///
    /// 键的作用域为 0（未绑定）时通配；否则必须逐位相等。
    scope: u32,
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
            retired: 0,
            scope: UNBOUND_SCOPE,
            _marker: std::marker::PhantomData,
        }
    }

    /// 该 arena 的作用域（窗口 tag；`0` = 未绑定）。消费者：`NodeTree::window_tag`。
    pub(crate) fn scope(&self) -> u32 {
        self.scope
    }

    /// 绑定作用域（消费者：`NodeTree::bind_window`）。
    ///
    /// 只允许在「最多铸出过根节点」时调用：根句柄由 `NodeTree::bind_window` 负责重贴，
    /// 其它槽位（若有）的键无从修补。
    pub(crate) fn set_scope(&mut self, scope: u32) {
        debug_assert!(
            self.slots.len() <= 1,
            "arena 的作用域只能在铸造句柄之前绑定"
        );
        self.scope = scope;
    }

    /// 键是否属于本 arena（未绑定键通配）。
    fn accept(&self, key: K) -> bool {
        let scope = key.scope();
        scope == UNBOUND_SCOPE || scope == self.scope
    }

    /// 插入并由 arena 铸出 id。
    ///
    /// 用回调而不是「先插入再回填」：节点要把握手 id 存进自己的字段（`Node.id`），回调形式让
    /// `id` 与 `value` 同时可用，避免出现「id 尚未写入」的中间态。
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

    /// 释放并返回旧值；陈旧句柄（含跨窗口句柄）→ `None`（幂等）。
    ///
    /// 代际用尽（`u32::MAX` 之后）⇒ 槽位**永久退休**：`generation` 置 0（非法代际），
    /// 不进自由表。句柄空间因此不会 ABA 复活旧句柄（A5）。
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

    /// 遍历所有存活值（消费者：`Scene::clear_frame`）。
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|slot| slot.value.as_mut())
    }

    /// 存活数（消费者：`Scene::lane_count`）。
    ///
    /// 恒等于「已分配槽位数 − 自由表长度 − 退休槽位数」，因此是 O(1)，不需要额外计数字段。
    pub(crate) fn len(&self) -> usize {
        self.slots
            .len()
            .saturating_sub(self.free.len() + self.retired)
    }

    /// 因代际用尽而永久退休的槽位数（诊断；正常寿命内恒为 0）。
    #[cfg(test)]
    pub(crate) fn retired_slots(&self) -> usize {
        self.retired
    }

    /// 仅测试：把某个存活槽位的代际推到 `u32::MAX`（模拟「用了 40 亿次」，验证退休路径）。
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
        // 8 B（index + generation）→ 12 B：wave-6 起句柄带窗口身份（R03-7），
        // `Option<NodeId>` 靠 NonZeroU32 的 niche 保持 12 B（不额外膨胀）。
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
        // A5：u32::MAX 之后没有合法代际 ⇒ 槽位退休，绝不回绕到 1（回绕会让旧句柄复活）。
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

        // 退休槽位不进自由表：新节点用**新**槽位，绝不复用第一个 index。
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
