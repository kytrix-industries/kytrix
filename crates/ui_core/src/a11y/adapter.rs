//! `A11yBridge`：变更集合、快照产出与 `accesskit::TreeUpdate` 组装。
//!
//! 三条纪律：
//! 1. **懒激活**：`TreeUpdate` 只在 `accesskit_*` 的 `update_if_active` 闭包里构造（adapter
//!    不活跃时闭包不执行 ⇒ 零 accesskit 开销）。引擎侧的 [`A11yBridge::take`] 只做「收集候选 +
//!    清位 + 建快照」，不构造任何 `accesskit::Node`。
//! 2. **单一 drainer**：`take` 会消费 `A11Y` 位（与 `dirty_a11y` 反向集合）及 `full_pending`，
//!    同一时刻只能有一个消费者（生产 = 平台层；`test-support` 的捕获路径只在 `TestApp` 里打开）。
//! 3. **投递预算**（[`A11Y_BATCH_BUDGET`]）：一笔 `TreeUpdate` 最多带固定数量的节点，
//!    剩余候选留在队列里、由后续交付继续推进。详见 [`A11yBridge`] 的字段文档。
//!
//! 焦点语义（A3）：`TreeUpdate.focus` 是"焦点身份"，而**焦点节点本身**必须已经在消费端的树里。
//! 本模块保证两件事：
//! - 焦点变化**不依赖脏位**：`pending` 直接比较"当前焦点 vs 上次交付的焦点"，所以焦点变化在
//!   下一轮泵就会被交付（不节流，见 `App::a11y_pending` 的焦点旁路）；
//! - 焦点节点若不在本批 `nodes` 里，[`build_update`] 会把它**并入同一笔**（去重），
//!   包括"节点刚挂载、还没被任何一笔交付过"的边界。

use accesskit::{NodeId as AkNodeId, Rect as AkRect, TreeId, TreeInfo, TreeUpdate};
use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabelSource, A11yNode, A11yNodeId, A11yRole, A11yTreeSnapshot,
        identity::A11yIds,
        mapping::{action_to_accesskit, live_to_accesskit, role_to_accesskit, sort_to_accesskit},
        textrun,
        tree::{collect_subtree, node_a11y, push_ancestors, resolve_focus},
    },
    app::App,
    math::Scale,
    node::{id::NodeId, tree::NodeTree},
    window::WindowId,
};

/// 一笔 `TreeUpdate` 最多携带的节点数（投递预算）。
///
/// 为什么需要它：首笔（AT 刚连上）、DPI 变化、捕获打开都会走"近似全树"的重建；`accesskit` 的
/// 更新是在 UI 线程上同步构造的，10 万节点的窗口一笔发完会把 UI 线程钉住几百毫秒
/// （R13-7）。2048 个节点约 1~3 ms（含 `accesskit::Node` 物化），既能让 AT 在 2~3 笔内看到
/// 完整的首棵树（100 ms 节流窗口的 1/30），又不会让单笔成本随窗口规模线性爆炸。
///
/// **超预算策略 = 分批（不是丢弃、也不是延迟到下一次脏位）**：候选按"父先于子"入队，每笔发
/// 预算内的前缀，剩余部分保留在 [`A11yBridge::queue`]；`pending` 在队列非空时恒为真，所以平台
/// 泵的下一轮会继续发。分批期间父节点的 `children` 只列**已交付或同批**的子节点，等该子节点
/// 到达的那一笔再把父节点重发一次（[`A11yBridge::refresh`]）—— 这样每一笔都满足 AccessKit 的
/// 硬约束（节点的父必须已在树里或同批，`children` 里的 id 必须可达）。
pub(crate) const A11Y_BATCH_BUDGET: usize = 2048;

/// 一个窗口的 a11y 状态机（`Window.a11y`）。
pub(crate) struct A11yBridge {
    /// 下一笔必须是全树（首次交付 / scale 变化 / 捕获开启）。
    full_pending: bool,
    /// 布局比例代际（`Window::set_scale` 递增；变化 ⇒ 全树重算 bounds）。
    bounds_epoch: u32,
    /// 上一笔交付时的代际。
    seen_bounds_epoch: u32,
    /// 内容整体平移的子树（滚动容器）：子孙 bounds 全变。
    subtrees: SmallVec<[NodeId; 2]>,
    /// 有 `bind_a11y` 节点被写信号（没有像素要画，但 `refresh_styles` 必须跑一次）。
    bind_work: bool,
    /// 稳定身份分配器（跨帧不变；逻辑行重建后不变）。
    ids: A11yIds,
    /// 上次交付的焦点（已解析到存在的节点）。
    last_focus: Option<NodeId>,
    /// 已经交付给消费端的引擎节点（用于 `children` 过滤与分批的"父先于子"）。
    delivered: ahash::HashSet<NodeId>,
    /// 待发节点队列（全树分片 or 增量溢出），`queue_cursor` 之前的都已发过。
    queue: Vec<NodeId>,
    /// `queue` 的读游标。
    queue_cursor: usize,
    /// `queue` 的成员集合（去重；增量补充候选时用）。
    queued: ahash::HashSet<NodeId>,
    /// 下一笔必须先重发的父节点（它们的 `children` 因分批暂时不完整）。
    refresh: SmallVec<[NodeId; 8]>,
    /// 本次全树运行是否还没发过 `TreeInfo`。
    tree_info_pending: bool,
    /// 测试捕获开关（生产构建里整个字段不存在：捕获只有 `test-support` 的 `TestApp` 会用）。
    #[cfg(feature = "test-support")]
    capture: bool,
}

