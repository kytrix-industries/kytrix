use bitflags::bitflags;
use taffy::style::{Dimension, Display, ExpandedDimension, Position};

use crate::node::{id::NodeId, tree::Node};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct DirtyFlags: u8 {
        const BIND = 0b0000_0001;
        const LAYOUT = 0b0000_0010;
        const PAINT = 0b0000_0100;
        const A11Y = 0b0000_1000;
        const REUSABLE = 0b0001_0000;
    }
}

#[derive(Debug, Default)]
pub(crate) struct DirtySet {
    entries: Vec<NodeId>,
    live: ahash::HashSet<NodeId>,
}

impl DirtySet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, node: NodeId) -> bool {
        if self.live.insert(node) {
            self.entries.push(node);
            true
        } else {
            false
        }
    }

    pub(crate) fn remove(&mut self, node: NodeId) -> bool {
        self.live.remove(&node)
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.live.len()
    }

    pub(crate) fn snapshot_into(&mut self, out: &mut Vec<NodeId>) {
        out.clear();
        out.extend(
            self.entries
                .iter()
                .copied()
                .filter(|node| self.live.contains(node)),
        );
    }

    pub(crate) fn take_into(&mut self, out: &mut Vec<NodeId>) {
        out.clear();
        out.extend(self.entries.drain(..).filter(|node| self.live.remove(node)));
        self.live.clear();
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.live.clear();
    }

    pub(crate) fn shrink_stale(&mut self) {
        if self.entries.len() > self.live.len() * 2 {
            self.entries.retain(|node| self.live.contains(node));
        }
    }
}

pub(crate) fn depends_on_children(node: &Node) -> bool {
    if node.kind().is_leaf() || node.layout_override().is_some() {
        return false;
    }
    let size = node.style().inner.size;
    axis_depends_on_children(size.width) || axis_depends_on_children(size.height)
}

fn axis_depends_on_children(dimension: Dimension) -> bool {
    match dimension.expand() {
        ExpandedDimension::Length(_)
        | ExpandedDimension::Percent(_)
        | ExpandedDimension::Stretch
        | ExpandedDimension::Calc(_) => false,
        _ => true,
    }
}

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
