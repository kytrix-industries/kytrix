//! 脏标记（`DirtyFlags`）、反向脏集合（[`DirtySet`]）与失效传播的**终止判定**。
//!
//! 设计参照 Flutter `PipelineOwner._nodesNeedingLayout` / `_nodesNeedingPaint`、Blink 的
//! `LayoutObject` 位 + `markContainerChainForLayout`、Chrome cc 的脏层集合：
//! **每个脏位只有一个写者集合、一个消费者、一个清除点**，门禁（"有没有脏"）必须是 O(1)。
//!
//! # 脏位表（唯一消费者 / 唯一清除点）
//!
//! | 位 | 写者（`mark_*`） | 唯一消费者 | 唯一清除点 | 反向集合 |
//! |---|---|---|---|---|
//! | [`DirtyFlags::BIND`] | `App::invalidate`（写信号）/ `App::tick_animations` / `NodeTree::mark_bind` | `App::refresh_styles` | `NodeTree::clear_bind`（求值完一个节点后） | `dirty_bind` |
//! | [`DirtyFlags::LAYOUT`] | `mark_layout` / `mark_structure` / `invalidate_layout` / `touch_layout_only` | taffy 布局 pass（读写回本地盒） | `set_local_layout`（taffy 写回该节点的 `Layout` 时） | `dirty_layout` |
//! | [`DirtyFlags::PAINT`] | `mark_paint` / 任何 `touch(LAYOUT\|PAINT)` | `PaintPass`（`rebuild_lane` / `hide_subtree`） | `NodeTree::clear_paint`（paint pass 消费 lane 后） | `dirty_paint` |
//! | [`DirtyFlags::A11Y`] | `mark_a11y` / `mark_layout` / `mark_structure` / `mark_moved_a11y` / 新建节点 | `A11yBridge::take`（a11y 交付） | `NodeTree::clear_a11y` | `dirty_a11y` |
//! | [`DirtyFlags::REUSABLE`] | `PaintPass`（重建/隐藏后）/ `settle_frame` | 不是待办位：`PaintPass` 的整桶复用判据 | `touch`（含 `LAYOUT\|PAINT`）/ `place_subtree`（移动） | —（位本身即状态） |
//!
//! **帧尾结算集合**（不是位，是"本帧动过、需要把 damage 归零 / 置 `REUSABLE`"的节点）：
//! 写者 = [`NodeTree::touch`]（含 `LAYOUT|PAINT` 时）/ `place_subtree`（位置或尺寸变了）/
//! 新建节点；唯一消费者与清除点 = `NodeTree::settle_frame`（`Window::commit_frame` 在 paint
//! 消费 lane 与 damage **之后**调用）。`settle_frame` 只遍历该集合，不再扫全树。
//!
//! # 为什么不是"每帧初重置"
//!
//! `REUSABLE` 的语义是"本帧结算时，该节点的 lane 与树/样式一致"。它在 **paint pass** 里被
//! 置位、在 `touch`/`place_subtree` 里被清除，并由 `settle_frame` 在帧尾对"本帧动过的节点"
//! 再次置位 —— 这是 **commit-on-frame-end**（提交式），不是帧初重置：在 `App::frame` 的
//! 布局后阶段（`rewindow_virtuals` / `clamp_scroll_offsets`）仍然允许标脏，那批脏位必须活到
//! 下一次布局/paint，否则虚拟列表回收行会停在 0×0（R03-9 现象 B）。
//!
//! `BIND` 与 `PAINT` 分开是 R03-9 的修复：旧实现让 `refresh_styles` 用 `PAINT` 当"需要重算
//! 动态属性"的判据并顺手清掉它，于是"没有绑定的节点被标 `PAINT`"（动画参与者 / `set_style`）
//! 会丢掉绘制请求。现在 `refresh_styles` 只碰 `BIND`，`PAINT` 只由 paint pass 消费。

use bitflags::bitflags;
use taffy::style::{Dimension, Display, ExpandedDimension, Position};

use crate::node::{id::NodeId, tree::Node};