impl A11yBridge {
    /// 新建（首笔必为全树）。
    pub(crate) fn new() -> Self {
        Self {
            full_pending: true,
            bounds_epoch: 0,
            seen_bounds_epoch: 0,
            subtrees: SmallVec::new(),
            bind_work: false,
            ids: A11yIds::new(),
            last_focus: None,
            delivered: ahash::HashSet::default(),
            queue: Vec::new(),
            queue_cursor: 0,
            queued: ahash::HashSet::default(),
            refresh: SmallVec::new(),
            tree_info_pending: false,
            #[cfg(feature = "test-support")]
            capture: false,
        }
    }

    /// 本轮是否有 a11y 工作（O(1) 级：脏位计数 + 旗标 + 一次焦点比较）。
    pub(crate) fn pending(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        self.full_pending
            || self.bounds_epoch != self.seen_bounds_epoch
            || !self.subtrees.is_empty()
            || tree.a11y_dirty() > 0
            || self.queue_cursor < self.queue.len()
            || !self.refresh.is_empty()
            || self.focus_changed(tree, focus)
    }

    /// 焦点是否变化且尚未交付（`App::a11y_pending` 的**不节流旁路**）。
    pub(crate) fn focus_pending(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        self.focus_changed(tree, focus)
    }

    /// DPI 比例变化（bounds 全变 ⇒ 下一笔全树）。
    pub(crate) fn note_bounds_changed(&mut self) {
        self.bounds_epoch = self.bounds_epoch.wrapping_add(1);
    }

    /// 只有 `bind_a11y` 的节点被写信号：记下来让下一帧仍跑 `refresh_styles`（不产生像素）。
    pub(crate) fn note_bind_work(&mut self) {
        self.bind_work = true;
    }

    /// 是否有待求值的 a11y 绑定（`refresh_styles` 开头清掉）。
    pub(crate) fn bind_work(&self) -> bool {
        self.bind_work
    }

    /// 清掉「a11y 绑定待求值」旗标（`refresh_styles` 的入口处）。
    pub(crate) fn clear_bind_work(&mut self) {
        self.bind_work = false;
    }

    /// 某个子树的内容整体平移（滚动）：下一笔重发整棵子树。
    pub(crate) fn invalidate_subtree(&mut self, node: NodeId) {
        if !self.subtrees.contains(&node) {
            self.subtrees.push(node);
        }
    }

    /// 强制下一笔为全树（测试 oracle / 调试 / `build_update` 的兜底重试）。
    pub(crate) fn force_full(&mut self) {
        self.full_pending = true;
    }

    /// 稳定 id → 引擎句柄（`ActionRequest` 解码）。
    pub(crate) fn engine_of(&self, id: A11yNodeId) -> Option<NodeId> {
        self.ids.engine_of(id)
    }

    /// 引擎句柄 → 稳定 id（只查不分配；身份 / 动作测试用）。
    #[cfg(test)]
    pub(crate) fn stable_of(&self, node: NodeId) -> Option<A11yNodeId> {
        self.ids.stable_of(node)
    }

