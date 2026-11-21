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
//!
//! 消费端生命周期（E1）：`accesskit_*` 的激活/停用会让消费端**丢弃整棵树**。AccessKit 的契约是
//! 「`request_initial_tree` 返回 `None` 后的第一笔必须是全树（`tree: Some`）」，否则
//! `accesskit_consumer` 在 `Tree::new` 里 panic，或适配器永远停在 `Pending`（AT 看到空树）。
//! 引擎用 [`A11yConsumerEpoch`] 表达"消费端已重置"：平台回调 `bump()`，[`A11yBridge::take`]
//! 发现代际变化就把这一笔升成全树，[`A11yBridge::pending`] 因此也立即为真（不等脏位）。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use accesskit::{NodeId as AkNodeId, Rect as AkRect, TreeId, TreeInfo, TreeUpdate};
use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabelSource, A11yNode, A11yNodeId, A11yRole, A11yTreeSnapshot,
        identity::A11yIds,
        mapping::{action_to_accesskit, live_to_accesskit, role_to_accesskit, sort_to_accesskit},
        textrun,
        tree::{
            CollectionSemanticsCache, collect_subtree, node_a11y_with_cache, push_ancestors,
            resolve_focus,
        },
    },
    app::App,
    math::Scale,
    node::{
        id::NodeId,
        tree::{Node, NodeTree},
    },
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

/// 消费端树生命周期代际：**每次消费端重置整棵树时递增**（E1）。
///
/// 三个平台 adapter 的激活/停用回调（`ActivationHandler::request_initial_tree` /
/// `DeactivationHandler::deactivate_accessibility`）在 AccessKit 的线程上执行，不能碰 `App`，
/// 所以只对这份 `Arc<AtomicU64>` 做一次 `fetch_add` + 唤醒主线程；引擎在
/// [`A11yBridge::pending`] / [`A11yBridge::take`] 里读它。
///
/// **为什么用代际而不是布尔旗标**：AT 可能连续重置两次（停用后立刻重新启用）；布尔旗标会在
/// "第一次重置 → 全树交付 → 第二次重置"的到达顺序上产生竞态（清旗标与置旗标谁先谁后不可控）。
/// 代际在 `bump` 与交付之间只要求"不相等 ⇒ 全树"：任意次数的重置最多多付一笔全树，绝不会
/// 漏掉"重置后第一笔必须是全树"的硬约束。
///
/// 内存序用 `SeqCst`：平台回调与主线程之间除了这个计数器没有其它同步点（adapter 内部的
/// 状态互斥锁保护的是 adapter 状态，不是引擎代际），代际变化必须与 `pending`/`take` 的顺序
/// 严格一致 —— 这里不是热路径（只在 AT 接入/重连时写一次），不值得为弱内存序冒险。
#[derive(Clone, Debug)]
pub(crate) struct A11yConsumerEpoch(Arc<AtomicU64>);

