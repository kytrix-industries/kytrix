//! 持久节点树：`Node` / `NodeKind` / `LayoutBox` / `NodeTree`。
//!
//! 设计见 `design/phase-03-node-state.md` §11。要点：
//! - 唯一的一棵树（I5）：节点树 + arena（`NodeId` 带代际）+ DFS 先序数组 `order`；
//! - 子树在 `order` 里是**连续块**，因此结构变更只做局部拼接（`Vec::insert`/`drain` 的一段）；
//! - 所有写路径都走 `mark_*`（§9.3 的表），`touch` 顺带并 damage 并清 `REUSABLE`；
//! - `taffy::Cache` **不**内联（P04 在 `NodeTree` 旁建旁表，见 §11.1）。

use std::fmt;

use smallvec::SmallVec;

use crate::{
    error::EngineError,
    math::Rect,
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, affects_parent, depends_on_children},
        id::{Arena, NodeId},
        signal::SignalId,
    },
    paint::{
        canvas::Canvas,
        geometry::GeometryId,
        scene::LaneId,
        style::{Style, StylePatch},
    },
};

/// 逻辑像素的布局结果（只记录 AABB）。
///
/// P03 只声明与搬运它（`layout` + `prev_layout` + `damage`）；写入者是 P04 的 `set_layout`。
/// `Copy`、16 B：damage 计算只需两次并集，不必借用。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutBox {
    /// 相对窗口左上角的逻辑像素 AABB（`w`/`h` 可为 0）。
    pub bounds: Rect,
}

/// 节点的种类（决定叶子性与绘制路径）。
///
/// 内部按平台无关的几何/画布形式存放（尚无平台窗口）。
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// 普通容器/叶子：由引擎按 `Style`/`StylePatch` 生成图元。
    Box,
    /// 自绘节点：挂载时提交的画布内容（P02 的 `Canvas`）。
    Canvas(Canvas),
    /// 静态几何节点：引用 P09 的 GPU 几何。
    Geometry(GeometryId),
}

impl NodeKind {
    /// 叶子节点（`Canvas`/`Geometry`）不布局子节点 —— 消费者：`depends_on_children`、`NodeBuilder::build` 的 debug 断言。
    #[must_use]
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(self, Self::Canvas(_) | Self::Geometry(_))
    }
}

/// 一个持久节点（跨帧存活）。**字段全私有**（I6：应用层不得持 `&mut Node`）。
pub struct Node {
    id: NodeId,
    parent: Option<NodeId>,
    children: SmallVec<[NodeId; 4]>,
    kind: NodeKind,
    /// 静态结构（挂载时一次写入，之后只有结构变更才重写）。
    style: Style,
    /// 动态属性补丁（`bind_style` 的**上次求值**结果）。
    style_patch: StylePatch,
    /// 动态属性闭包（`None` = 静态节点）。
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    /// 当前布局盒（逻辑像素；P04 写）。
    layout: LayoutBox,
    /// 上一次布局盒（damage 的「旧」一侧）。
    prev_layout: LayoutBox,
    /// 本帧累积的逻辑 damage AABB（§9.4）。
    damage: Rect,
    /// 该节点的绘制输出桶（按绘制序；P06 的 paint 分配）。
    lanes: SmallVec<[LaneId; 2]>,
    /// 上次求值实际读过的信号（差分清理用；`replace_deps` 的 `old`）。
    deps: SmallVec<[SignalId; 8]>,
    dirty: DirtyFlags,
}

/// **手写 `Debug`**（N4）：`bind_style` 是 `Box<dyn Fn>`（不实现 `Debug`），闭包位固定打印 `<closure>`。
impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.id)
            .field("parent", &self.parent)
            .field("children", &self.children)
            .field("kind", &self.kind)
            .field("style", &self.style)
            .field("style_patch", &self.style_patch)
            .field("bind_style", &self.bind_style.as_ref().map(|_| "<closure>"))
            .field("layout", &self.layout)
            .field("prev_layout", &self.prev_layout)
            .field("damage", &self.damage)
            .field("lanes", &self.lanes)
            .field("deps", &self.deps)
            .field("dirty", &self.dirty)
            .finish()
    }
}

