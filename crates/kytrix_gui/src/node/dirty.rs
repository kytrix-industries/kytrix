//! 脏标记（`DirtyFlags`）与失效传播的**终止判定**。
//!
//! 设计见 `design/phase-03-node-state.md` §9。位布局与「哪些变化标哪些位」的表在 §9.1/§9.3；
//! 标记与冒泡的实现在 `node::tree::NodeTree` 的 `mark_*`。
//!
//! **P03 的已知空位**：`GEOMETRY` 无写者（P08 的 `set_geometry_transform`）；`REUSABLE` 的帧初重置属 P06
//! （`NodeTree::begin_frame`），P03 只有清除者，因此 `StylePassReport::paint_dirty` 恒为 `true`。

use bitflags::bitflags;
use taffy::style::{Dimension, Display, ExpandedDimension, Position};

use crate::node::tree::Node;

bitflags! {
    /// 节点的失效集合。`u8`：6 个已用位 + 2 个保留位。
    ///
    /// 不派生 `Default`：节点的初始值是显式的 `LAYOUT | PAINT | A11Y`（新建节点必须布局、必画、必发 a11y）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct DirtyFlags: u8 {
        /// 需要重新测量/布局。
        const LAYOUT = 0b0000_0001;
        /// 需要重算动态属性（`bind_style`），并在 P06 重建 lane。
        const PAINT = 0b0000_0010;
        /// `Geometry` 的 per-draw uniform（`Transform`）需要更新；顶点不动（P08）。
        const GEOMETRY = 0b0000_0100;
        /// 无障碍节点需要重发（P11）。
        const A11Y = 0b0000_1000;
        /// 绘制序变化（浮层 / 虚拟列表 / 子节点增删）。
        const ORDER = 0b0001_0000;
        /// lane 可整桶复用（P06 的 paint pass 读它；被失效的节点清掉该位）。
        const REUSABLE = 0b0010_0000;
    }
}

/// N 的布局尺寸**是否可能**随子内容变化（保守：任一轴依赖即 true）。
///
/// 叶子节点（`Canvas`/`Geometry`）恒 `false`：它们不布局子节点。
pub(crate) fn depends_on_children(node: &Node) -> bool {
    if node.kind().is_leaf() {
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

    use super::{DirtyFlags, affects_parent, depends_on_children};
    use crate::{
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{LayoutBox, NodeTree},
        },
        paint::{canvas::Canvas, style::Style},
    };

    fn style_with(width: Dimension, height: Dimension) -> Style {
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size { width, height };
        Style { inner }
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
        assert_eq!(DirtyFlags::LAYOUT.bits(), 0b0000_0001);
        assert_eq!(DirtyFlags::PAINT.bits(), 0b0000_0010);
        assert_eq!(DirtyFlags::GEOMETRY.bits(), 0b0000_0100);
        assert_eq!(DirtyFlags::A11Y.bits(), 0b0000_1000);
        assert_eq!(DirtyFlags::ORDER.bits(), 0b0001_0000);
        assert_eq!(DirtyFlags::REUSABLE.bits(), 0b0010_0000);
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
        assert!(!flags.contains(DirtyFlags::ORDER));
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
        let canvas = NodeBuilder::canvas(Canvas::new(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0)))
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
                bounds: crate::math::Rect::new(0.0, 0.0, 4.0, 4.0),
            },
            LayoutBox::default(),
        );
        assert!(tree.mark_layout(child));
        assert!(dirty(&tree, parent).contains(DirtyFlags::LAYOUT));
        let damage_before = tree.get(root).expect("存在").damage();
        assert_eq!(
            tree.get(child).expect("存在").damage(),
            crate::math::Rect::new(0.0, 0.0, 4.0, 4.0),
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
                bounds: crate::math::Rect::new(2.0, 3.0, 10.0, 10.0),
            },
            LayoutBox::default(),
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
}