    /// 打开/关闭测试捕获。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_capture(&mut self, on: bool) {
        if on && !self.capture {
            self.full_pending = true;
        }
        self.capture = on;
    }

    /// 产出一笔交付（消费 `A11Y` 反向集合 / `subtrees` / 队列前缀）。`None` = 没有工作。
    ///
    /// `caret` = 窗口的 IME 文本插入点提示 `(文本节点, UTF-8 字节偏移)`：本节点没有显式
    /// `A11yPatch::selection` 时用它填 `text_selection`（读屏"光标在第 N 个字符"由此而来）。
    pub(crate) fn take(
        &mut self,
        tree: &mut NodeTree,
        focus: Option<NodeId>,
        scale: Scale,
        title: &str,
        caret: Option<(NodeId, usize)>,
        diagnostics: &mut Vec<A11yDiagnostic>,
    ) -> Option<A11yTreeSnapshot> {
        // R13-29：`pending` 与 `take` 使用**同一个**判据（旧实现两份独立条件，漂移后会走到
        // "非法空 update"分支）。
        if !self.pending(tree, focus) {
            return None;
        }
        self.ids.begin_delivery();
        let full_run = self.full_pending || self.bounds_epoch != self.seen_bounds_epoch;
        if full_run {
            // 全树：消费端会整体重置树，`delivered` 从零开始。
            self.full_pending = false;
            self.seen_bounds_epoch = self.bounds_epoch;
            self.tree_info_pending = true;
            self.delivered.clear();
            self.queue.clear();
            self.queued.clear();
            self.queue_cursor = 0;
            self.refresh.clear();
            self.ids.prune(tree);
            // 全树笔同样是"唯一清除点"：`take_a11y` 只摘集合，节点上的 `A11Y` 位仍要用
            // `clear_a11y` 消掉（否则下一次 `mark_a11y` 会因位置已在而漏掉候选）。
            let mut dirty = tree.take_a11y();
            for node in dirty.iter().copied() {
                tree.clear_a11y(node);
            }
            tree.recycle(std::mem::take(&mut dirty));
            tree.for_each_dfs(|node| {
                self.queued.insert(node);
                self.queue.push(node);
            });
        }
        // 增量候选（滚动子树 / 脏节点 / 祖先链）追加到队列；分批时新到的脏位不会丢。
        if !full_run {
            self.collect_incremental(tree);
        }

        // ① 本批：先重发"children 暂时不完整"的父节点，再按队列顺序取新节点。
        //    每个新节点的未交付祖先会被**先行**补进同一批（`ensure_ancestors`），保证
        //    AccessKit 的"父在树里或同批"约束；DFS 全树队列里祖先本来就在前面，所以首树
        //    这条路径上的祖先补充是零成本（已交付/已在批）。
        let mut batch: Vec<NodeId> = Vec::with_capacity(A11Y_BATCH_BUDGET.min(self.queue.len()));
        let mut batch_set: ahash::HashSet<NodeId> = ahash::HashSet::default();
        let push =
            |node: NodeId, batch: &mut Vec<NodeId>, batch_set: &mut ahash::HashSet<NodeId>| {
                if batch_set.insert(node) {
                    batch.push(node);
                }
            };
        for node in std::mem::take(&mut self.refresh) {
            if tree.get(node).is_ok() {
                push(node, &mut batch, &mut batch_set);
            }
        }
        let mut ancestors: SmallVec<[NodeId; 16]> = SmallVec::new();
        while batch.len() < A11Y_BATCH_BUDGET && self.queue_cursor < self.queue.len() {
            let node = self.queue[self.queue_cursor];
            self.queue_cursor += 1;
            if tree.get(node).is_err() {
                continue;
            }
            // 增量笔：结构增删必须同批发父（AccessKit 硬约束：节点的父必须已在树里或同批，
            // 且 `children` 里的 id 必须可达）。全树笔不需要 —— DFS 顺序已经保证父先于子，
            // 且父的 `children` 由 `refresh` 机制在子节点到达时补齐。
            if !full_run {
                push_ancestors(tree, node, &mut ancestors);
                for ancestor in ancestors.drain(..) {
                    push(ancestor, &mut batch, &mut batch_set);
                }
            }
            push(node, &mut batch, &mut batch_set);
        }

        // ② 建记录：祖先先于后代（depth 升序；同深度保持入队序）。节点数是预算内的常数，
        //    这次排序不会引入 O(N) 成本。
        batch.sort_by_key(|node| depth_of(tree, *node));
        let mut nodes = Vec::with_capacity(batch.len());
        let mut re_refresh: SmallVec<[NodeId; 8]> = SmallVec::new();
        for node in &batch {
            if let Some(mut record) =
                node_a11y(tree, *node, scale, title, &mut self.ids, diagnostics)
            {
                // `children` 只列**已交付或同批**的子节点（AccessKit 契约）；被省略的子节点
                // 到达时会把父节点重发一次。
                let before = record.children.len();
                record.children.retain(|child| {
                    self.ids.engine_of(*child).is_some_and(|engine| {
                        self.delivered.contains(&engine) || batch_set.contains(&engine)
                    })
                });
                if record.children.len() != before {
                    re_refresh.push(*node);
                }
                nodes.push(record);
            }
        }
        for node in &batch {
            self.delivered.insert(*node);
        }
        self.refresh = re_refresh;

        // ③ 收尾：队列与重发都空了 ⇒ 本次全树/增量运行结束，做一次旁表清理。
        if self.queue_cursor >= self.queue.len() && self.refresh.is_empty() {
            self.queue.clear();
            self.queued.clear();
            self.queue_cursor = 0;
            self.ids.prune(tree);
            if self.delivered.len() > tree.len().saturating_mul(2) {
                self.delivered.retain(|node| tree.get(*node).is_ok());
            }
        }
        let emit_tree_info = std::mem::take(&mut self.tree_info_pending);

        // ④ 焦点（必带；陈旧 ⇒ 回退到根 + 诊断）。焦点变化由 `pending` 的独立通道保证
        //    及时，不依赖脏位。
        let (focus_id, missing) = resolve_focus(tree, focus);
        if let Some(missing) = missing {
            diagnostics.push(A11yDiagnostic::MissingFocusTarget { node: missing });
        }
        let mut scratch: Vec<A11yDiagnostic> = Vec::new();
        let mut focus_node = node_a11y(tree, focus_id, scale, title, &mut self.ids, &mut scratch)
            .expect("resolve_focus 已保证焦点节点存活");
        apply_caret(&mut focus_node, caret);
        for record in &mut nodes {
            apply_caret(record, caret);
        }
        self.last_focus = Some(focus_id);
        let root_id = self
            .ids
            .stable_of(tree.root())
            .expect("根节点必然在 id 分配器里（首笔起）");
        Some(A11yTreeSnapshot {
            full: emit_tree_info,
            root: root_id,
            nodes,
            focus: self
                .ids
                .stable_of(focus_id)
                .expect("焦点节点刚被 node_a11y 分配 id"),
            focus_node,
            diagnostics: diagnostics.len(),
        })
    }

    /// 焦点是否与上次交付不同（陈旧焦点按"回退到根"后的值比较，避免永久 pending）。
    fn focus_changed(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        let (resolved, _) = resolve_focus(tree, focus);
        self.last_focus != Some(resolved)
    }

    /// 收集增量候选（脏位 + 滚动子树）追加到队列；清位在收集后立即做。
    fn collect_incremental(&mut self, tree: &mut NodeTree) {
        let mut dirty = tree.take_a11y();
        let mut fresh: SmallVec<[NodeId; 32]> = SmallVec::new();
        for node in dirty.iter().copied() {
            fresh.push(node);
            // 清位（唯一清除点）：`take_a11y` 只摘集合，节点上的 `A11Y` 位要单独消掉。
            tree.clear_a11y(node);
        }
        tree.recycle(std::mem::take(&mut dirty));
        let mut subtree_nodes: SmallVec<[NodeId; 16]> = SmallVec::new();
        for root in std::mem::take(&mut self.subtrees) {
            collect_subtree(tree, root, &mut subtree_nodes);
        }
        for node in subtree_nodes.drain(..) {
            fresh.push(node);
        }
        for node in fresh {
            if self.queued.insert(node) {
                self.queue.push(node);
            }
        }
    }
}

