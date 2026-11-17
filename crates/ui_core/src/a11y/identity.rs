//! a11y 节点的**稳定身份**分配器（与渲染 `NodeId` 解耦）。
//!
//! AccessKit 的 `TreeUpdate` 是差分协议：屏幕阅读器（NVDA/讲述人/VoiceOver/Orca）维护的是
//! "当前元素对象"，同一个语义元素跨帧必须是同一个 `accesskit::NodeId`，否则 AT 认为它被移除并
//! 重建 —— 读屏跳读/重读、阅读位置与焦点丢失。引擎的 arena `NodeId`（`(generation << 32) | index`）
//! 会随槽位复用与虚拟列表回收而变化，**不能**用作 a11y 身份（R13-1 / 审查 §5.7）。
//!
//! 因此这里维护一份窗口级（引擎侧即 App 级）分配器：
//!
//! - `by_engine`：引擎句柄 → 稳定 id。普通节点跨帧不变。
//! - `by_key`：**逻辑键** → 稳定 id。虚拟列表的行在回收/重建后拿到同一 id（行滚出再滚回，
//!   AT 认为"同一行回来了"，而不是"一行消失 + 一行新增"）；应用也可以用
//!   [`crate::a11y::A11yOverride::key`] 声明自己的逻辑键。
//! - `by_id`：稳定 id → 当前引擎句柄（`ActionRequest` 的解码表；键为行 id 时绑定到当前挂载的
//!   引擎节点）。
//!
//! id 从 1 单调递增、**永不复用**（`0` 保留给 AccessKit 的"无节点"），所以"删掉 A 再加 B"不会
//! 让 B 顶替 A 的身份。逻辑键的绑定有 TTL（见 [`A11Y_ID_KEY_TTL`]）与容量上限
//! （[`A11Y_ID_KEY_CAPACITY`]），长时间滚动的超长列表不会让旁表无界增长；**仍然挂载**的行
//! 每次交付都会刷新 `seen`，其身份不受清理影响。

use crate::{
    a11y::{A11yDiagnostic, A11yNodeId},
    node::{id::NodeId, tree::NodeTree},
};

/// 逻辑键绑定在多少笔 a11y 交付之后被回收（未被再次看见的键）。
///
/// 64 笔 ≈ 6.4 s（按 `A11Y_THROTTLE` 的 100 ms 上限）：虚拟列表把行滚出视口再滚回来的时间远
/// 小于这个窗口，身份仍然连续；被回收的是"很久没出现过"的键，回收后同一个逻辑行会拿到新 id
/// （AT 视为新增行 —— 可接受，因为它确实离开语义视口很久了）。
pub(crate) const A11Y_ID_KEY_TTL: u32 = 64;

/// `by_key` 的容量上限：超过后按 `seen` 由旧到新淘汰一半。
///
/// 65536 ≈ 一张 6.5 万行的表格的全部逻辑行；再大就需要应用自己给 `A11yOverride::key`
/// 收窄键空间（而不是让引擎无限记忆）。
pub(crate) const A11Y_ID_KEY_CAPACITY: usize = 65_536;

/// 逻辑身份键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum A11yKey {
    /// 应用显式声明的键（`A11yOverride::key` / `A11yPatch::key`）。
    App(u64),
    /// 虚拟列表里的一行：`(容器引擎句柄, 条目下标)`。
    ///
    /// 容器句柄本身由应用创建并长期存活（行是它的子节点），因此 `(list, index)` 在行的
    /// 回收/重建之间是稳定的。
    VirtualRow {
        /// 容器节点的引擎句柄。
        list: NodeId,
        /// 条目下标（0 基于）。
        index: usize,
    },
}

/// 一个逻辑键的绑定（id + 最近一次被看见的交付轮次）。
#[derive(Clone, Copy, Debug)]
struct KeyBinding {
    id: u64,
    seen: u32,
}

/// 稳定 id 分配器。
pub(crate) struct A11yIds {
    /// 下一个可用 id（`0` 不用）。
    next: u64,
    /// 交付轮次（`begin_delivery` 递增；逻辑键的 TTL 以此计）。
    epoch: u32,
    /// 引擎句柄 → 稳定 id（只含当前树里存活的节点；`prune` 时清理）。
    by_engine: ahash::HashMap<NodeId, u64>,
    /// 逻辑键 → 稳定 id。
    by_key: ahash::HashMap<A11yKey, KeyBinding>,
    /// 稳定 id → 当前引擎句柄（动作解码；只含存活绑定）。
    by_id: ahash::HashMap<u64, NodeId>,
}

impl A11yIds {
    /// 空分配器。
    pub(crate) fn new() -> Self {
        Self {
            next: 1,
            epoch: 0,
            by_engine: ahash::HashMap::default(),
            by_key: ahash::HashMap::default(),
            by_id: ahash::HashMap::default(),
        }
    }