impl Node {
    /// 节点的种类。
    #[must_use]
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    /// 静态样式。
    #[must_use]
    pub fn style(&self) -> &Style {
        &self.style
    }

    /// 上次求值的动态属性补丁（消费者：`App::refresh_styles`、`App::write` 的 `affects_layout`）。
    #[must_use]
    pub fn style_patch(&self) -> &StylePatch {
        &self.style_patch
    }

    /// 失效集合（消费者：`App::refresh_styles`、`StylePassReport`）。
    #[must_use]
    pub fn dirty(&self) -> DirtyFlags {
        self.dirty
    }

    /// 节点自己的句柄（消费者：P08 的 `Probe`、诊断日志）。
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// 父句柄；`None` = root。
    #[must_use]
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// 直接子节点（按插入序；消费者：P04 的布局遍历、P08 的 `Probe`）。
    #[must_use]
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    /// 本帧累积的 damage AABB（消费者：P06 的 damage 合并）。
    #[must_use]
    pub fn damage(&self) -> Rect {
        self.damage
    }

    /// 当前布局盒（P04 写入；`Probe` 读取）。
    #[must_use]
    pub fn layout(&self) -> LayoutBox {
        self.layout
    }

    /// 上一次布局盒（P04 写入；damage 的「旧」一侧）。
    #[must_use]
    pub fn prev_layout(&self) -> LayoutBox {
        self.prev_layout
    }

    /// 该节点占用的绘制桶（P06 的 paint 分配；`Probe` 读取）。
    #[must_use]
    pub fn lanes(&self) -> &[LaneId] {
        &self.lanes
    }

    /// 把 `prev_layout ∪ layout` 并入 damage（§9.4：位置变化时新旧都要重画）。
    fn accumulate_damage(&mut self) {
        self.damage = self
            .damage
            .union(self.prev_layout.bounds)
            .union(self.layout.bounds);
    }
}

/// 唯一的一棵节点树（I5）。
///
/// `order` 是 DFS 先序数组：任何节点的子树在数组里占**连续**一段，因此结构变更只需拼接一段。
pub struct NodeTree {
    arena: Arena<Node>,
    root: NodeId,
    order: Vec<NodeId>,
}

impl Default for NodeTree {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeTree {
    /// 建一棵只有根节点的树（消费者：`Window::new`）。
    #[must_use]
    pub fn new() -> Self {
        let mut arena: Arena<Node> = Arena::new();
        let root = arena.insert_with(|id| Node {
            id,
            parent: None,
            children: SmallVec::new(),
            kind: NodeKind::Box,
            style: Style::default(),
            style_patch: StylePatch::default(),
            bind_style: None,
            layout: LayoutBox::default(),
            prev_layout: LayoutBox::default(),
            damage: Rect::default(),
            lanes: SmallVec::new(),
            deps: SmallVec::new(),
            dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        });
        Self {
            arena,
            root,
            order: vec![root],
        }
    }

    /// 根句柄（永不失效）。
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// 取节点；陈旧句柄 → `EngineError::StaleNode`。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：节点从未存在或已卸载。
    pub fn get(&self, node: NodeId) -> Result<&Node, EngineError> {
        self.arena.get(node).ok_or(EngineError::StaleNode { node })
    }

    /// DFS 先序句柄数组（`root` 在首位）。消费者：`App::refresh_styles`、`App::close_window`。
    #[must_use]
    pub fn order(&self) -> &[NodeId] {
        &self.order
    }