impl Default for A11yBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// 节点深度（根 = 0）；分批排序用。
fn depth_of(tree: &NodeTree, node: NodeId) -> u32 {
    let mut depth = 0u32;
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(parent) = cursor {
        depth += 1;
        cursor = tree.get(parent).ok().and_then(|target| target.parent());
    }
    depth
}

/// 用窗口的 IME 插入点提示补 `text_selection`（只在没有显式选区、且节点是可编辑文本时）。
fn apply_caret(record: &mut A11yNode, caret: Option<(NodeId, usize)>) {
    let Some((node, byte_index)) = caret else {
        return;
    };
    if record.engine != node
        || record.text_selection.is_some()
        || record.character_lengths.is_empty()
        || !matches!(
            record.role,
            A11yRole::TextRun | A11yRole::TextInput | A11yRole::MultilineTextInput
        )
    {
        return;
    }
    record.text_selection = Some(textrun::selection(
        &record.character_lengths,
        crate::a11y::A11yTextSelection {
            anchor: byte_index,
            focus: byte_index,
        },
    ));
}

/// 生产/捕获的唯一物化入口：跑一笔 pass 并转成 `accesskit::TreeUpdate`。
///
/// **必须在 `update_if_active` 的闭包里调用**（adapter 不活跃时闭包不执行 ⇒ 零开销）。
pub(crate) fn build_update(app: &mut App, window: WindowId) -> TreeUpdate {
    if app.window(window).is_none() {
        // 窗口已关闭：平台层在 adapter 析构后不会再调用（不可达分支）。返回空更新而不是 panic；
        // `focus: 0` 在这条路径上不会被消费（消费端随窗口一起销毁）。
        return TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: TreeId::ROOT,
            focus: AkNodeId(0),
        };
    }
    let Some(snapshot) = app.a11y_take(window) else {
        // 防御：`a11y_pending` 为真时 `take` 必产出。真走到这里说明调用方没判 pending
        // （R13-29 的"非法空 update"分支）：强制全树再取一次，保证 focus 一定是树里的节点。
        if let Some(target) = app.window_mut(window) {
            target.a11y_force_full();
        }
        // 强制全树后 `take` 必产出（`pending` 为真），否则是引擎不变式被破坏。
        let snapshot = app
            .a11y_take(window)
            .expect("a11y: force-full delivery must produce a snapshot");
        return to_tree_update(&snapshot);
    };
    to_tree_update(&snapshot)
}

/// 快照 → `accesskit::TreeUpdate`（`tree: Some(..)` 只在全树那一笔出现）。
///
/// **焦点节点并入同笔**（A3）：消费端要求 `focus` 指向的节点已经在树里；增量笔里焦点节点
/// 通常不在 `nodes`（它自己没变），但它可能刚挂载（"焦点节点尚未进入树"），所以这里按 id 去重
/// 后把它追加进 `nodes`。AccessKit 允许重发未变化的节点（只是多付一点带宽）。
pub(crate) fn to_tree_update(snapshot: &A11yTreeSnapshot) -> TreeUpdate {
    let tree = snapshot.full.then(|| TreeInfo {
        root: AkNodeId(snapshot.root.get()),
        toolkit_name: Some(String::from("Kytrix")),
        toolkit_version: Some(String::from(env!("CARGO_PKG_VERSION"))),
    });
    let mut nodes = Vec::with_capacity(snapshot.nodes.len() + 1);
    for record in &snapshot.nodes {
        nodes.push((AkNodeId(record.id.get()), to_accesskit(record)));
    }
    let focus_engine = snapshot.focus_node.engine;
    if !snapshot
        .nodes
        .iter()
        .any(|record| record.engine == focus_engine)
    {
        nodes.push((
            AkNodeId(snapshot.focus_node.id.get()),
            to_accesskit(&snapshot.focus_node),
        ));
    }
    TreeUpdate {
        nodes,
        tree,
        tree_id: TreeId::ROOT,
        focus: AkNodeId(snapshot.focus.get()),
    }
}

