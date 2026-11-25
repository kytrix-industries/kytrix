use crate::{
    a11y::{A11yDiagnostic, A11yNodeId},
    node::{id::NodeId, tree::NodeTree},
};

pub(crate) const A11Y_ID_KEY_TTL: u32 = 64;

pub(crate) const A11Y_ID_KEY_CAPACITY: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum A11yKey {
    App(u64),
    VirtualRow { list: NodeId, index: usize },
    TextRun { input: NodeId },
}

#[derive(Clone, Copy, Debug)]
struct KeyBinding {
    id: u64,
    seen: u32,
}

pub(crate) struct A11yIds {
    next: u64,
    epoch: u32,
    by_engine: ahash::HashMap<NodeId, u64>,
    by_key: ahash::HashMap<A11yKey, KeyBinding>,
    by_id: ahash::HashMap<u64, NodeId>,
    virtual_ids: ahash::HashMap<u64, NodeId>,
}

impl A11yIds {
    pub(crate) fn new() -> Self {
        Self {
            next: 1,
            epoch: 0,
            by_engine: ahash::HashMap::default(),
            by_key: ahash::HashMap::default(),
            by_id: ahash::HashMap::default(),
            virtual_ids: ahash::HashMap::default(),
        }
    }

    pub(crate) fn begin_delivery(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

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

    pub(crate) fn id_for_text_run(&mut self, input: NodeId) -> A11yNodeId {
        let key = A11yKey::TextRun { input };
        if let Some(binding) = self.by_key.get(&key).copied() {
            self.observe_key(key, binding.id);
            self.bind_virtual(binding.id, input);
            return A11yNodeId::from_stable(binding.id);
        }
        let id = self.next;
        self.next += 1;
        self.observe_key(key, id);
        self.bind_virtual(id, input);
        A11yNodeId::from_stable(id)
    }

    pub(crate) fn engine_of(&self, id: A11yNodeId) -> Option<NodeId> {
        self.by_id.get(&id.get()).copied()
    }

    pub(crate) fn stable_of(&self, node: NodeId) -> Option<A11yNodeId> {
        self.by_engine
            .get(&node)
            .copied()
            .map(A11yNodeId::from_stable)
    }

    #[cfg(test)]
    pub(crate) fn sizes(&self) -> (usize, usize, usize) {
        (self.by_engine.len(), self.by_key.len(), self.by_id.len())
    }

    pub(crate) fn prune(&mut self, tree: &NodeTree) {
        self.by_engine.retain(|node, _| tree.get(*node).is_ok());
        self.virtual_ids.retain(|_, owner| tree.get(*owner).is_ok());
        self.by_id.clear();
        self.by_id
            .reserve(self.by_engine.len() + self.virtual_ids.len());
        for (node, id) in &self.by_engine {
            self.by_id.insert(*id, *node);
        }
        for (id, owner) in &self.virtual_ids {
            self.by_id.insert(*id, *owner);
        }
        self.by_key.retain(|key, binding| {
            self.epoch.wrapping_sub(binding.seen) < A11Y_ID_KEY_TTL
                || matches!(key, A11yKey::TextRun { input } if tree.get(*input).is_ok())
        });
        if self.by_key.len() > A11Y_ID_KEY_CAPACITY {
            let mut seen: Vec<(u32, A11yKey)> = self
                .by_key
                .iter()
                .filter(|(key, _)| {
                    !matches!(key, A11yKey::TextRun { input } if tree.get(*input).is_ok())
                })
                .map(|(key, b)| (b.seen, *key))
                .collect();
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

    fn bind_virtual(&mut self, id: u64, owner: NodeId) {
        self.by_id.insert(id, owner);
        self.virtual_ids.insert(id, owner);
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
    use super::{A11Y_ID_KEY_CAPACITY, A11Y_ID_KEY_TTL, A11yIds, A11yKey};
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

    #[test]
    fn a_virtual_text_run_keeps_its_id_and_resolves_actions_to_its_input() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = mount(&mut tree, root);
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        let input_id = ids.id_for(&tree, input, None, &mut diagnostics);
        let run = ids.id_for_text_run(input);
        assert_ne!(run, input_id, "虚拟子节点必须有自己的稳定 id");
        assert_eq!(
            ids.engine_of(run),
            Some(input),
            "虚拟 id 的动作/状态必须解析回输入节点"
        );
        for _ in 0..5 {
            ids.begin_delivery();
            ids.prune(&tree);
            assert_eq!(ids.id_for_text_run(input), run, "虚拟 id 跨帧必须稳定");
        }

        tree.remove(input).expect("unmount input");
        ids.begin_delivery();
        ids.prune(&tree);
        assert_eq!(ids.engine_of(run), None, "输入卸载后虚拟 id 必须失效");
        let rebuilt = mount(&mut tree, root);
        assert_ne!(
            ids.id_for_text_run(rebuilt),
            run,
            "旧虚拟 id 不得复用给新输入"
        );
    }

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

    #[test]
    fn a_mounted_text_run_key_survives_more_than_the_id_key_ttl() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = mount(&mut tree, root);
        let other = mount(&mut tree, root);
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        let run = ids.id_for_text_run(input);
        let _ = ids.id_for(&tree, other, None, &mut diagnostics);
        for _ in 0..(A11Y_ID_KEY_TTL + 137) {
            ids.begin_delivery();
            ids.prune(&tree);
            let _ = ids.id_for(&tree, other, None, &mut diagnostics);
        }
        assert_eq!(
            ids.id_for_text_run(input),
            run,
            "挂载中的 TextRun 键不得被 TTL 淘汰（否则 AT 会周期性看到节点重建）"
        );
        assert_eq!(ids.engine_of(run), Some(input));
        let (_, keys, _) = ids.sizes();
        assert!(
            keys <= 2,
            "旁表不得随交付轮数累积：{keys} > A11Y_ID_KEY_TTL={A11Y_ID_KEY_TTL}"
        );
    }
}