    /// 把 `child`（连同子树）挂到 `parent` 的末尾；`child` 已有父则先摘下（局部拼接，不重建整树）。
    ///
    /// # Errors
    /// `EngineError::StaleNode`：`parent`/`child` 任一陈旧。
    ///
    /// # Panics (debug)
    /// 形成结构环（`child` 是 `parent` 的祖先或二者相同）：debug 断言失败，release 下**不改结构**。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError> {
        self.get(parent)?;
        self.get(child)?;
        if parent == child || self.is_ancestor(child, parent) {
            debug_assert!(
                false,
                "structural cycle: cannot append an ancestor as a child"
            );
            return Ok(());
        }

        if let Some(old_parent) = self.get(child)?.parent {
            self.order_remove_subtree(child);
            if let Some(p) = self.arena.get_mut(old_parent) {
                p.children.retain(|existing| *existing != child);
            }
            self.mark_structure(old_parent);
            // 整棵被移动的子树都要重画：它的绘制桶顺序（`order`）在拼接后可能改变。
            self.mark_subtree_paint(child);
        }

        self.order_insert(parent, child);
        if let Some(node) = self.arena.get_mut(child) {
            node.parent = Some(parent);
        }
        if let Some(node) = self.arena.get_mut(parent) {
            node.children.push(child);
        }
        self.mark_structure(parent);
        #[cfg(debug_assertions)]
        self.debug_assert_order_matches_tree();
        Ok(())
    }

    /// 卸载 `node` 及其整棵子树（句柄全部变为陈旧）。
    ///
    /// # Errors
    /// `EngineError::StaleNode`：`node` 陈旧，或 `node` 是 root（root 不是可移除的子节点，见 N11）。
    pub fn remove(&mut self, node: NodeId) -> Result<(), EngineError> {
        if node == self.root {
            return Err(EngineError::StaleNode { node });
        }
        let parent = self
            .get(node)?
            .parent
            .expect("non-root node always has a parent");
        self.order_remove_subtree(node);
        if let Some(p) = self.arena.get_mut(parent) {
            p.children.retain(|existing| *existing != node);
        }
        let block = self.collect_subtree_ids(node);
        for id in block.iter().rev() {
            let _ = self.arena.remove(*id);
        }
        self.mark_structure(parent);
        #[cfg(debug_assertions)]
        self.debug_assert_order_matches_tree();
        Ok(())
    }

    /// 铸一个新节点并挂到 `parent` 末尾（消费者：`NodeBuilder::build`）。
    ///
    /// 参数而不是半成品 `Node`：`Node` 的全部字段都是 `node::tree` 私有的，只有本模块能构造。
    pub(crate) fn insert_child(
        &mut self,
        parent: NodeId,
        kind: NodeKind,
        style: Style,
        bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    ) -> Result<NodeId, EngineError> {
        self.get(parent)?;
        let id = self.arena.insert_with(move |id| Node {
            id,
            parent: None,
            children: SmallVec::new(),
            kind,
            style,
            style_patch: StylePatch::default(),
            bind_style,
            layout: LayoutBox::default(),
            prev_layout: LayoutBox::default(),
            damage: Rect::default(),
            lanes: SmallVec::new(),
            deps: SmallVec::new(),
            dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        });
        self.order_insert(parent, id);
        if let Some(node) = self.arena.get_mut(id) {
            node.parent = Some(parent);
        }
        if let Some(node) = self.arena.get_mut(parent) {
            node.children.push(id);
        }
        self.mark_structure(parent);
        #[cfg(debug_assertions)]
        self.debug_assert_order_matches_tree();
        Ok(id)
    }