bitflags! {
    /// 节点的失效集合。`u8`：5 个已用位 + 3 个保留位。
    ///
    /// 不派生 `Default`：节点的初始值是显式的 `LAYOUT | PAINT | A11Y`（新建节点必须布局、
    /// 必画、必发 a11y）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct DirtyFlags: u8 {
        /// 动态属性（`bind_style` / `bind_text` / `bind_a11y`）需要重新求值。
        ///
        /// 写者：`App::invalidate`（写信号标脏）、`App::tick_animations`、`NodeTree::mark_bind`。
        /// 唯一消费者 + 清除点：`App::refresh_styles`。
        const BIND = 0b0000_0001;
        /// 需要重新测量/布局。
        ///
        /// 唯一消费者 + 清除点：`set_local_layout`（taffy 写回该节点的 `Layout` 时）。
        const LAYOUT = 0b0000_0010;
        /// 绘制输出（lane）需要重建。
        ///
        /// 唯一消费者 + 清除点：`PaintPass`（`clear_paint`）。
        const PAINT = 0b0000_0100;
        /// 无障碍节点需要重发。
        ///
        /// 唯一消费者 + 清除点：`A11yBridge::take`（`clear_a11y`）。
        const A11Y = 0b0000_1000;
        /// lane 可整桶复用（`PaintPass` 的复用判据；任何绘制输入变化都清掉它）。
        ///
        /// 不是待办位：置位者是 `PaintPass` 与 `settle_frame`，清除者是 `touch` /
        /// `place_subtree`。
        const REUSABLE = 0b0001_0000;
    }
}

/// 一个**去重的插入序**节点集合（`Vec` 保存遍历顺序 + `ahash::HashSet` 做 O(1) 判重）。
///
/// 为什么不直接 `ahash::HashSet<NodeId>`：消费顺序必须确定（同一帧内先标脏的节点先处理，
/// 帧记录/诊断可复现），而 `HashSet` 的迭代顺序取决于哈希种子。`Vec` 里可能残留已被
/// `remove` 的条目（删除是 O(1) 的 `set` 操作），迭代时以 `live` 为准。
///
/// 复杂度：插入/删除/判空/长度都是 O(1)（均摊），`take_into` 是 O(条目数)。
#[derive(Debug, Default)]
pub(crate) struct DirtySet {
    /// 插入序（可能含已被移除的陈旧条目；`live` 是权威）。
    entries: Vec<NodeId>,
    live: ahash::HashSet<NodeId>,
}

impl DirtySet {
    /// 空集合。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 插入；返回 `true` = 该节点之前不在集合里（新工作）。
    pub(crate) fn insert(&mut self, node: NodeId) -> bool {
        if self.live.insert(node) {
            self.entries.push(node);
            true
        } else {
            false
        }
    }

    /// 移除；返回 `true` = 之前确实在集合里。
    pub(crate) fn remove(&mut self, node: NodeId) -> bool {
        self.live.remove(&node)
    }

    /// O(1) 的"有没有待办"判据（所有帧门禁的唯一读点）。
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// 待办条目数（`StylePassReport` 等诊断的读点；不再为它扫全树）。
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.live.len()
    }

    /// 快照存活条目（插入序；**不**修改集合）；`out` 的容量保留。
    ///
    /// 消费者：`refresh_styles`。这里刻意**不**清空集合：求值过程中可能 panic（应用闭包），
    /// 展开后未处理的节点必须仍在集合里，下一次 pass 才能接着处理（R03-12 的 panic-safety 面）。
    /// 每个节点被真正消费时由 `clear_bind` 单独出集合。
    pub(crate) fn snapshot_into(&mut self, out: &mut Vec<NodeId>) {
        out.clear();
        out.extend(
            self.entries
                .iter()
                .copied()
                .filter(|node| self.live.contains(node)),
        );
    }

    /// 取出全部存活条目（插入序），并清空集合；`out` 与自身的容量都保留。
    pub(crate) fn take_into(&mut self, out: &mut Vec<NodeId>) {
        out.clear();
        out.extend(self.entries.drain(..).filter(|node| self.live.remove(node)));
        self.live.clear();
    }

    /// 清空（容量保留；仅测试用：生产路径都经 `take_into` 消费）。
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.live.clear();
    }

    /// 压缩陈旧条目（`remove` 之后 `entries` 会留洞；集合很大时回收）。
    ///
    /// 只在集合接近空、但 `entries` 明显过大时调用（例如一次大范围卸载把脏集合清空）。
    pub(crate) fn shrink_stale(&mut self) {
        if self.entries.len() > self.live.len() * 2 {
            self.entries.retain(|node| self.live.contains(node));
        }
    }
}

/// N 的布局尺寸**是否可能**随子内容变化（保守：任一轴依赖即 true）。
///
/// 叶子节点（`Canvas`/`Geometry`）恒 `false`：它们不布局子节点。
/// 带 `LayoutOverride` 的节点（P04）同样恒 `false`：布局盒由应用给定，跳过了 taffy 的子布局。
pub(crate) fn depends_on_children(node: &Node) -> bool {
    if node.kind().is_leaf() || node.layout_override().is_some() {
        return false;
    }
    let size = node.style().inner.size;
    axis_depends_on_children(size.width) || axis_depends_on_children(size.height)
}