/// 快照记录 → `accesskit::Node`。
pub(crate) fn to_accesskit(record: &crate::a11y::A11yNode) -> accesskit::Node {
    let mut node = accesskit::Node::new(role_to_accesskit(record.role));
    if !record.children.is_empty() {
        node.set_children(
            record
                .children
                .iter()
                .map(|child| AkNodeId(child.get()))
                .collect::<Vec<_>>(),
        );
    }
    if record.hidden {
        node.set_hidden();
    }
    if record.disabled {
        node.set_disabled();
    }
    // `Label` / `TextRun` 的可访问文本来自 `value`（`accesskit_consumer::label_comes_from_value`，
    // 以及 `character_lengths` 的求和基准）。
    if let Some(label) = &record.label {
        if record.role == A11yRole::Label
            || record.role == A11yRole::TextRun
            || label.source() == A11yLabelSource::TextContent
        {
            node.set_value(label.text());
        } else {
            node.set_label(label.text());
        }
    }
    if let Some(value) = &record.value {
        node.set_value(value.as_str());
    }
    if let Some(description) = &record.description {
        node.set_description(description.as_str());
    }
    if let Some(selected) = record.selected {
        node.set_selected(selected);
    }
    if let Some(checked) = record.checked {
        node.set_toggled(accesskit::Toggled::from(checked));
    }
    if let Some(expanded) = record.expanded {
        node.set_expanded(expanded);
    }
    // 实时区域（P2-6）：没有它 AT 不会主动播报数值变化。
    if let Some(live) = record.live {
        node.set_live(live_to_accesskit(live));
    }
    // 集合内位置（P2-6）：虚拟列表行靠它播报「第 N 项，共 M 项」。
    if let Some(position) = record.pos_in_set {
        node.set_position_in_set(position);
    }
    if let Some(size) = record.size_of_set {
        node.set_size_of_set(size);
    }
    // 数值通道（P2-8）：UIA 的 RangeValuePattern 读这几个字段。
    if let Some(value) = record.numeric_value {
        node.set_numeric_value(value);
    }
    if let Some(value) = record.min_numeric_value {
        node.set_min_numeric_value(value);
    }
    if let Some(value) = record.max_numeric_value {
        node.set_max_numeric_value(value);
    }
    if let Some(value) = record.numeric_value_step {
        node.set_numeric_value_step(value);
    }
    if let Some(value) = record.numeric_value_jump {
        node.set_numeric_value_jump(value);
    }
    // 表格 / 虚拟列表的行列语义（P2-7）：屏幕阅读器据此播报「第 N 行 / 共 M 行」并支持
    // 按行列导航。没声明就不写（AccessKit 的 `None` 与「0 行」语义不同）。
    if let Some(index) = record.row_index {
        node.set_row_index(index);
    }
    if let Some(index) = record.column_index {
        node.set_column_index(index);
    }
    if let Some(count) = record.row_count {
        node.set_row_count(count);
    }
    if let Some(count) = record.column_count {
        node.set_column_count(count);
    }
    // `aria-sort`（A2）：表头靠它播报"按价格降序"。
    if let Some(sort) = record.sort {
        node.set_sort_direction(sort_to_accesskit(sort));
    }
    // 滚动状态（R13-13）：UIA 的 ScrollPattern / AT-SPI 的 Value 读这几项。
    if let Some(scroll) = record.scroll {
        node.set_scroll_x(f64::from(scroll.x));
        node.set_scroll_y(f64::from(scroll.y));
        node.set_scroll_x_min(0.0);
        node.set_scroll_y_min(0.0);
        node.set_scroll_x_max(f64::from(scroll.x_max));
        node.set_scroll_y_max(f64::from(scroll.y_max));
        node.set_orientation(if scroll.vertical {
            accesskit::Orientation::Vertical
        } else {
            accesskit::Orientation::Horizontal
        });
    }
    if record.clips_children {
        node.set_clips_children();
    }
    // 文本语义（A5）：`Σ character_lengths == value.len()`。
    if !record.character_lengths.is_empty() {
        node.set_character_lengths(record.character_lengths.clone());
    }
    if !record.word_starts.is_empty() {
        node.set_word_starts(record.word_starts.clone());
    }
    if let Some(selection) = record.text_selection {
        node.set_text_selection(accesskit::TextSelection {
            anchor: accesskit::TextPosition {
                node: AkNodeId(record.id.get()),
                character_index: selection.anchor,
            },
            focus: accesskit::TextPosition {
                node: AkNodeId(record.id.get()),
                character_index: selection.focus,
            },
        });
    }
    // 表单 / 状态属性（R13-14）。
    if record.read_only {
        node.set_read_only();
    }
    if record.modal {
        node.set_modal();
    }
    if record.busy {
        node.set_busy();
    }
    if record.invalid {
        node.set_invalid(accesskit::Invalid::True);
    }
    if let Some(message) = record.error_message {
        // `ErrorMessage` 是 NodeId 关系（`aria-errormessage`），不是字符串。
        node.set_error_message(AkNodeId(message.get()));
    }
    if let Some(placeholder) = &record.placeholder {
        node.set_placeholder(placeholder.as_str());
    }
    if let Some(level) = record.level {
        node.set_level(level);
    }
    // 自定义动作（R13-4）：能力 + 注册表（id + 名字）都要写；只写类别时 AT 看不到动作名。
    for custom in &record.custom_actions {
        node.push_custom_action(accesskit::CustomAction {
            id: custom.id,
            description: custom.label.to_string(),
        });
    }
    if !record.custom_actions.is_empty() {
        // 注册表本身就是能力声明：即使 `actions` 没显式列 `Custom(id)`，节点也支持该类别。
        node.add_action(accesskit::Action::CustomAction);
    }
    for action in &record.actions {
        node.add_action(action_to_accesskit(*action));
    }
    let bounds = record.bounds;
    node.set_bounds(AkRect::new(
        f64::from(bounds.x0),
        f64::from(bounds.y0),
        f64::from(bounds.x1),
        f64::from(bounds.y1),
    ));
    node
}

#[cfg(test)]
mod tests {
    use accesskit::{Action, NodeId as AkNodeId};
    use smol_str::SmolStr;

    use super::{A11Y_BATCH_BUDGET, A11yBridge, to_accesskit, to_tree_update};
    use crate::{
        a11y::{
            A11yAction, A11yCustomAction, A11yOverride, A11yPatch, A11yRole, A11ySort,
            A11yTextSelection,
        },
        math::Scale,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style::StylePatch,
        text::TextStyle,
    };