    /// 取走动态属性闭包（求值期；消费者：`App::refresh_styles`）。
    pub(crate) fn take_bind_style(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>> {
        self.arena.get_mut(node)?.bind_style.take()
    }

    /// 放回动态属性闭包（求值后；panic 展开时闭包丢失 → 该节点退化为静态节点）。
    pub(crate) fn put_bind_style(
        &mut self,
        node: NodeId,
        bind_style: Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>,
    ) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.bind_style = Some(bind_style);
                true
            }
            None => false,
        }
    }

    /// 上次求值的依赖（`replace_deps` 的 `old`；关窗退订）。
    pub(crate) fn deps(&self, node: NodeId) -> Option<&[SignalId]> {
        self.arena.get(node).map(|target| target.deps.as_slice())
    }

    /// 写回本次求值的依赖。
    pub(crate) fn set_deps(&mut self, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.deps = deps;
                true
            }
            None => false,
        }
    }

    /// 写回动态属性补丁。
    pub(crate) fn set_style_patch(&mut self, node: NodeId, patch: StylePatch) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.style_patch = patch;
                true
            }
            None => false,
        }
    }

    /// 标 `PAINT`（不冒泡）。返回 `false` = 句柄陈旧。
    pub(crate) fn mark_paint(&mut self, node: NodeId) -> bool {
        self.touch(node, DirtyFlags::PAINT)
    }

    /// 标 `LAYOUT | PAINT` 并向上冒泡（§9.2 / N7）。
    pub(crate) fn mark_layout(&mut self, node: NodeId) -> bool {
        if !self.touch(node, DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
            return false;
        }
        self.bubble_layout(node);
        true
    }

    /// 标 `A11Y`（不冒泡）。
    pub(crate) fn mark_a11y(&mut self, node: NodeId) -> bool {
        self.touch(node, DirtyFlags::A11Y)
    }

    /// 结构变更：标 `LAYOUT|PAINT|ORDER|A11Y` 并向上冒泡。
    pub(crate) fn mark_structure(&mut self, node: NodeId) -> bool {
        if !self.touch(
            node,
            DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::ORDER | DirtyFlags::A11Y,
        ) {
            return false;
        }
        self.bubble_layout(node);
        true
    }

    /// 消费 `PAINT`（`refresh_styles` 求值完一个节点后调用）。
    pub(crate) fn clear_paint(&mut self, node: NodeId) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.dirty.remove(DirtyFlags::PAINT);
                true
            }
            None => false,
        }
    }

    /// 遍历全部存活节点并测试谓词（消费者：`StylePassReport` 的 `layout_dirty`/`paint_dirty`）。
    pub(crate) fn any_node(&self, mut predicate: impl FnMut(&Node) -> bool) -> bool {
        self.order
            .iter()
            .any(|id| self.arena.get(*id).is_some_and(&mut predicate))
    }

    /// 标记 + damage 并集 + 清 `REUSABLE`（唯一的写入口，§9.3）。
    fn touch(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        let Some(target) = self.arena.get_mut(node) else {
            return false;
        };
        target.dirty.insert(flags);
        if flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
            target.dirty.remove(DirtyFlags::REUSABLE);
            target.accumulate_damage();
        }
        true
    }

    /// `LAYOUT` 的向上冒泡（§9.2 的逐字伪代码）。
    fn bubble_layout(&mut self, from: NodeId) {
        let mut child = from;
        while let Some((parent, affects)) = self
            .arena
            .get(child)
            .map(|target| (target.parent, affects_parent(target)))
        {
            if !affects {
                break;
            }
            let Some(parent) = parent else {
                break;
            };
            let already = self
                .arena
                .get(parent)
                .is_some_and(|target| target.dirty.contains(DirtyFlags::LAYOUT));
            if already && self.arena.get(parent).is_some_and(depends_on_children) {
                break;
            }
            if !self.touch(parent, DirtyFlags::LAYOUT) {
                break;
            }
            if !self.arena.get(parent).is_some_and(depends_on_children) {
                break;
            }
            child = parent;
        }
    }

    /// 整棵子树标 `PAINT`（并清 `REUSABLE`）—— 重挂后绘制桶顺序可能变化（消费者：`append_child`）。
    fn mark_subtree_paint(&mut self, node: NodeId) {
        let ids = self.collect_subtree_ids(node);
        for id in ids {
            let _ = self.touch(id, DirtyFlags::PAINT);
        }
    }

    /// debug-only：`order` 必须是**全序**（DFS 先序、无重复、无遗漏、root 在首位）。
    #[cfg(debug_assertions)]
    fn debug_assert_order_matches_tree(&self) {
        let mut expected = SmallVec::<[NodeId; 32]>::new();
        let mut stack = SmallVec::<[NodeId; 32]>::new();
        stack.push(self.root);
        while let Some(current) = stack.pop() {
            expected.push(current);
            if let Some(target) = self.arena.get(current) {
                for child in target.children.iter().rev() {
                    stack.push(*child);
                }
            }
        }
        debug_assert_eq!(
            expected.as_slice(),
            self.order.as_slice(),
            "order 必须是 DFS 先序全序"
        );
    }

    /// `ancestor` 是否是 `node` 的（严格）祖先。
    fn is_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
        let mut cursor = self.arena.get(node).and_then(|target| target.parent);
        while let Some(current) = cursor {
            if current == ancestor {
                return true;
            }
            cursor = self.arena.get(current).and_then(|target| target.parent);
        }
        false
    }

    /// `node` 在 `order` 里的位置（O(n) 线性扫描；P03 的最简版，见 V-28）。
    fn order_pos(&self, node: NodeId) -> Option<usize> {
        self.order.iter().position(|id| *id == node)
    }

    /// 子树节点数（迭代 DFS，不递归，深树也不爆栈）。
    fn subtree_len(&self, node: NodeId) -> usize {
        let mut count = 0;
        let mut stack = SmallVec::<[NodeId; 16]>::new();
        stack.push(node);
        while let Some(current) = stack.pop() {
            count += 1;
            if let Some(target) = self.arena.get(current) {
                stack.extend(target.children.iter().copied());
            }
        }
        count
    }

    /// 子树的 DFS 先序 id 序列。
    fn collect_subtree_ids(&self, node: NodeId) -> SmallVec<[NodeId; 16]> {
        let mut ids = SmallVec::new();
        let mut stack = SmallVec::<[NodeId; 16]>::new();
        stack.push(node);
        while let Some(current) = stack.pop() {
            ids.push(current);
            if let Some(target) = self.arena.get(current) {
                for child in target.children.iter().rev() {
                    stack.push(*child);
                }
            }
        }
        ids
    }

    /// 把 `child` 的子树块插到 `parent` 的子节点区的**末尾**（局部拼接）。
    fn order_insert(&mut self, parent: NodeId, child: NodeId) {
        let position = {
            let start = self
                .order_pos(parent)
                .map_or(self.order.len(), |pos| pos + 1);
            let mut position = start;
            if let Some(target) = self.arena.get(parent) {
                for existing in &target.children {
                    if *existing != child {
                        position += self.subtree_len(*existing);
                    }
                }
            }
            position
        };
        let block = self.collect_subtree_ids(child);
        for (offset, id) in block.iter().enumerate() {
            self.order.insert(position + offset, *id);
        }
    }

    /// 从 `order` 里删掉 `node` 的子树块（调用前该块必须是连续的）。
    fn order_remove_subtree(&mut self, node: NodeId) {
        let Some(position) = self.order_pos(node) else {
            return;
        };
        let len = self.subtree_len(node);
        self.order.drain(position..position + len);
    }

    /// 仅测试：清掉全部节点的失效位（让「标了什么」可被观察）。
    #[cfg(test)]
    pub(crate) fn clear_all_dirty_for_test(&mut self) {
        for id in &self.order {
            if let Some(target) = self.arena.get_mut(*id) {
                target.dirty = DirtyFlags::empty();
            }
        }
    }

    /// 仅测试：直接设置失效位。
    #[cfg(test)]
    pub(crate) fn set_dirty_for_test(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.dirty = flags;
                true
            }
            None => false,
        }
    }

    /// 仅测试：直接写布局盒（P04 的 `set_layout` 之前的唯一写点，damage 用例需要）。
    #[cfg(test)]
    pub(crate) fn set_layout_for_test(
        &mut self,
        node: NodeId,
        layout: LayoutBox,
        prev_layout: LayoutBox,
    ) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.layout = layout;
                target.prev_layout = prev_layout;
                true
            }
            None => false,
        }
    }

    /// 仅测试：直接替换静态样式。
    #[cfg(test)]
    pub(crate) fn set_style_for_test(&mut self, node: NodeId, style: Style) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.style = style;
                true
            }
            None => false,
        }
    }

    /// 仅测试：就地编辑静态样式。
    #[cfg(test)]
    pub(crate) fn edit_style_for_test(&mut self, node: NodeId, edit: impl FnOnce(&mut Style)) {
        if let Some(target) = self.arena.get_mut(node) {
            edit(&mut target.style);
        }
    }
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use super::{LayoutBox, Node, NodeKind, NodeTree};
    use crate::{
        error::EngineError,
        math::Rect,
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId, signal::SignalId},
        paint::style::{Style, StylePatch},
    };

    fn boxed(tree: &mut NodeTree, parent: NodeId) -> NodeId {
        NodeBuilder::boxed().build(tree, parent).expect("挂载成功")
    }

    #[test]
    fn new_tree_has_root() {
        let tree = NodeTree::new();
        let root = tree.root();

        assert_eq!(tree.order(), [root].as_slice());
        assert_eq!(tree.get(root).expect("root 存在").kind(), &NodeKind::Box);
        assert_eq!(
            tree.get(root).expect("root 存在").parent(),
            None,
            "root 无父"
        );
    }

    #[test]
    fn append_child_keeps_dfs_preorder() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);
        let b = boxed(&mut tree, root);

        assert_eq!(tree.order(), [root, a, a1, b].as_slice());
        assert_eq!(tree.get(a).expect("存在").parent(), Some(root));
        assert_eq!(tree.get(a1).expect("存在").parent(), Some(a));
    }

    #[test]
    fn append_child_reparents_and_splices() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);
        let b = boxed(&mut tree, root);
        let b1 = boxed(&mut tree, b);

        tree.append_child(a, b).expect("重挂成功");

        assert_eq!(
            tree.order(),
            [root, a, a1, b, b1].as_slice(),
            "子树块必须整体搬到新父的末尾，且不重复"
        );
        assert_eq!(tree.get(b).expect("存在").parent(), Some(a));
        assert_eq!(tree.get(b1).expect("存在").parent(), Some(b));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "structural cycle")]
    fn append_child_rejects_cycle() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);

        // 把祖先挂成后代的子节点：结构环。
        let _ = tree.append_child(a1, a);
    }

    #[test]
    fn remove_unmounts_subtree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);
        let a2 = boxed(&mut tree, a);
        let b = boxed(&mut tree, root);

        tree.remove(a).expect("卸载成功");

        assert_eq!(tree.order(), [root, b].as_slice());
        for stale in [a, a1, a2] {
            assert!(
                matches!(tree.get(stale), Err(EngineError::StaleNode { .. })),
                "子树内句柄必须全部陈旧"
            );
        }
        assert!(tree.get(b).is_ok());
    }

    #[test]
    fn remove_root_is_error() {
        let mut tree = NodeTree::new();
        let root = tree.root();

        assert!(matches!(
            tree.remove(root),
            Err(EngineError::StaleNode { node }) if node == root
        ));
        assert_eq!(tree.order(), [root].as_slice(), "root 必须还在");
    }

    #[test]
    fn append_child_stale_ids_are_errors() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = boxed(&mut tree, root);
        tree.remove(doomed).expect("卸载成功");

        assert!(matches!(
            tree.append_child(doomed, root),
            Err(EngineError::StaleNode { node }) if node == doomed
        ));
        assert!(matches!(
            tree.append_child(root, doomed),
            Err(EngineError::StaleNode { node }) if node == doomed
        ));
    }

    #[test]
    fn insert_child_and_bind_style_roundtrip() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = tree
            .insert_child(
                root,
                NodeKind::Box,
                Style::default(),
                Some(Box::new(|_cx| StylePatch::default())),
            )
            .expect("插入成功");

        assert!(tree.take_bind_style(node).is_some(), "闭包必须被取走");
        assert!(tree.take_bind_style(node).is_none(), "取走后为 None");
        assert!(tree.put_bind_style(node, Box::new(|_cx| StylePatch::default())));
        assert!(tree.take_bind_style(node).is_some());

        assert_eq!(tree.deps(node), Some([].as_slice()));
        assert!(tree.set_deps(node, smallvec![SignalId::for_test(0)]));
        assert_eq!(tree.deps(node).map(<[SignalId]>::len), Some(1));
        assert!(tree.set_style_patch(node, StylePatch::default()));
    }

    #[test]
    fn node_and_handle_sizes() {
        assert!(
            size_of::<Node>() <= 1024,
            "Node 尺寸超预算: {} B（备选：style: Box<Style>，见 V-30）",
            size_of::<Node>()
        );
        assert_eq!(size_of::<Option<NodeId>>(), 8, "Option<NodeId> 必须零开销");
        assert_eq!(size_of::<NodeId>(), 8);
    }

    #[test]
    fn damage_unions_prev_and_current() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = boxed(&mut tree, root);
        tree.set_layout_for_test(
            node,
            LayoutBox {
                bounds: Rect::new(10.0, 0.0, 4.0, 4.0),
            },
            LayoutBox {
                bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            },
        );
        assert!(
            tree.get(node).expect("存在").damage().is_empty(),
            "布局写入本身不产生 damage（damage 由 touch 累积）"
        );

        assert!(tree.mark_paint(node));

        let damage = tree.get(node).expect("存在").damage();
        assert_eq!(
            damage,
            Rect::new(0.0, 0.0, 14.0, 4.0),
            "旧位置与新位置的并集必须都覆盖"
        );
    }

    #[test]
    fn remove_marks_parent_structure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = boxed(&mut tree, root);
        let child = boxed(&mut tree, parent);
        let _grandchild = boxed(&mut tree, child);
        tree.clear_all_dirty_for_test();

        tree.remove(child).expect("卸载成功");

        let parent_flags = tree.get(parent).expect("存在").dirty();
        for expected in [
            DirtyFlags::LAYOUT,
            DirtyFlags::PAINT,
            DirtyFlags::ORDER,
            DirtyFlags::A11Y,
        ] {
            assert!(
                parent_flags.contains(expected),
                "结构变更必须标 {expected:?}（实际 {parent_flags:?}）"
            );
        }
        assert!(
            tree.get(root)
                .expect("存在")
                .dirty()
                .contains(DirtyFlags::LAYOUT),
            "父是 auto 链 → 必须冒泡到根"
        );
        assert!(
            tree.get(parent).expect("存在").children().is_empty(),
            "被移除的子树必须从父的 children 里摘掉"
        );
    }

    #[test]
    fn append_child_marks_moved_subtree_paint() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let from = boxed(&mut tree, root);
        let to = boxed(&mut tree, root);
        let child = boxed(&mut tree, from);
        let grandchild = boxed(&mut tree, child);
        tree.set_dirty_for_test(child, DirtyFlags::REUSABLE);
        tree.set_dirty_for_test(grandchild, DirtyFlags::REUSABLE | DirtyFlags::LAYOUT);

        tree.append_child(to, child).expect("重挂成功");

        for moved in [child, grandchild] {
            let flags = tree.get(moved).expect("存在").dirty();
            assert!(
                flags.contains(DirtyFlags::PAINT),
                "被移动子树的每个节点都必须标 PAINT（实际 {flags:?}）"
            );
            assert!(
                !flags.contains(DirtyFlags::REUSABLE),
                "REUSABLE 必须被清（实际 {flags:?}）"
            );
        }
        assert!(
            tree.get(to)
                .expect("存在")
                .dirty()
                .contains(DirtyFlags::ORDER)
        );
    }
}