    /// 开始一笔交付（刷新逻辑键 TTL 的计时基准）。
    pub(crate) fn begin_delivery(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// 节点的稳定 id（必要时分配）。
    ///
    /// `key` 是调用方算好的逻辑键（应用 key 优先、虚拟列表行其次；`None` = 普通节点）。
    /// 两个**同时存活**的节点声明同一个逻辑键是应用错误：后到的节点拿新 id（不顶替前者），
    /// 并记一条 [`A11yDiagnostic::DuplicateKey`]。
    pub(crate) fn id_for(
        &mut self,
        tree: &NodeTree,
        node: NodeId,
        key: Option<A11yKey>,
        diagnostics: &mut Vec<A11yDiagnostic>,
    ) -> A11yNodeId {
        if let Some(id) = self.by_engine.get(&node).copied() {
            if let Some(key) = key {
                self.observe_key(key, id);
            }
            return A11yNodeId::from_stable(id);
        }
        if let Some(key) = key
            && let Some(binding) = self.by_key.get(&key).copied()
        {
            let live_other = self
                .by_id
                .get(&binding.id)
                .copied()
                .filter(|other| *other != node && tree.get(*other).is_ok());
            if live_other.is_none() {
                self.bind(binding.id, node);
                self.observe_key(key, binding.id);
                return A11yNodeId::from_stable(binding.id);
            }
            diagnostics.push(A11yDiagnostic::DuplicateKey { node });
        }
        let id = self.next;
        self.next += 1;
        if let Some(key) = key {
            self.observe_key(key, id);
        }
        self.bind(id, node);
        A11yNodeId::from_stable(id)
    }

    /// 稳定 id → 当前引擎句柄（`ActionRequest` 解码）。未知 / 已卸载 ⇒ `None`。
    pub(crate) fn engine_of(&self, id: A11yNodeId) -> Option<NodeId> {
        self.by_id.get(&id.get()).copied()
    }

    /// 引擎句柄 → 稳定 id（**只查不分配**；测试 / 诊断用）。
    pub(crate) fn stable_of(&self, node: NodeId) -> Option<A11yNodeId> {
        self.by_engine
            .get(&node)
            .copied()
            .map(A11yNodeId::from_stable)
    }

    /// 旁表规模（`(引擎绑定, 逻辑键, id 绑定)`；测试用）。
    #[cfg(test)]
    pub(crate) fn sizes(&self) -> (usize, usize, usize) {
        (self.by_engine.len(), self.by_key.len(), self.by_id.len())
    }

    /// 清理已卸载节点与过期逻辑键。
    ///
    /// **只允许在"已发出的树仍自洽"的时机调用**：全树笔（`TreeInfo` 会重置消费端的树）与
    /// 容量超限时。增量笔里删掉一个 `by_engine` 条目会让该节点的 `ActionRequest` 变成
    /// `StaleAction`，这是正确的（节点确实没了），但会让"同一笔里父节点重发"的时序变脆，
    /// 所以日常增量不做。
    pub(crate) fn prune(&mut self, tree: &NodeTree) {
        self.by_engine.retain(|node, _| tree.get(*node).is_ok());
        self.by_id.clear();
        self.by_id.reserve(self.by_engine.len());
        for (node, id) in &self.by_engine {
            self.by_id.insert(*id, *node);
        }
        self.by_key
            .retain(|_, binding| self.epoch.wrapping_sub(binding.seen) < A11Y_ID_KEY_TTL);
        if self.by_key.len() > A11Y_ID_KEY_CAPACITY {
            let mut seen: Vec<(u32, A11yKey)> =
                self.by_key.iter().map(|(key, b)| (b.seen, *key)).collect();
            seen.sort_unstable_by_key(|(seen, _)| *seen);
            let drop_count = seen.len() / 2;
            for (_, key) in seen.into_iter().take(drop_count) {
                self.by_key.remove(&key);
            }
        }
    }

    fn bind(&mut self, id: u64, node: NodeId) {
        if let Some(previous) = self.by_id.insert(id, node)
            && previous != node
        {
            self.by_engine.remove(&previous);
        }
        self.by_engine.insert(node, id);
    }

    fn observe_key(&mut self, key: A11yKey, id: u64) {
        self.by_key.insert(
            key,
            KeyBinding {
                id,
                seen: self.epoch,
            },
        );
    }
}

impl Default for A11yIds {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{A11Y_ID_KEY_CAPACITY, A11yIds, A11yKey};
    use crate::{
        a11y::A11yDiagnostic,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    };

    fn mount(tree: &mut NodeTree, parent: NodeId) -> NodeId {
        NodeBuilder::boxed().build(tree, parent).expect("mount")
    }

    fn id_of(ids: &mut A11yIds, tree: &NodeTree, node: NodeId) -> u64 {
        let mut diagnostics = Vec::new();
        ids.id_for(tree, node, None, &mut diagnostics).get()
    }

    /// 同一引擎节点跨多笔交付 id 不变（AT 的"当前元素"不会漂移）。
    #[test]
    fn the_same_node_keeps_its_id_across_deliveries() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = mount(&mut tree, root);
        let mut ids = A11yIds::new();
        let first = id_of(&mut ids, &tree, button);
        for _ in 0..5 {
            ids.begin_delivery();
            ids.prune(&tree);
            assert_eq!(
                id_of(&mut ids, &tree, button),
                first,
                "同一节点跨帧 id 必须不变"
            );
        }
    }