/// 单轴判定：`Length`/`Percent`/`Stretch`/`calc()` 都由**父**决定，与子内容无关。
fn axis_depends_on_children(dimension: Dimension) -> bool {
    match dimension.expand() {
        ExpandedDimension::Length(_)
        | ExpandedDimension::Percent(_)
        | ExpandedDimension::Stretch
        | ExpandedDimension::Calc(_) => false,
        // Auto / MinContent / MaxContent / FitContent* / Content（以及未来变体）→ 保守为 true。
        _ => true,
    }
}

/// N 出流或隐藏时，它的尺寸/位置变化**不影响**父节点的布局（冒泡的起点条件）。
pub(crate) fn affects_parent(node: &Node) -> bool {
    !matches!(node.style().inner.display, Display::None)
        && !matches!(
            node.style().inner.position,
            Position::Absolute | Position::Fixed
        )
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use taffy::style::{Dimension, Position};

    use super::{DirtyFlags, DirtySet, affects_parent, depends_on_children};
    use crate::{
        math::Rect,
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{LayoutBox, NodeTree},
        },
        paint::{
            canvas::Canvas,
            style::{Clip, Style},
        },
    };

    fn style_with(width: Dimension, height: Dimension) -> Style {
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size { width, height };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    fn auto_style() -> Style {
        style_with(Dimension::auto(), Dimension::auto())
    }

    fn chain(tree: &mut NodeTree) -> (NodeId, NodeId, NodeId) {
        let root = tree.root();
        let parent = NodeBuilder::boxed()
            .style(auto_style())
            .build(tree, root)
            .expect("父节点挂载成功");
        let child = NodeBuilder::boxed()
            .style(auto_style())
            .build(tree, parent)
            .expect("子节点挂载成功");
        (root, parent, child)
    }

    fn dirty(tree: &NodeTree, node: NodeId) -> DirtyFlags {
        tree.get(node).expect("节点存在").dirty()
    }

    #[test]
    fn dirty_flag_bit_values() {
        assert_eq!(DirtyFlags::BIND.bits(), 0b0000_0001);
        assert_eq!(DirtyFlags::LAYOUT.bits(), 0b0000_0010);
        assert_eq!(DirtyFlags::PAINT.bits(), 0b0000_0100);
        assert_eq!(DirtyFlags::A11Y.bits(), 0b0000_1000);
        assert_eq!(DirtyFlags::REUSABLE.bits(), 0b0001_0000);
    }

    #[test]
    fn dirty_set_dedupes_and_keeps_insertion_order() {
        let mut set = DirtySet::new();
        let a = NodeId::for_test(3, 1);
        let b = NodeId::for_test(1, 1);
        let c = NodeId::for_test(2, 1);

        assert!(set.insert(a));
        assert!(!set.insert(a), "重复插入必须返回 false");
        assert!(set.insert(b));
        assert!(set.insert(c));
        assert_eq!(set.len(), 3);
        assert!(!set.is_empty());

        assert!(set.remove(b));
        assert!(!set.remove(b));
        assert_eq!(set.len(), 2, "移除后长度立刻收敛（O(1) 门禁）");

        let mut out = Vec::new();
        set.take_into(&mut out);
        assert_eq!(out, vec![a, c], "取出顺序 = 插入序，已移除条目不得出现");
        assert!(set.is_empty(), "take 之后集合为空");
        // 取出之后可以重新插入同一节点（下一帧）。
        assert!(set.insert(a));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn dirty_set_compacts_stale_entries() {
        let mut set = DirtySet::new();
        for index in 0..16u32 {
            set.insert(NodeId::for_test(index, 1));
        }
        for index in 0..15u32 {
            set.remove(NodeId::for_test(index, 1));
        }
        set.shrink_stale();
        assert_eq!(set.len(), 1);
        let mut out = Vec::new();
        set.take_into(&mut out);
        assert_eq!(out, vec![NodeId::for_test(15, 1)]);
    }

    #[test]
    fn new_node_is_layout_paint_a11y() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("挂载成功");

        let flags = dirty(&tree, node);
        assert!(flags.contains(DirtyFlags::LAYOUT));
        assert!(flags.contains(DirtyFlags::PAINT));
        assert!(flags.contains(DirtyFlags::A11Y));
        assert!(!flags.contains(DirtyFlags::REUSABLE));
        assert!(!flags.contains(DirtyFlags::BIND));
    }

    #[test]
    fn mark_paint_does_not_bubble() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_paint(child));

        assert!(dirty(&tree, child).contains(DirtyFlags::PAINT));
        assert!(!dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
        assert!(!dirty(&tree, parent).contains(DirtyFlags::PAINT));
        assert!(!dirty(&tree, root).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn mark_paint_clears_reusable() {
        let mut tree = NodeTree::new();
        let (_, _, child) = chain(&mut tree);
        tree.set_dirty_for_test(child, DirtyFlags::REUSABLE);

        assert!(tree.mark_paint(child));

        assert!(!dirty(&tree, child).contains(DirtyFlags::REUSABLE));
        assert!(dirty(&tree, child).contains(DirtyFlags::PAINT));
    }

    #[test]
    fn mark_layout_bubbles_to_root() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_layout(child));

        assert!(dirty(&tree, child).contains(DirtyFlags::LAYOUT));
        assert!(dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
        assert!(
            dirty(&tree, root).contains(DirtyFlags::LAYOUT),
            "auto 链必须冒泡到根"
        );
    }

    #[test]
    fn bubble_stops_at_fixed_size() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.set_style_for_test(
            parent,
            style_with(Dimension::length(10.0), Dimension::length(10.0)),
        );
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_layout(child));

        assert!(
            dirty(&tree, parent).contains(DirtyFlags::LAYOUT),
            "父自己要重排"
        );
        assert!(
            !dirty(&tree, root).contains(DirtyFlags::LAYOUT),
            "固定宽高的父 → 祖先不需要重排"
        );
    }

    #[test]
    fn bubble_stops_at_stretch() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.set_style_for_test(
            parent,
            style_with(Dimension::stretch(), Dimension::stretch()),
        );
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_layout(child));

        assert!(dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
        assert!(!dirty(&tree, root).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn bubble_skips_absolute_child() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.set_style_for_test(
            child,
            style_with(Dimension::length(4.0), Dimension::length(4.0)),
        );
        tree.edit_style_for_test(child, |style| {
            style.inner.position = Position::Absolute;
        });
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_layout(child));

        assert!(dirty(&tree, child).contains(DirtyFlags::LAYOUT));
        assert!(
            !dirty(&tree, parent).contains(DirtyFlags::LAYOUT),
            "出流子节点不影响父布局"
        );
        assert!(!dirty(&tree, root).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn bubble_skips_hidden_child() {
        let mut tree = NodeTree::new();
        let (_root, parent, child) = chain(&mut tree);
        tree.edit_style_for_test(child, |style| {
            style.inner.display = taffy::style::Display::None;
        });
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_layout(child));

        assert!(dirty(&tree, child).contains(DirtyFlags::LAYOUT));
        assert!(!dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn leaf_kinds_do_not_depend_on_children() {
        // 叶子节点不布局子节点 → 尺寸与子内容无关；同时证明它自身被 mark_layout 时只标本节点。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 10.0, 10.0)))
            .build(&mut tree, root)
            .expect("挂载成功");
        let geometry = NodeBuilder::geometry(crate::paint::geometry::GeometryId::for_test(1, 1))
            .build(&mut tree, root)
            .expect("挂载成功");

        let auto = NodeBuilder::boxed()
            .style(auto_style())
            .build(&mut tree, root)
            .expect("挂载成功");
        let fixed = NodeBuilder::boxed()
            .style(style_with(Dimension::length(8.0), Dimension::percent(0.5)))
            .build(&mut tree, root)
            .expect("挂载成功");
        let mixed = NodeBuilder::boxed()
            .style(style_with(Dimension::auto(), Dimension::length(8.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(
            !depends_on_children(tree.get(canvas).expect("存在")),
            "Canvas 是叶子"
        );
        assert!(
            !depends_on_children(tree.get(geometry).expect("存在")),
            "Geometry 是叶子"
        );
        assert!(tree.get(canvas).expect("存在").kind().is_leaf());
        assert!(
            !tree.get(root).expect("存在").kind().is_leaf(),
            "root 是容器"
        );
        assert!(
            depends_on_children(tree.get(auto).expect("存在")),
            "auto/auto 必须视为依赖子内容"
        );
        assert!(
            !depends_on_children(tree.get(fixed).expect("存在")),
            "length/percent 与子内容无关"
        );
        assert!(
            depends_on_children(tree.get(mixed).expect("存在")),
            "任一轴依赖即依赖（保守）"
        );
    }

    #[test]
    fn repeated_mark_layout_is_idempotent() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.clear_all_dirty_for_test();

        tree.set_layout_for_test(
            child,
            LayoutBox {
                border: crate::math::Edges::ZERO,
                bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            },
        );
        assert!(tree.mark_layout(child));
        assert!(dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
        let damage_before = tree.get(root).expect("存在").damage();
        assert_eq!(
            tree.get(child).expect("存在").damage(),
            Rect::new(0.0, 0.0, 4.0, 4.0),
            "damage 必须是 prev ∪ layout"
        );

        assert!(tree.mark_layout(child), "重复标记仍返回 true（幂等成功）");

        assert!(dirty(&tree, root).contains(DirtyFlags::LAYOUT));
        assert_eq!(
            tree.get(root).expect("存在").damage(),
            damage_before,
            "重复标记不得放大祖先的 damage"
        );
    }

    #[test]
    fn layout_escalation_marks_fixed_size_node_ancestors() {
        // N7：固定尺寸节点因 `affects_layout` 升级时，父必须被标（早停条件不得挡掉它）。
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        tree.edit_style_for_test(child, |style| {
            style.inner.size = taffy::geometry::Size {
                width: Dimension::length(20.0),
                height: Dimension::length(20.0),
            };
        });
        assert!(!depends_on_children(tree.get(child).expect("存在")));
        tree.clear_all_dirty_for_test();

        assert!(tree.mark_layout(child));

        assert!(
            dirty(&tree, parent).contains(DirtyFlags::LAYOUT),
            "父必须被标"
        );
        assert!(
            dirty(&tree, root).contains(DirtyFlags::LAYOUT),
            "父是 auto 链 → 祖父也必须被标"
        );
        assert!(
            depends_on_children(tree.get(parent).expect("存在")),
            "父是 auto/auto ⇒ 冒泡继续（这正是早停条件为真的那一支）"
        );
        assert!(
            !depends_on_children(tree.get(child).expect("存在")),
            "触发方自己是固定尺寸（N7 的场景）"
        );

        // 早停分支：父已 LAYOUT 且依赖子内容 → 下一次不再向上重复走，但祖先仍保持 LAYOUT。
        assert!(tree.mark_layout(child));
        assert!(dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
        assert!(dirty(&tree, root).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn affects_parent_respects_out_of_flow_and_hidden() {
        let mut tree = NodeTree::new();
        let (_, _, child) = chain(&mut tree);

        assert!(
            affects_parent(tree.get(child).expect("存在")),
            "静态流内节点"
        );

        tree.edit_style_for_test(child, |style| {
            style.inner.position = Position::Fixed;
        });
        assert!(!affects_parent(tree.get(child).expect("存在")));

        tree.edit_style_for_test(child, |style| {
            style.inner.position = Position::Static;
            style.inner.display = taffy::style::Display::None;
        });
        assert!(!affects_parent(tree.get(child).expect("存在")));
    }

    #[test]
    fn mark_a11y_keeps_damage() {
        // A11Y 不改变像素：不得产生 damage、不得清 REUSABLE。
        let mut tree = NodeTree::new();
        let (_root, _parent, child) = chain(&mut tree);
        tree.set_layout_for_test(
            child,
            LayoutBox {
                border: crate::math::Edges::ZERO,
                bounds: Rect::new(2.0, 3.0, 10.0, 10.0),
            },
        );
        tree.set_dirty_for_test(child, DirtyFlags::REUSABLE);
        let before = tree.get(child).expect("存在").damage();

        assert!(tree.mark_a11y(child));

        assert!(dirty(&tree, child).contains(DirtyFlags::A11Y));
        assert_eq!(
            tree.get(child).expect("存在").damage(),
            before,
            "A11Y 不得累积 damage"
        );
        assert!(
            dirty(&tree, child).contains(DirtyFlags::REUSABLE),
            "A11Y 不得清 REUSABLE（像素没变）"
        );
    }

    #[test]
    fn mark_bind_does_not_touch_paint() {
        // R03-9：BIND 与 PAINT 分离 —— `refresh_styles` 只消费 BIND，不得清掉绘制请求。
        let mut tree = NodeTree::new();
        let (_, _, child) = chain(&mut tree);
        tree.set_dirty_for_test(child, DirtyFlags::REUSABLE);

        assert!(tree.mark_bind(child));

        assert!(dirty(&tree, child).contains(DirtyFlags::BIND));
        assert!(
            !dirty(&tree, child).contains(DirtyFlags::PAINT),
            "BIND 不是绘制请求"
        );
        assert!(
            dirty(&tree, child).contains(DirtyFlags::REUSABLE),
            "BIND 不清 REUSABLE"
        );
    }
}