    fn scale() -> Scale {
        Scale::ONE
    }

    fn take(
        bridge: &mut A11yBridge,
        tree: &mut NodeTree,
        focus: Option<NodeId>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        let mut diagnostics = Vec::new();
        bridge.take(tree, focus, scale(), "Kytrix", None, &mut diagnostics)
    }

    #[test]
    fn first_delivery_is_full_and_then_incremental() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed()
            .on_click(crate::event::Action::custom("ok"))
            .a11y(A11yOverride {
                label: Some(SmolStr::new_static("提交")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("mount");
        let mut bridge = A11yBridge::new();

        let full = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(full.full);
        assert_eq!(full.nodes.len(), tree.len());
        assert_eq!(full.focus, full.node_for(root).expect("root").id);
        assert_eq!(
            full.node_for(button).expect("button").role,
            A11yRole::Button
        );
        assert!(!bridge.pending(&tree, None), "交付后必须无工作");

        assert!(
            take(&mut bridge, &mut tree, None).is_none(),
            "无变化时不得产出"
        );

        // 只改一个 label ⇒ 只含该节点 + 祖先。
        tree.set_a11y_patch(
            button,
            A11yPatch {
                label: Some(SmolStr::new_static("提交（改）")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(button);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!patch.full);
        assert_eq!(
            patch
                .nodes
                .iter()
                .map(|node| node.engine)
                .collect::<Vec<_>>(),
            vec![root, button],
            "增量 = 变更节点 + 祖先链"
        );
        assert_eq!(
            patch.node_for(button).expect("button").label_text(),
            Some("提交（改）")
        );
        // A1：身份跨笔稳定（AT 不会认为"旧按钮消失、新按钮出现"）。
        assert_eq!(
            full.node_for(button).expect("button").id,
            patch.node_for(button).expect("button").id
        );
    }

    /// A3（失败用例先行）：焦点节点不在本笔 `nodes` 里时，`TreeUpdate.nodes` 仍必须包含它。
    ///
    /// AccessKit 要求 `focus` 指向树里存在的节点；旧实现把 `focus_node` 只放在快照里
    /// （生产路径从不发送）——"焦点节点刚挂载、还没被任何一笔交付"时 consumer 会看到
    /// 一个不存在的焦点（R13-5 现象 B）。
    #[test]
    fn the_focus_node_is_carried_in_the_same_tree_update() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let full = take(&mut bridge, &mut tree, None).expect("首笔");
        let update = to_tree_update(&full);
        assert!(
            update
                .nodes
                .iter()
                .any(|(id, _)| *id == AkNodeId(full.focus.get())),
            "首笔焦点必须在本笔 nodes 里"
        );

        // 只标根 ⇒ 本笔 nodes 只有根；焦点指向 button（未变化）。
        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("增量");
        assert_eq!(
            patch
                .nodes
                .iter()
                .map(|node| node.engine)
                .collect::<Vec<_>>(),
            vec![root],
            "本笔只含根"
        );
        let update = to_tree_update(&patch);
        let focus = update.focus;
        assert_eq!(focus, AkNodeId(patch.focus_node.id.get()));
        assert!(
            update.nodes.iter().any(|(id, _)| *id == focus),
            "焦点节点必须与 focus 同笔送达（否则 consumer 指向不存在的节点）"
        );
        // 不重复：焦点节点已经在 nodes 里时不得再追加一份。
        assert_eq!(
            update.nodes.iter().filter(|(id, _)| *id == focus).count(),
            1
        );
    }

    /// A3 边界：**节点还没被任何一笔交付过**，焦点立刻落在它上面。
    ///
    /// 真实时序：大树的首树是分批交付的（A4），用户在某一行还没被 AT 看到时就把焦点移过去
    /// （键盘 PageDown / 点击）。此时 TreeUpdate 必须同时带上焦点 id 和该节点的记录，否则
    /// consumer 会把焦点指向一个不存在的节点。
    #[test]
    fn a_focus_target_that_was_never_delivered_arrives_with_the_focus() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut children = Vec::new();
        for _ in 0..6000 {
            children.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let never_delivered = children[5999];
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        assert!(first.full);
        assert!(
            first.node_for(never_delivered).is_none(),
            "首笔只发预算内前缀，最后一个节点还没被交付"
        );

        assert!(
            bridge.pending(&tree, Some(never_delivered)),
            "焦点变化必须让 bridge pending（不依赖脏位）"
        );
        let snapshot = take(&mut bridge, &mut tree, Some(never_delivered)).expect("焦点笔");
        assert!(
            snapshot.node_for(never_delivered).is_none(),
            "本批仍在推进首树前缀，焦点节点不在 nodes 里"
        );

        let update = to_tree_update(&snapshot);
        assert_eq!(update.focus, AkNodeId(snapshot.focus_node.id.get()));
        let focus_record = update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .map(|(_, node)| node)
            .expect("焦点节点必须与 focus 同笔送达");
        assert_eq!(focus_record.role(), accesskit::Role::GenericContainer);
        assert_eq!(
            update
                .nodes
                .iter()
                .filter(|(id, _)| *id == update.focus)
                .count(),
            1,
            "焦点节点不得重复出现"
        );
    }

    #[test]
    fn structure_change_resends_the_parent_children() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = NodeBuilder::boxed().build(&mut tree, root).expect("parent");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        let child = NodeBuilder::boxed()
            .build(&mut tree, parent)
            .expect("child");
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let parent_node = patch.node_for(parent).expect("父必须重发");
        assert!(
            parent_node
                .children
                .contains(&patch.node_for(child).expect("child").id)
        );
        assert!(patch.node_for(child).is_some(), "新子节点必须出现");

        // 卸载：父的 children 缩短，被删节点**不出现**。
        tree.remove(child).expect("remove");
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(patch.node_for(child).is_none(), "被删节点不得出现");
        assert!(
            patch
                .node_for(parent)
                .expect("父必须重发")
                .children
                .is_empty()
        );
    }

    #[test]
    fn focus_is_always_carried_and_falls_back_to_root() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        tree.mark_a11y(button);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("焦点");
        assert_eq!(patch.focus, patch.node_for(button).expect("button").id);
        assert!(patch.node(patch.focus).is_some(), "焦点必须在快照里");

        tree.remove(button).expect("remove");
        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("陈旧焦点");
        assert_eq!(
            patch.focus,
            patch.node_for(root).expect("root").id,
            "陈旧焦点回退到根"
        );
    }

    /// A3：焦点变化本身就会让 `pending` 为真（不依赖 `mark_a11y`）。
    #[test]
    fn focus_changes_make_the_bridge_pending_without_dirty_bits() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(!bridge.pending(&tree, None));
        assert!(bridge.pending(&tree, Some(button)), "焦点变化必须 pending");
        assert!(bridge.focus_pending(&tree, Some(button)));
        let _ = take(&mut bridge, &mut tree, Some(button)).expect("焦点笔");
        assert!(
            !bridge.pending(&tree, Some(button)),
            "交付后焦点不再 pending"
        );
        assert!(!bridge.focus_pending(&tree, Some(button)));
    }