    /// 不同节点一定拿不同 id（含"删掉 A 再加 B"：B 不得复用 A 的 id）。
    #[test]
    fn distinct_nodes_never_share_an_id() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = mount(&mut tree, root);
        let b = mount(&mut tree, root);
        let mut ids = A11yIds::new();
        let id_a = id_of(&mut ids, &tree, a);
        let id_b = id_of(&mut ids, &tree, b);
        assert_ne!(id_a, id_b);

        tree.remove(a).expect("remove a");
        ids.prune(&tree);
        let c = mount(&mut tree, root);
        let id_c = id_of(&mut ids, &tree, c);
        assert_ne!(
            id_c, id_a,
            "被删节点的 id 不得被新节点复用（否则 AT 认错元素）"
        );
        assert_ne!(id_c, id_b);
    }

    /// 逻辑键：行的引擎句柄被 `remove` + `build` 重建后，同一逻辑行仍是同一 id。
    #[test]
    fn a_logical_row_keeps_its_id_after_being_rebuilt() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = mount(&mut tree, root);
        let mut ids = A11yIds::new();
        let key = A11yKey::VirtualRow { list, index: 42 };
        let mut diagnostics = Vec::new();
        let row = mount(&mut tree, list);
        let first = ids.id_for(&tree, row, Some(key), &mut diagnostics).get();

        tree.remove(row).expect("recycle row");
        ids.begin_delivery();
        ids.prune(&tree);
        assert!(
            ids.engine_of(crate::a11y::A11yNodeId::from_stable(first))
                .is_none(),
            "行卸载后 id 不得再解析到引擎节点（动作必须判陈旧）"
        );

        let rebuilt = mount(&mut tree, list);
        let again = ids
            .id_for(&tree, rebuilt, Some(key), &mut diagnostics)
            .get();
        assert_eq!(again, first, "滚出再滚回的行必须拿回同一 id");
        assert_eq!(
            ids.engine_of(crate::a11y::A11yNodeId::from_stable(again)),
            Some(rebuilt),
            "动作解码必须指向当前挂载的引擎节点"
        );
    }

    /// 两个同时存活的节点声明同一逻辑键：后到者拿新 id（不顶替前者）+ 诊断。
    #[test]
    fn duplicate_live_keys_do_not_steal_identity() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = mount(&mut tree, root);
        let key = A11yKey::VirtualRow { list, index: 7 };
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        let first_row = mount(&mut tree, list);
        let first = ids
            .id_for(&tree, first_row, Some(key), &mut diagnostics)
            .get();
        let second_row = mount(&mut tree, list);
        let second = ids
            .id_for(&tree, second_row, Some(key), &mut diagnostics)
            .get();
        assert_ne!(second, first);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, A11yDiagnostic::DuplicateKey { .. })),
            "键冲突必须留诊断，不能静默"
        );
    }

    /// 过期逻辑键会被 TTL 回收；一直挂载的行不会被回收。
    #[test]
    fn idle_keys_expire_but_mounted_rows_do_not() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = mount(&mut tree, root);
        let mounted = mount(&mut tree, list);
        let gone = mount(&mut tree, list);
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        let mounted_id = ids
            .id_for(
                &tree,
                mounted,
                Some(A11yKey::VirtualRow { list, index: 0 }),
                &mut diagnostics,
            )
            .get();
        let gone_id = ids
            .id_for(&tree, gone, Some(A11yKey::App(9)), &mut diagnostics)
            .get();
        tree.remove(gone).expect("unmount the keyed row");
        for _ in 0..super::A11Y_ID_KEY_TTL {
            ids.begin_delivery();
            ids.prune(&tree);
            // 仍然挂载的行每笔都被"看见"。
            ids.id_for(
                &tree,
                mounted,
                Some(A11yKey::VirtualRow { list, index: 0 }),
                &mut diagnostics,
            );
        }
        ids.begin_delivery();
        ids.prune(&tree);
        assert_eq!(
            ids.stable_of(gone).map(crate::a11y::A11yNodeId::get),
            None,
            "长期未出现的键绑定必须被回收（旁表不得无界增长）"
        );
        assert_eq!(
            ids.stable_of(mounted).map(crate::a11y::A11yNodeId::get),
            Some(mounted_id),
            "仍然挂载的行不能被 TTL 误伤"
        );
        assert_ne!(gone_id, mounted_id);
    }

    /// `by_key` 有硬容量上限（超限按最旧淘汰）。
    #[test]
    fn key_bindings_are_capped() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = mount(&mut tree, root);
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        for index in 0..(A11Y_ID_KEY_CAPACITY + 64) {
            let row = mount(&mut tree, list);
            ids.id_for(
                &tree,
                row,
                Some(A11yKey::VirtualRow { list, index }),
                &mut diagnostics,
            );
            tree.remove(row).expect("unmount");
        }
        ids.begin_delivery();
        ids.prune(&tree);
        let (_, keys, _) = ids.sizes();
        assert!(
            keys <= A11Y_ID_KEY_CAPACITY,
            "逻辑键旁表必须有硬上限：{keys}"
        );
    }
}