impl A11yConsumerEpoch {
    /// 新代际（从 0 开始；`A11yBridge::new` 的初始 `seen` 也是 0）。
    ///
    /// **不对平台层开放构造**（RE-01）：平台 adapter 必须持有窗口 `A11yBridge` 里的**那一份**
    /// 句柄（`App::window_a11y_epoch`），否则激活回调 `bump()` 的是另一份计数器，引擎的
    /// `seen_consumer_epoch` 永远看不到变化 —— E1 在生产上退化成「重激活后等下一次输入才有
    /// 树」。构造函数私有 + 无 `Default` 让「另起一份」在编译期不可写。
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// 标记消费端已重置：下一笔交付必须是全树。
    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    /// 当前代际。
    pub(crate) fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// 两份句柄是否指向**同一个**计数器（RE-01 的共享句柄断言用；仅测试有读者）。
    ///
    /// 平台 adapter 与窗口 `A11yBridge` 必须共享同一份：`same_handle == false` 时回调的
    /// `bump()` 永远不会被引擎看到（E1 在生产上失效）。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// 一个窗口的 a11y 状态机（`Window.a11y`）。
pub(crate) struct A11yBridge {
    /// 下一笔必须是全树（首次交付 / scale 变化 / 捕获开启）。
    full_pending: bool,
    /// 布局比例代际（`Window::set_scale` 递增；变化 ⇒ 全树重算 bounds）。
    bounds_epoch: u32,
    /// 上一笔交付时的代际。
    seen_bounds_epoch: u32,
    /// 消费端树生命周期代际（平台 adapter 的激活/停用回调 `bump`）。
    consumer_epoch: A11yConsumerEpoch,
    /// 上一笔**全树**交付时看到的消费端代际；不等 ⇒ 消费端已重置、下一笔必须 full。
    seen_consumer_epoch: u64,
    /// 内容整体平移的子树（滚动容器）：子孙 bounds 全变。
    subtrees: SmallVec<[NodeId; 2]>,
    /// 待重算 `bind_a11y` 的节点（只有 a11y 绑定的节点被写信号时入队）。
    ///
    /// **不进 `dirty_bind`、不跑 paint 管线**（R13-6）：这些节点没有视觉绑定，重算
    /// `A11yPatch` 不产生像素。帧入口只跑一遍 [`A11yBridge::take_bindings`] 的求值，然后
    /// 按 a11y-only 帧结算（不重建 lane、不编码、不产生 damage），TreeUpdate 由平台泵在
    /// 同一轮投递。
    bindings: Vec<NodeId>,
    /// `bindings` 的成员集合（去重）：同一批写可能重复命中同一节点。
    binding_set: ahash::HashSet<NodeId>,
    /// 本窗口累计的 a11y-only 帧数（仅 `test-support`：`FrameStats` 的断言面）。
    #[cfg(feature = "test-support")]
    a11y_only_frames: usize,
    /// 稳定身份分配器（跨帧不变；逻辑行重建后不变）。
    ids: A11yIds,
    /// 表级语义索引（每笔交付重建一次；同一笔内所有 cell 共享 O(1) 查询）。
    tables: CollectionSemanticsCache,
    /// 上次交付的焦点（已解析到存在的节点）。
    last_focus: Option<NodeId>,
    /// 已经交付给消费端的引擎节点（用于 `children` 过滤与分批的"父先于子"）。
    delivered: ahash::HashSet<NodeId>,
    /// 待发节点队列（全树分片 or 增量溢出），`queue_cursor` 之前的都已发过。
    queue: Vec<NodeId>,
    /// `queue` 的读游标。
    queue_cursor: usize,
    /// **本队列里尚未交付**的候选集合（去重）。
    ///
    /// 语义是「当前待交付」，不是"本轮出现过"（E2）：节点一旦在本笔 `nodes` 里交付就移出，
    /// 之后被 `mark_a11y` / 滚动子树失效重新标脏时必须能再次入队 —— 否则脏位已被
    /// `collect_incremental` 清掉、节点又被去重挡在队列外，新值/新 bounds 永久丢失。
    queued: ahash::HashSet<NodeId>,
    /// 下一笔必须先重发的父节点（它们的 `children` 因分批暂时不完整）。
    refresh: SmallVec<[NodeId; 8]>,
    /// 累计因**每笔预算**被推迟到下一笔的 `refresh` 条目数（P2-6 的可观测指标）。
    ///
    /// 只在测试 / 测试基建构建里存在：它在生产里没有读者（"未交付剩余量"由
    /// [`A11yBridge::refresh_backlog`] 与 `pending()` 表达），但单测需要一个累计计数来区分
    /// 「本来就没活」与「活被推迟了」。
    #[cfg(test)]
    refresh_deferred: u64,
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
            consumer_epoch: A11yConsumerEpoch::new(),
            seen_consumer_epoch: 0,
            subtrees: SmallVec::new(),
            bindings: Vec::new(),
            binding_set: ahash::HashSet::default(),
            #[cfg(feature = "test-support")]
            a11y_only_frames: 0,
            ids: A11yIds::new(),
            tables: CollectionSemanticsCache::default(),
            last_focus: None,
            delivered: ahash::HashSet::default(),
            queue: Vec::new(),
            queue_cursor: 0,
            queued: ahash::HashSet::default(),
            refresh: SmallVec::new(),
            #[cfg(test)]
            refresh_deferred: 0,
            tree_info_pending: false,
            #[cfg(feature = "test-support")]
            capture: false,
        }
    }

    /// 本轮是否有 a11y 工作（O(1) 级：脏位计数 + 旗标 + 一次焦点比较）。
    pub(crate) fn pending(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        self.full_pending
            || self.bounds_epoch != self.seen_bounds_epoch
            || self.consumer_epoch_pending()
            || !self.subtrees.is_empty()
            || tree.a11y_dirty() > 0
            || self.queue_cursor < self.queue.len()
            || !self.refresh.is_empty()
            || self.focus_changed(tree, focus)
    }

    /// 消费端是否重置过而本次全树尚未交付（`App::a11y_pending` 的**不节流旁路**）。
    ///
    /// 重置后的第一笔必须立刻发：AccessKit 适配器此时停在 `Pending`，在收到全树之前 AT 看到的是
    /// **空树**；按 100 ms 节流等下一拍没有任何意义（也没有脏位可以触发补发）。
    pub(crate) fn consumer_epoch_pending(&self) -> bool {
        self.consumer_epoch.current() != self.seen_consumer_epoch
    }

    /// 消费端代际句柄（`Window::a11y_consumer_epoch` 把它交给平台 adapter 的激活/停用回调）。
    pub(crate) fn consumer_epoch(&self) -> A11yConsumerEpoch {
        self.consumer_epoch.clone()
    }

    /// 焦点是否变化且尚未交付（`App::a11y_pending` 的**不节流旁路**）。
    pub(crate) fn focus_pending(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        self.focus_changed(tree, focus)
    }

    /// 是否还有**已开始交付的**运行未走完的批次（`queue` 候选 / 待重发父节点；E5）。
    ///
    /// **不含** `full_pending` / DPI 代际（见 [`A11yBridge::rebuild_pending`]）：那是「还没开始
    /// 交付」的重建待发，用来让 `App::a11y_pending` 在**被叫醒后**立刻交付；若把它算进
    /// backlog，无辅助技术客户端时 `full_pending` 恒真（adapter Inactive ⇒ `a11y_take` 永不
    /// 执行）、平台泵的 `wake_for_a11y_backlog` 会每轮自唤醒 = 单核 100% 忙等，I3「空闲零
    /// 唤醒」失效（RE-05）。平台自唤醒只认**交付后仍真**的本函数。
    pub(crate) fn backlog(&self) -> bool {
        self.queue_cursor < self.queue.len() || !self.refresh.is_empty()
    }

    /// 是否有一轮**尚未开始交付**的全量重建（首树 / DPI 代际变化；E5）。
    ///
    /// 只用于 `App::a11y_pending` 的「被叫醒后立刻交付」不节流旁路（否则 DPI 首笔要等
    /// 100 ms 节流）；**不参与**平台泵自唤醒判定（`wake_for_a11y_backlog` 只看
    /// [`A11yBridge::backlog`]，RE-05 的忙等根因就是把二者混在一起）。
    pub(crate) fn rebuild_pending(&self) -> bool {
        self.full_pending || self.bounds_epoch != self.seen_bounds_epoch
    }

    /// 当前因每笔预算而**未交付**的重发父节点数（P2-6 的"未交付剩余量"指标）。
    ///
    /// 非 0 ⇒ 还有父节点的 `children` 未补齐：`pending()` 恒为真、平台泵的下一轮会继续发。
    /// 生产构建里这个值没有独立读者（待办本身由 `pending()` 消费）；单测用它断言"超出预算的
    /// 一定要留在下一笔，而不是被丢掉"。
    #[cfg(test)]
    pub(crate) fn refresh_backlog(&self) -> usize {
        self.refresh.len()
    }

    /// 累计因每笔预算被推迟到下一笔的重发条目数（P2-6 的计数指标，单调递增）。
    #[cfg(test)]
    pub(crate) fn deferred_refresh(&self) -> u64 {
        self.refresh_deferred
    }

    /// DPI 比例变化（bounds 全变 ⇒ 下一笔全树）。
    pub(crate) fn note_bounds_changed(&mut self) {
        self.bounds_epoch = self.bounds_epoch.wrapping_add(1);
    }

    /// 只有 `bind_a11y` 的节点被写信号：登记一次待重算（去重）。
    ///
    /// 返回 `true` = 本次新入队。调用方（`App::invalidate`）据此判断"本批至少脏了一个 owner"。
    /// **不标 `BIND`/`PAINT`**：这条通道不产生像素，帧入口只重算 patch（R13-6）。
    pub(crate) fn note_binding_work(&mut self, node: NodeId) -> bool {
        if self.binding_set.insert(node) {
            self.bindings.push(node);
            true
        } else {
            false
        }
    }

    /// 是否有待重算的 a11y 绑定（`needs_frame` 的门禁：a11y-only 写仍要出一帧来投递）。
    pub(crate) fn binding_work(&self) -> bool {
        !self.bindings.is_empty()
    }

    /// 取走本批待重算节点（帧的 a11y 绑定 pass 消费；调用方用
    /// [`A11yBridge::recycle_bindings`] 归还缓冲）。
    pub(crate) fn take_bindings(&mut self) -> Vec<NodeId> {
        self.binding_set.clear();
        std::mem::take(&mut self.bindings)
    }

    /// 归还 [`A11yBridge::take_bindings`] 借走的缓冲（容量保留，避免每帧分配）。
    pub(crate) fn recycle_bindings(&mut self, mut buffer: Vec<NodeId>) {
        buffer.clear();
        self.bindings = buffer;
    }

    /// 记一笔 a11y-only 帧（本窗口累计），返回累计值。
    #[cfg(feature = "test-support")]
    pub(crate) fn note_a11y_only_frame(&mut self) -> usize {
        self.a11y_only_frames += 1;
        self.a11y_only_frames
    }

    /// 本窗口累计的 a11y-only 帧数。
    #[cfg(feature = "test-support")]
    pub(crate) fn a11y_only_frames(&self) -> usize {
        self.a11y_only_frames
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
        // 表格结构可能已经变化：索引只在**一笔**内复用（同一笔的 arena 不会变）。
        self.tables.clear();
        // 消费端代际只读一次：本次交付要么消费掉这次重置（升成全树），要么留给下一笔。
        let consumer_epoch = self.consumer_epoch.current();
        let full_run = self.full_pending
            || self.bounds_epoch != self.seen_bounds_epoch
            || consumer_epoch != self.seen_consumer_epoch;
        if full_run {
            // 全树：消费端会整体重置树，`delivered` 从零开始。
            self.full_pending = false;
            self.seen_bounds_epoch = self.bounds_epoch;
            self.seen_consumer_epoch = consumer_epoch;
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
        // P2-6：`refresh`（"children 暂时不完整"的待重发父节点）也受**同一笔预算**约束。
        // 旧实现把整个 `refresh` 无条件塞进本批 ⇒ 单笔 `TreeUpdate` 可以远超
        // `A11Y_BATCH_BUDGET`（极端分批下 `refresh` 会积累上千个父节点，AT 侧一笔收到几千个
        // 节点的更新）。超预算的留在 `refresh` 里，`pending()` 因它非空恒为真 ⇒ 下一笔继续发
        // （分批契约不变：仍然"父先于子 + 每笔预算内"，只是本笔不再无界）。
        let mut deferred: SmallVec<[NodeId; 8]> = SmallVec::new();
        for node in std::mem::take(&mut self.refresh) {
            if batch.len() >= A11Y_BATCH_BUDGET {
                // 已卸载的节点直接丢：重发一个不存在的节点没有意义，且会让积压永不清空。
                if tree.get(node).is_ok() {
                    deferred.push(node);
                }
                continue;
            }
            if tree.get(node).is_ok() {
                push(node, &mut batch, &mut batch_set);
            }
        }
        #[cfg(test)]
        {
            self.refresh_deferred = self.refresh_deferred.saturating_add(deferred.len() as u64);
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
        // 焦点解析提前到记录构建之前：焦点节点的父链必须参与本笔的 `children` 过滤，
        // 否则父记录会把刚挂载的焦点节点裁掉（consumer 随后把焦点归入 `pending_nodes`）。
        let (focus_id, missing) = resolve_focus(tree, focus);
        if let Some(missing) = missing {
            diagnostics.push(A11yDiagnostic::MissingFocusTarget { node: missing });
        }
        let mut focus_chain: SmallVec<[NodeId; 16]> = SmallVec::new();
        if !batch_set.contains(&focus_id) {
            let mut cursor = tree.get(focus_id).ok().and_then(Node::parent);
            while let Some(parent) = cursor {
                if !batch_set.contains(&parent) {
                    focus_chain.push(parent);
                }
                cursor = tree.get(parent).ok().and_then(Node::parent);
            }
            focus_chain.reverse();
        }
        let focus_related = |engine: NodeId| engine == focus_id || focus_chain.contains(&engine);
        let mut nodes = Vec::with_capacity(batch.len());
        let mut re_refresh: SmallVec<[NodeId; 8]> = SmallVec::new();
        for node in &batch {
            if let Some(mut record) = node_a11y_with_cache(
                tree,
                *node,
                scale,
                title,
                &mut self.ids,
                &mut self.tables,
                diagnostics,
            ) {
                // `children` 只列**已交付、同批或焦点父链**的子节点（AccessKit 契约）；
                // 被省略的子节点到达时会把父节点重发一次。焦点父链必须放行：否则焦点节点
                // 即使在本次追加，父记录也不承认它是自己的孩子。
                let before = record.children.len();
                record.children.retain(|child| {
                    self.ids.engine_of(*child).is_some_and(|engine| {
                        self.delivered.contains(&engine)
                            || batch_set.contains(&engine)
                            || focus_related(engine)
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
            // E2：去重只约束**未交付**候选。本笔已交付的节点必须移出 `queued`，否则它在本轮
            // 结束前被改脏（patch / 滚动子树失效）时无法重新入队 —— `collect_incremental`
            // 已清掉脏位，而 `queued` 会挡住重发 ⇒ 更新永久丢失，直到下一次无关落点再标记。
            self.queued.remove(node);
        }
        // 下一笔的重发集 = 本笔因预算推迟的（P2-6）+ 本笔 `children` 被裁剪的。推迟项排在前面，
        // 保证积压在有限笔数内一定会被清空（不会因为持续新增的 re_refresh 而饿死）。
        self.refresh = deferred;
        self.refresh.extend(re_refresh);

        // 焦点父链（根 → 直接父）：即使祖先已经交付过也必须重发一次，因为只有这一笔的
        // `children` 才会包含新挂载的焦点节点（Chrome AXTreeSerializer 同样把新节点的
        // 父链一起序列化）。
        let mut focus_ancestors = Vec::with_capacity(focus_chain.len());
        for ancestor in &focus_chain {
            if let Some(mut record) = node_a11y_with_cache(
                tree,
                *ancestor,
                scale,
                title,
                &mut self.ids,
                &mut self.tables,
                diagnostics,
            ) {
                let before = record.children.len();
                record.children.retain(|child| {
                    self.ids.engine_of(*child).is_some_and(|engine| {
                        self.delivered.contains(&engine)
                            || batch_set.contains(&engine)
                            || focus_related(engine)
                    })
                });
                if record.children.len() != before {
                    self.refresh.push(*ancestor);
                }
                self.delivered.insert(*ancestor);
                self.queued.remove(ancestor);
                focus_ancestors.push(record);
            }
        }

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
        let mut focus_node =
            if let Some(record) = nodes.iter().find(|record| record.engine == focus_id) {
                // 焦点节点在本批：复用已经过滤过 children 的记录，避免同一节点出现两份
                // 不同 children 的副本。
                record.clone()
            } else {
                let mut record = node_a11y_with_cache(
                    tree,
                    focus_id,
                    scale,
                    title,
                    &mut self.ids,
                    &mut self.tables,
                    diagnostics,
                )
                .expect("resolve_focus 已保证焦点节点存活");
                let before = record.children.len();
                record.children.retain(|child| {
                    self.ids.engine_of(*child).is_some_and(|engine| {
                        self.delivered.contains(&engine) || batch_set.contains(&engine)
                    })
                });
                if record.children.len() != before {
                    self.refresh.push(focus_id);
                }
                record
            };
        apply_caret(&mut focus_node, caret);
        for record in &mut nodes {
            apply_caret(record, caret);
        }
        for record in &mut focus_ancestors {
            apply_caret(record, caret);
        }
        self.delivered.insert(focus_id);
        self.queued.remove(&focus_id);
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
            focus_ancestors,
            diagnostics: diagnostics.len(),
        })
    }

    /// 焦点是否与上次交付不同（陈旧焦点按"回退到根"后的值比较，避免永久 pending）。
    fn focus_changed(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        let (resolved, _) = resolve_focus(tree, focus);
        self.last_focus != Some(resolved)
    }

    /// 收集增量候选（脏位 + 滚动子树）追加到队列；清位在收集后立即做。
    ///
    /// `queued` 只挡**尚未交付**的重复候选（E2）：已交付节点在交付时已移出集合，因此这里对
    /// 它再次 `insert` 会成功、追加到队尾并在后续笔重发（同一轮里一个节点可以交付多次）。
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

/// 用窗口的 IME 插入点提示补 `text_selection`（只在没有显式选区、且节点是 `TextRun` 时）。
///
/// E4a 契约：`text_selection` **只**挂 `TextRun`（`accesskit_consumer` 要求
/// `TextPosition.node` 是 `Role::TextRun`，否则整段选区被静默丢弃）；输入框的 caret 提示由它的
/// 子 TextRun 承载。
fn apply_caret(record: &mut A11yNode, caret: Option<(NodeId, usize)>) {
    let Some((node, byte_index)) = caret else {
        return;
    };
    if record.engine != node
        || record.text_selection.is_some()
        || record.character_lengths.is_empty()
        || record.role != A11yRole::TextRun
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
    let mut nodes = Vec::with_capacity(snapshot.nodes.len() + snapshot.focus_ancestors.len() + 1);
    for record in &snapshot.nodes {
        nodes.push((AkNodeId(record.id.get()), to_accesskit(record)));
    }
    let focus_engine = snapshot.focus_node.engine;
    if !snapshot
        .nodes
        .iter()
        .any(|record| record.engine == focus_engine)
    {
        // 父链必须先于焦点节点进入本笔：父的 `children` 里包含焦点节点，consumer 才能
        // 在一次遍历里把「新挂载的焦点节点」挂到树上（Chrome AXTreeSerializer 的父链
        // 序列化同款）。焦点节点自己的 `children` 已在构建快照时按「已交付/同批」过滤。
        for record in &snapshot.focus_ancestors {
            nodes.push((AkNodeId(record.id.get()), to_accesskit(record)));
        }
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
    // 字符度量（E4b）：`character_positions` / `character_widths` 长度必须等于
    // `character_lengths`（AccessKit 硬约束），且只在单行 LTR 时由 a11y::tree 生成；这里再
    // 防御一次长度（不一致时**省略**，绝不让 consumer 读到错位的 caret 表）。
    if !record.character_positions.is_empty()
        && record.character_positions.len() == record.character_lengths.len()
        && record.character_widths.len() == record.character_lengths.len()
    {
        node.set_character_positions(record.character_positions.clone());
        node.set_character_widths(record.character_widths.clone());
        node.set_text_direction(accesskit::TextDirection::LeftToRight);
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

    /// E1：消费端重置（AT 重新激活 / adapter 停用）后，第一笔**必须是全树**，且不依赖脏位。
    ///
    /// 失败场景（修前）：平台激活回调拿到 `None` 后 AccessKit 停在 `Pending`；下一笔由脏位
    /// 驱动，是增量（`tree: None`）⇒ `accesskit_consumer::Tree::new` 直接 panic；若没有脏位，
    /// 引擎永远不交付 ⇒ AT 看到永久空树。
    ///
    /// 变异守卫：让 `take` 不检查 `consumer_epoch`（或 `pending` 不检查）⇒ 本用例变红。
    #[test]
    fn a_consumer_epoch_reset_forces_the_next_delivery_to_be_full() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();

        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full, "首次激活必须是全树");
        assert!(
            to_tree_update(&first).tree.is_some(),
            "全树笔必须带 TreeInfo"
        );

        // 正常的增量路径先跑一笔：模拟"消费端已经有一棵树"。
        tree.mark_a11y(button);
        let incremental = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!incremental.full);
        assert!(to_tree_update(&incremental).tree.is_none());
        assert!(!bridge.pending(&tree, None), "交付后必须无工作");

        // 消费端重置：没有任何脏位，也必须立即有工作。
        let epoch = bridge.consumer_epoch();
        epoch.bump();
        assert!(
            bridge.consumer_epoch_pending(),
            "重置后必须标记「待全树」（不依赖脏位）"
        );
        assert!(
            bridge.pending(&tree, None),
            "重置后 pending 必须为真，否则平台泵不会交付"
        );

        let forced = take(&mut bridge, &mut tree, None).expect("重置后的第一笔");
        assert!(forced.full, "重置后的第一笔必须是全树");
        assert!(
            to_tree_update(&forced).tree.is_some(),
            "AccessKit 要求重置后的首笔携带 TreeInfo（否则 consumer panic/空树）"
        );
        assert_eq!(forced.nodes.len(), tree.len(), "全树笔必须覆盖整棵树");
        assert!(
            !bridge.consumer_epoch_pending(),
            "全树交付后重置标记必须消费掉"
        );
        assert!(!bridge.pending(&tree, None), "消费后不得残留待办");
        assert!(
            take(&mut bridge, &mut tree, None).is_none(),
            "全树交付后无变化时不得再产出"
        );
    }

    /// E1：连续两次重置（停用后立刻重新启用）只应付一次全树，不得重复/遗漏。
    #[test]
    fn two_consumer_epoch_resets_collapse_into_one_full_delivery() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        tree.mark_a11y(button);
        let _ = take(&mut bridge, &mut tree, None).expect("增量");

        let epoch = bridge.consumer_epoch();
        epoch.bump();
        epoch.bump();
        let forced = take(&mut bridge, &mut tree, None).expect("重置后的第一笔");
        assert!(forced.full);
        assert!(to_tree_update(&forced).tree.is_some());
        assert!(!bridge.consumer_epoch_pending(), "两次重置一次交付即消费完");
    }

    /// E2：分批运行中给**已交付**节点标脏 ⇒ 它必须重新入队并在后续笔重发。
    ///
    /// 修前行为：`queued` 在整轮运行结束前都不清，已交付节点仍占着去重名额；而
    /// `collect_incremental` 已把脏位清掉 ⇒ 新值永久丢失（滚动后的 bounds 同理会停在旧值）。
    ///
    /// 变异守卫：删掉 `take` 里交付后的 `self.queued.remove(node)` ⇒ 本用例红。
    #[test]
    fn an_update_to_an_already_delivered_node_during_a_batched_run_is_redelivered() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(3000);
        for _ in 0..3000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full);
        assert_eq!(first.nodes.len(), A11Y_BATCH_BUDGET);
        assert!(first.node_for(rows[100]).is_some(), "首笔必须包含行 100");
        assert!(
            first.node_for(rows[2999]).is_none(),
            "首笔预算 2048 ⇒ 行 2999 尚未交付"
        );

        // 运行中（队列还有 953 个未交付节点）改一个**已交付**行。
        tree.set_a11y_patch(
            rows[100],
            A11yPatch {
                label: Some(SmolStr::new_static("changed")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(rows[100]);

        let second = take(&mut bridge, &mut tree, None).expect("第二笔");
        assert!(!second.full, "运行中不得退化成全树");
        assert_eq!(
            second
                .node_for(rows[100])
                .expect("已交付行被改脏后必须重发（否则更新永久丢失）")
                .label_text(),
            Some("changed")
        );
        assert!(
            second.node_for(rows[2047]).is_some(),
            "队列照常推进：重发已交付行不得挤掉尚未交付的新节点"
        );
    }

    /// E2：滚动（`invalidate_subtree`）失效的子树里，**已交付**的节点也必须重发。
    ///
    /// 真实触发：AT 接入后的分批首树 / DPI 变化期间用户滚动一次，整棵子树 bounds 全变；
    /// 修前只有尚未交付的节点会进下一批，已交付行的 AT 高亮框/命中区域停在旧位置。
    #[test]
    fn a_subtree_invalidation_reaches_already_delivered_nodes() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("container");
        let mut rows = Vec::with_capacity(3000);
        for _ in 0..3000 {
            rows.push(
                NodeBuilder::boxed()
                    .build(&mut tree, container)
                    .expect("row"),
            );
        }
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full);
        assert!(first.node_for(rows[0]).is_some(), "行 0 已交付");
        assert!(first.node_for(rows[2999]).is_none(), "行 2999 尚未交付");

        // 内容整体平移（滚动容器）：整棵子树失效。
        bridge.invalidate_subtree(container);

        let second = take(&mut bridge, &mut tree, None).expect("第二笔");
        assert!(
            second.node_for(rows[0]).is_some(),
            "已交付行必须在滚动后重发（否则 AT 的 bounds 永久过期）"
        );
        assert!(
            second.node_for(container).is_some(),
            "容器自身也必须重发（children 的几何来自这里）"
        );
    }

    /// E2 压力：长批次运行期间持续给已交付节点写入，跑完后所有新值都必须生效，且批数收敛。
    #[test]
    fn marks_during_a_long_batched_run_are_not_starved() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(5000);
        for _ in 0..5000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full);

        // 运行期间改 100 个已交付行（首笔覆盖 rows[0..2046]）。
        let mut seen: ahash::HashMap<NodeId, Option<SmolStr>> = ahash::HashMap::default();
        for (index, row) in rows.iter().enumerate().take(100) {
            let label = smol_str::format_smolstr!("改{index}");
            tree.set_a11y_patch(
                *row,
                A11yPatch {
                    label: Some(label.clone()),
                    ..A11yPatch::default()
                },
            );
            tree.mark_a11y(*row);
            seen.insert(*row, Some(label));
        }

        let mut batches = 1usize;
        while let Some(snapshot) = take(&mut bridge, &mut tree, None) {
            batches += 1;
            assert!(
                batches < 32,
                "批次必须收敛（实际 {batches}，疑似队列膨胀死循环）"
            );
            for record in &snapshot.nodes {
                if let Some(expected) = seen.get_mut(&record.engine)
                    && record.label_text() == expected.as_deref()
                {
                    *expected = None;
                }
            }
        }
        let missing: Vec<NodeId> = seen
            .iter()
            .filter_map(|(row, expected)| expected.as_ref().map(|_| *row))
            .collect();
        assert!(
            missing.is_empty(),
            "运行期间写入的 100 个新值必须全部送达，缺失节点：{missing:?}（共 {batches} 笔）"
        );
        assert!(
            batches <= 2 * 5000_usize.div_ceil(A11Y_BATCH_BUDGET) + 8,
            "批数必须收敛在上界内（防队列膨胀活锁），实际 {batches} 笔"
        );
        assert!(!bridge.pending(&tree, None), "收敛后不得残留待办");
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

    /// P0 回归：焦点节点带未交付子节点，且父链当前不在本批时，必须真正走
    /// `focus_ancestors` 重发分支（不是只靠本批父记录的 `focus_related` 放行）。
    #[cfg(feature = "test-support")]
    #[test]
    fn a_focus_target_with_a_deep_parent_chain_is_a_valid_tree_update() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(6000);
        for _ in 0..6000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        // 焦点节点挂在最后一行下面：首笔 2048 节点只能交付 root + 前 2047 行；
        // 第二笔的 batch 仍不包含最后一行，因此焦点父链必然非空。
        let focus_parent = rows[5999];
        let focus = NodeBuilder::boxed()
            .build(&mut tree, focus_parent)
            .expect("focus node");
        let _cell = NodeBuilder::boxed().build(&mut tree, focus).expect("cell");

        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        let mut shadow = crate::test_support::a11y::ShadowTree::new();
        shadow
            .apply(&to_tree_update(&first))
            .expect("首批 TreeUpdate 必须合法");

        let snapshot = take(&mut bridge, &mut tree, Some(focus)).expect("焦点笔");
        assert!(
            snapshot.node_for(focus).is_none(),
            "前置条件：焦点节点不在本批 nodes 里"
        );
        assert!(
            !snapshot.focus_ancestors.is_empty(),
            "前置条件：焦点父链不在本批，必须走 focus_ancestors 分支"
        );
        let update = to_tree_update(&snapshot);
        crate::test_support::a11y::assert_contract_holds(&mut shadow, &update);
    }

    /// P0 回归：跨 `A11yBridge::take` 必须清掉表格结构索引；增行后旧行数不得复用。
    #[cfg(feature = "test-support")]
    #[test]
    fn table_semantics_cache_is_invalidated_between_deliveries() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        for _ in 0..2 {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .build(&mut tree, table)
                .expect("row");
            let _cell = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Cell),
                    ..A11yOverride::default()
                })
                .build(&mut tree, row)
                .expect("cell");
        }

        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        assert_eq!(
            first.node_for(table).and_then(|node| node.row_count),
            Some(2),
            "首批必须看到 2 行"
        );

        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .build(&mut tree, table)
            .expect("row");
        let _cell = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Cell),
                ..A11yOverride::default()
            })
            .build(&mut tree, row)
            .expect("cell");
        tree.mark_a11y(table);

        let second = take(&mut bridge, &mut tree, None).expect("增行后的交付");
        assert_eq!(
            second.node_for(table).and_then(|node| node.row_count),
            Some(3),
            "新一笔交付必须重建表格索引，不能复用 2 行的旧缓存"
        );
    }

    /// P0 回归：焦点节点带未交付子节点时，整笔 `TreeUpdate` 必须仍满足 AccessKit 契约。
    ///
    /// 旧实现把 `focus_node` 原样追加：它的 `children` 里含尚未交付的 cell，consumer 会把
    /// 这些 id 归入 `pending_children` 并 panic；同时焦点节点自身也可能没有父链记录
    /// （`pending_nodes` panic）。本用例用 6000 行 + 焦点落在最后一行来同时命中两条路径。
    #[cfg(feature = "test-support")]
    #[test]
    fn a_focus_target_with_undelivered_children_is_a_valid_tree_update() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(6000);
        for _ in 0..6000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let focus = rows[5999];
        let _cell = NodeBuilder::boxed().build(&mut tree, focus).expect("cell");

        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        let mut shadow = crate::test_support::a11y::ShadowTree::new();
        shadow
            .apply(&to_tree_update(&first))
            .expect("首批 TreeUpdate 必须合法");

        let snapshot = take(&mut bridge, &mut tree, Some(focus)).expect("焦点笔");
        assert!(
            snapshot.node_for(focus).is_none(),
            "前置条件：焦点节点不在本批 nodes 里"
        );
        assert_eq!(
            tree.get(focus).expect("focus row").children().len(),
            1,
            "前置条件：焦点节点必须带一个未交付子节点"
        );
        let update = to_tree_update(&snapshot);
        crate::test_support::a11y::assert_contract_holds(&mut shadow, &update);
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
    /// **P2-6**：`refresh`（"children 暂时不完整"的待重发父节点）也必须受**每笔预算**约束。
    ///
    /// 旧实现把 `bridge.refresh` 整个塞进本批 ⇒ 单笔 `TreeUpdate` 的节点数可以远超
    /// `A11Y_BATCH_BUDGET`（AT 侧一笔收到几千个节点 = UI 线程被钉住、AccessKit 更新超时）。
    /// 现在超预算的留在 `refresh` 里（下一笔继续），并计入 `deferred_refresh`。
    #[test]
    fn the_refresh_set_is_capped_by_the_batch_budget() {
        const EXTRA: usize = 17;
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut nodes = Vec::with_capacity(A11Y_BATCH_BUDGET + EXTRA);
        for _ in 0..(A11Y_BATCH_BUDGET + EXTRA) {
            nodes.push(NodeBuilder::boxed().build(&mut tree, root).expect("node"));
        }
        let mut bridge = A11yBridge::new();
        // 先把首笔全树**发完**（首笔也受预算约束 ⇒ 要分几笔），这样 `refresh` / `queue` 从空开始。
        let mut full_frames = 0usize;
        while take(&mut bridge, &mut tree, None).is_some() {
            full_frames += 1;
            assert!(full_frames < 16, "首笔全树的分批没有收敛");
        }
        assert_eq!(bridge.refresh_backlog(), 0, "首笔发完后不得有积压");
        // 手动制造积压：等价于「上一笔把这一批父节点的 children 裁掉了」。
        bridge.refresh.extend(nodes.iter().copied());

        let batch = take(&mut bridge, &mut tree, None).expect("增量笔");
        assert!(
            batch.nodes.len() <= A11Y_BATCH_BUDGET,
            "单笔节点数超预算：{} > {A11Y_BATCH_BUDGET}",
            batch.nodes.len()
        );
        assert_eq!(
            bridge.refresh_backlog(),
            EXTRA,
            "超出预算的 {} 条必须留在下一笔（实际 {}）",
            EXTRA,
            bridge.refresh_backlog()
        );
        assert_eq!(
            bridge.deferred_refresh(),
            EXTRA as u64,
            "推迟条数必须被计数"
        );
        assert!(
            bridge.pending(&tree, None),
            "还有积压时 pending 必须为真，否则泵不会再发下一笔"
        );

        // 下一笔把积压发完（不是丢弃）：剩余 17 条全部交付、积压清零。
        let rest = take(&mut bridge, &mut tree, None).expect("尾笔");
        assert_eq!(rest.nodes.len(), EXTRA, "积压必须被交付，不能丢");
        assert_eq!(bridge.refresh_backlog(), 0);
        assert!(!bridge.pending(&tree, None), "清空之后不得再有工作");
    }
    /// E9：虚拟化表格的**增量**交付 —— 行一直在 `Table` 下，行挂载 / 卸载只重发变更行与
    /// 它的逻辑祖先（不是全树），拖动窗口不丢行、也不重复发整棵树。
    #[cfg(feature = "test-support")]
    #[test]
    fn a_virtualized_table_keeps_rows_under_the_table_across_incremental_updates() {
        use crate::{
            a11y::A11yRole,
            interaction::VirtualSlot,
            test_support::a11y::{ShadowTree, assert_contract_holds},
        };

        fn mount_row(tree: &mut NodeTree, viewport: NodeId, item: usize) -> (NodeId, NodeId) {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    label: Some(smol_str::format_smolstr!("row {item}")),
                    ..A11yOverride::default()
                })
                .build(tree, viewport)
                .expect("row");
            let column = NodeBuilder::boxed().build(tree, row).expect("column");
            let cell = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Cell),
                    label: Some(smol_str::SmolStr::new_static("42")),
                    ..A11yOverride::default()
                })
                .build(tree, column)
                .expect("cell");
            (row, cell)
        }

        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let list = NodeBuilder::boxed().build(&mut tree, table).expect("list");
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, list)
            .expect("viewport");
        let (row0, _cell0) = mount_row(&mut tree, viewport, 0);
        let (row1, _cell1) = mount_row(&mut tree, viewport, 1);
        let mut slot = VirtualSlot::new(20.0, 0);
        slot.count = 50;
        slot.mounted = vec![(0, row0), (1, row1)];
        tree.ensure_interaction(viewport)
            .expect("viewport interaction")
            .virtual_list = Some(slot);

        let mut bridge = A11yBridge::new();
        let mut shadow = ShadowTree::new();
        let mut stable: ahash::HashMap<NodeId, crate::a11y::A11yNodeId> = ahash::HashMap::default();
        // 首笔全树受预算约束 ⇒ 分几笔发完，每一笔都必须合法。
        let mut batches = 0usize;
        while let Some(snapshot) = take(&mut bridge, &mut tree, None) {
            batches += 1;
            assert!(batches < 16, "首笔全树的分批没有收敛");
            for record in &snapshot.nodes {
                stable.insert(record.engine, record.id);
            }
            assert_contract_holds(&mut shadow, &to_tree_update(&snapshot));
        }
        assert!(stable.contains_key(&table) && stable.contains_key(&row0));

        // 改一行的 label：增量 = 变更行 + 物理祖先链（其中含逻辑父 Table），不是全树。
        tree.set_a11y_patch(
            row0,
            A11yPatch {
                label: Some(smol_str::SmolStr::new_static("row 0（改）")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(row0);
        let patch = take(&mut bridge, &mut tree, None).expect("增量笔");
        assert!(!patch.full, "单行变化不得退化成全树");
        assert!(
            patch.nodes.len() <= 8,
            "增量只带变更行 + 祖先链（实际 {} 个节点）",
            patch.nodes.len()
        );
        let row_record = patch.node_for(row0).expect("变更行必须在本笔");
        let table_record = patch
            .node(row_record.parent.expect("行必须有 a11y 父"))
            .expect("逻辑父（Table）必须与变更行同批");
        assert_eq!(table_record.engine, table);
        assert_eq!(
            table_record.role,
            A11yRole::Table,
            "行的 a11y 父角色必须是 Table（不是 List 行窗口）"
        );
        assert_contract_holds(&mut shadow, &to_tree_update(&patch));

        // 行窗口滚一行：挂载新行 ⇒ 表必须重发 children 且新行在表下。
        let (row2, _cell2) = mount_row(&mut tree, viewport, 2);
        tree.virtual_slot_mut(viewport)
            .expect("virtual slot")
            .mounted
            .push((2, row2));
        let patch = take(&mut bridge, &mut tree, None).expect("挂载行笔");
        assert!(!patch.full);
        let table_record = patch.node_for(table).expect("表必须重发 children");
        let row2_record = patch.node_for(row2).expect("新行必须在本笔");
        assert!(
            table_record.children.contains(&row2_record.id),
            "新行的 a11y 父必须是表（表的 children 必须包含它）"
        );
        assert_contract_holds(&mut shadow, &to_tree_update(&patch));

        // 行窗口滚出：卸载行 ⇒ 表重发 children 且不含该行，整笔仍满足 AccessKit 契约。
        let gone = stable.get(&row1).copied();
        tree.remove(row1).expect("unmount row");
        tree.virtual_slot_mut(viewport)
            .expect("virtual slot")
            .mounted
            .retain(|(_, row)| *row != row1);
        let patch = take(&mut bridge, &mut tree, None).expect("卸载行笔");
        let table_record = patch.node_for(table).expect("表必须重发 children");
        assert!(
            !table_record
                .children
                .iter()
                .any(|child| patch.node(*child).is_some_and(|node| node.engine == row1)),
            "被卸载的行不得再出现在表的 children 里"
        );
        assert_contract_holds(&mut shadow, &to_tree_update(&patch));
        if let Some(gone) = gone {
            assert!(
                !shadow
                    .summaries()
                    .contains_key(&accesskit::NodeId(gone.get())),
                "被卸载行的 a11y 节点必须从影子树里消失"
            );
        }
    }
}