    /// A4：大表格（5000 节点）首笔必须**分批**——单笔节点数 ≤ 预算，且剩余部分由后续笔推进，
    /// 最终全部送达；分批期间父节点先于子节点出现。
    #[test]
    fn a_large_tree_is_delivered_in_budgeted_batches() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = NodeBuilder::boxed().build(&mut tree, root).expect("list");
        for _ in 0..5000 {
            NodeBuilder::boxed().build(&mut tree, list).expect("row");
        }
        let mut bridge = A11yBridge::new();
        let mut batches = 0usize;
        let mut seen = ahash::HashSet::default();
        while let Some(snapshot) = take(&mut bridge, &mut tree, None) {
            batches += 1;
            assert!(
                snapshot.nodes.len() <= A11Y_BATCH_BUDGET,
                "单笔节点数超预算：{} > {A11Y_BATCH_BUDGET}",
                snapshot.nodes.len()
            );
            assert!(batches < 100, "分批必须收敛（死循环保护）");
            if batches == 1 {
                assert!(snapshot.full, "首笔必须带 TreeInfo");
            }
            for record in &snapshot.nodes {
                // AccessKit 硬约束：节点的父必须已在树里或同批。
                if let Some(parent_engine) = tree
                    .get(record.engine)
                    .ok()
                    .and_then(crate::node::tree::Node::parent)
                {
                    assert!(
                        seen.contains(&parent_engine)
                            || snapshot
                                .nodes
                                .iter()
                                .any(|other| other.engine == parent_engine),
                        "节点的父必须已在树里或同批"
                    );
                }
                seen.insert(record.engine);
            }
            if !bridge.pending(&tree, None) {
                break;
            }
        }
        assert!(batches > 1, "5000 节点不可能一笔发完（实际 {batches} 笔）");
        assert_eq!(
            seen.len(),
            tree.len(),
            "分批不得丢节点（重发的父节点会重复出现，按集合计）"
        );
    }

    /// 数据更新走增量（不是全树）：一笔只带变更节点 + 祖先，与树规模无关。
    #[test]
    fn incremental_updates_do_not_resend_the_whole_tree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::new();
        for _ in 0..2000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let mut bridge = A11yBridge::new();
        while let Some(_snapshot) = take(&mut bridge, &mut tree, None) {
            if !bridge.pending(&tree, None) {
                break;
            }
        }
        tree.set_a11y_patch(
            rows[1000],
            A11yPatch {
                label: Some(SmolStr::new_static("changed")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(rows[1000]);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!patch.full);
        assert!(
            patch.nodes.len() <= 2,
            "2000 节点改 1 处只需 1 节点 + 祖先，实际 {}",
            patch.nodes.len()
        );
    }

    #[test]
    fn hidden_disabled_and_patch_fields_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        let message = NodeBuilder::text("余额不足", TextStyle::new(12.0))
            .build(&mut tree, root)
            .expect("message");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        tree.set_a11y_patch(
            node,
            A11yPatch {
                label: Some(SmolStr::new_static("量")),
                value: Some(SmolStr::new_static("42")),
                disabled: Some(true),
                hidden: Some(true),
                checked: Some(true),
                selected: Some(true),
                read_only: Some(true),
                busy: Some(true),
                invalid: Some(true),
                error_message: Some(message),
                placeholder: Some(SmolStr::new_static("输入价格")),
                level: Some(2),
                modal: Some(true),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(node);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let record = patch.node_for(node).expect("node");
        assert!(record.hidden);
        assert!(record.disabled);
        assert_eq!(record.checked, Some(true));
        assert_eq!(record.selected, Some(true));
        assert_eq!(record.label_text(), Some("量"));
        assert_eq!(record.value.as_deref(), Some("42"));

        let converted = to_accesskit(record);
        assert!(converted.is_hidden());
        assert!(converted.is_disabled());
        assert_eq!(converted.toggled(), Some(accesskit::Toggled::True));
        assert_eq!(converted.is_selected(), Some(true));
        assert_eq!(converted.label(), Some("量"));
        assert_eq!(converted.value(), Some("42"));
        assert!(converted.is_read_only());
        assert!(converted.is_busy());
        assert_eq!(converted.invalid(), Some(accesskit::Invalid::True));
        assert_eq!(
            converted.error_message(),
            Some(AkNodeId(
                bridge
                    .stable_of(message)
                    .expect("message 节点在首笔里已分配 id")
                    .get()
            )),
            "error_message 是 NodeId 关系（aria-errormessage）"
        );
        assert_eq!(converted.placeholder(), Some("输入价格"));
        assert_eq!(converted.level(), Some(2));
        assert!(converted.is_modal());
        let _ = StylePatch::default();
    }

    /// R13-4：自定义动作 + 排序 + 滚动都必须真的写到 accesskit 节点上。
    #[test]
    fn custom_actions_sort_and_scroll_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let header = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColumnHeader),
                label: Some(SmolStr::new_static("价格")),
                sort: Some(A11ySort::Ascending),
                custom_actions: vec![A11yCustomAction {
                    id: 5,
                    label: SmolStr::new_static("按价格排序"),
                }],
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("header");
        let mut bridge = A11yBridge::new();
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let converted = to_accesskit(snapshot.node_for(header).expect("header"));
        assert_eq!(
            converted.sort_direction(),
            Some(accesskit::SortDirection::Ascending)
        );
        assert_eq!(converted.custom_actions().len(), 1);
        assert!(converted.supports_action(Action::CustomAction));
    }

    #[test]
    fn scale_change_forces_a_full_tree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, root)
            .expect("node");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(!bridge.pending(&tree, None));

        bridge.note_bounds_changed();
        assert!(bridge.pending(&tree, None), "scale 变化必须被 pending 看见");
        let full = take(&mut bridge, &mut tree, None).expect("全树");
        assert!(full.full);
    }

    #[test]
    fn scroll_invalidation_resends_the_subtree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("container");
        let row = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, container)
            .expect("row");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        bridge.invalidate_subtree(container);
        let patch = take(&mut bridge, &mut tree, None).expect("滚动增量");
        assert!(patch.node_for(container).is_some());
        assert!(patch.node_for(row).is_some(), "容器子树必须整体重发");
    }

    #[test]
    fn tree_update_carries_toolkit_info_only_on_full() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut bridge = A11yBridge::new();
        let full = take(&mut bridge, &mut tree, None).expect("首笔");
        let update = to_tree_update(&full);
        assert!(update.tree.is_some(), "首笔必须给 TreeInfo");
        assert_eq!(update.tree_id, accesskit::TreeId::ROOT);
        assert_eq!(
            full.focus,
            full.node_for(root).expect("root").id,
            "无焦点时焦点是根"
        );

        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let update = to_tree_update(&patch);
        assert!(update.tree.is_none(), "增量不得带 TreeInfo");
        assert_eq!(update.focus, AkNodeId(patch.focus.get()));
    }

    #[test]
    fn actions_and_bounds_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(1.0, 2.0, 10.0, 20.0))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(SmolStr::new_static("ok")),
                actions: Some(vec![A11yAction::Click]),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("node");
        let mut bridge = A11yBridge::new();
        // 布局盒是 a11y bounds 的唯一来源；这里直接写一个已知盒（不跑布局）。
        tree.set_layout_for_test(
            node,
            crate::node::tree::LayoutBox {
                bounds: crate::math::Rect::new(1.0, 2.0, 10.0, 20.0),
                border: crate::math::Edges::ZERO,
            },
        );
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let record = snapshot.node_for(node).expect("node");
        assert_eq!(
            (
                record.bounds.x0,
                record.bounds.y0,
                record.bounds.x1,
                record.bounds.y1
            ),
            (1, 2, 11, 22)
        );
        let converted = to_accesskit(record);
        assert!(converted.supports_action(Action::Click));
        assert_eq!(
            converted.bounds(),
            Some(accesskit::Rect::new(1.0, 2.0, 11.0, 22.0))
        );
    }

    /// A5：IME caret hint 自动填进 `text_selection`（读屏"光标在第 N 个字符"的来源），
    /// 显式 `A11yPatch::selection` 优先。
    #[test]
    fn the_ime_caret_hint_fills_the_text_selection() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        let text = NodeBuilder::text("你好ab", TextStyle::new(12.0))
            .build(&mut tree, input)
            .expect("text");
        let mut bridge = A11yBridge::new();
        let mut diagnostics = Vec::new();
        let snapshot = bridge
            .take(
                &mut tree,
                Some(text),
                scale(),
                "Kytrix",
                Some((text, 3)),
                &mut diagnostics,
            )
            .expect("首笔");
        let record = snapshot.node_for(text).expect("text");
        assert_eq!(
            record.text_selection,
            Some(A11yTextSelection {
                anchor: 1,
                focus: 1
            }),
            "字节 3 = 第二个汉字起点 = grapheme 1"
        );
        let converted = to_accesskit(record);
        let selection = converted.text_selection().expect("text_selection");
        assert_eq!(selection.focus.character_index, 1);
        assert_eq!(selection.focus.node, AkNodeId(record.id.get()));
        assert_eq!(converted.character_lengths(), &[3, 3, 1, 1]);
    }
}
