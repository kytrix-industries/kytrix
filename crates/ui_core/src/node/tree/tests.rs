use smallvec::smallvec;
use smol_str::SmolStr;

use super::{LayoutBox, Node, NodeKind, NodeSpec, NodeTree};
use crate::{
    a11y::A11yRole,
    error::EngineError,
    math::{LogicalEdges, LogicalPoint, LogicalRect, px},
    node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId, signal::SignalId},
    paint::{
        color::Color,
        style::{Clip, Style, StylePatch},
        style_builder::StyleBuilder,
    },
    text::TextStyle,
    text_edit::EditableSpec,
};

#[cfg(target_os = "linux")]
const NODE_SIZE_BASELINE: usize = 328;
#[cfg(not(target_os = "linux"))]
const NODE_SIZE_BASELINE: usize = 336;

fn boxed(tree: &mut NodeTree, parent: NodeId) -> NodeId {
    NodeBuilder::boxed().build(tree, parent).expect("挂载成功")
}

#[test]
fn new_tree_has_root() {
    let tree = NodeTree::new();
    let root = tree.root();

    assert_eq!(tree.dfs_ids(), vec![root]);
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

    assert_eq!(tree.dfs_ids(), vec![root, a, a1, b]);
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
        tree.dfs_ids(),
        vec![root, a, a1, b, b1],
        "DFS 先序：重挂后的子树排在父的末尾，且不重复"
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

    assert_eq!(tree.dfs_ids(), vec![root, b]);
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
    assert_eq!(tree.dfs_ids(), vec![root], "root 必须还在");
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
            NodeSpec::new(
                NodeKind::Box,
                Style::default(),
                Some(Box::new(|_cx| StylePatch::default())),
            ),
        )
        .expect("插入成功");

    assert!(tree.take_bind_style(node).is_some(), "闭包必须被取走");
    assert!(tree.take_bind_style(node).is_none(), "取走后为 None");
    assert!(tree.put_bind_style(node, Box::new(|_cx| StylePatch::default())));
    assert!(tree.take_bind_style(node).is_some());

    assert_eq!(tree.deps(node), None, "从未求值 ⇒ 没有依赖集合");
    assert!(tree.set_deps(node, smallvec![SignalId::for_test(0)]));
    assert_eq!(tree.deps(node).map(<[SignalId]>::len), Some(1));
    assert!(tree.set_style_patch(node, StylePatch::default()));
}

#[test]
fn node_and_handle_sizes() {
    let node_size = size_of::<Node>();
    println!("size_of::<Node>() = {node_size}");
    println!("size_of::<LayoutBox>() = {}", size_of::<LayoutBox>());
    println!("size_of::<NodeKind>() = {}", size_of::<NodeKind>());
    println!(
        "size_of::<TextContent>() = {}",
        size_of::<crate::text::TextContent>()
    );
    println!(
        "size_of::<Option<LayoutOverride>>() = {}",
        size_of::<Option<crate::layout::LayoutOverride>>()
    );
    println!(
        "size_of::<Option<LaneId>>() = {}",
        size_of::<Option<crate::paint::scene::LaneId>>()
    );
    println!("size_of::<Option<Node>>() = {}", size_of::<Option<Node>>());
    println!(
        "size_of::<Option<TextSlot>>() = {}",
        size_of::<Option<super::content::TextSlot>>()
    );
    println!(
        "size_of::<Option<SmolStr>>() = {}",
        size_of::<Option<smol_str::SmolStr>>()
    );
    println!(
        "size_of::<Option<(u8, Option<DeviceRect>)>>() = {}",
        size_of::<Option<(u8, Option<crate::math::DeviceRect>)>>()
    );
    println!("size_of::<taffy::Cache>() = {}", size_of::<taffy::Cache>());
    println!(
        "size_of::<Option<Box<crate::a11y::tree::NodeA11y>>>() = {}",
        size_of::<Option<Box<crate::a11y::tree::NodeA11y>>>()
    );
    assert_eq!(
        node_size, NODE_SIZE_BASELINE,
        "Node 尺寸基线: {node_size} B（上限 336 B：冷字段必须装箱，见 R03-3；X5 起多 lane 集合占 24 B；\
         E13 的组不透明度戳 `paint_opacity` +4 B ⇒ 补齐到 336 B，**已无余量**，后续加字段须先装箱搬走冷字段；\
         Linux 的 wayland-backend 启用 smallvec/union ⇒ 少 8 B 为 328 B）"
    );
    assert_eq!(
        size_of::<crate::text::TextContent>(),
        112,
        "TextContent 的尺寸基线（v2.1 E2 的 `runs: Option<Box<ThinSlice>>` +8；E2b 的归一化诊断 \
         计数 +8。`NodeKind::Text` 装箱持有 TextContent ⇒ Node 336 B（Linux 328 B），上限 336 B）"
    );
    assert_eq!(
        size_of::<Option<NodeId>>(),
        12,
        "Option<NodeId> 与 NodeId 同尺寸（NonZeroU32 的 niche ⇒ 零开销包装）"
    );
    assert_eq!(size_of::<NodeId>(), 12, "index + 窗口 tag + 代际（R03-7）");
}

#[test]
fn layout_box_is_32_bytes() {
    assert_eq!(
        size_of::<LayoutBox>(),
        size_of::<LogicalRect>() + size_of::<LogicalEdges>()
    );
    assert_eq!(size_of::<LayoutBox>(), 32);
}

#[test]
fn root_style_is_percent_100() {
    use taffy::style::{Dimension, ExpandedDimension};

    let tree = NodeTree::new();
    let root = tree.get(tree.root()).expect("root 存在");
    let size = root.style().inner().size;

    assert!(matches!(size.width.expand(), ExpandedDimension::Percent(v) if v == 1.0));
    assert!(matches!(size.height.expand(), ExpandedDimension::Percent(v) if v == 1.0));
    assert_eq!(size.width, Dimension::percent(1.0));
}

#[test]
fn slot_reuse_drops_cache() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let doomed = boxed(&mut tree, root);
    let index = doomed.index();
    let _ = tree.taffy_cache(doomed);
    assert!(tree.layout_cache(doomed).is_some(), "缓存条目必须建立");

    tree.remove(doomed).expect("卸载成功");
    assert!(
        tree.layout_cache(doomed).is_none(),
        "卸载必须清掉槽位的缓存"
    );

    let reused = boxed(&mut tree, root);
    assert_eq!(reused.index(), index, "槽位必须复用");
    assert!(
        tree.layout_cache(doomed).is_none(),
        "复用槽位不得继承上一代节点的缓存"
    );
}

#[test]
fn set_style_clears_caches_to_root() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let parent = boxed(&mut tree, root);
    let child = boxed(&mut tree, parent);
    let sibling = boxed(&mut tree, root);
    let _ = tree
        .compute_layout(taffy::geometry::Size {
            width: Some(100.0),
            height: Some(100.0),
        })
        .expect("布局成功");
    assert!(
        tree.layout_cache(sibling).is_some(),
        "布局后兄弟必须有缓存条目"
    );

    tree.set_style(child, fixed_style(10.0, 10.0))
        .expect("节点存活");

    for node in [root, parent, child] {
        let empty = tree.layout_cache(node).is_none_or(taffy::Cache::is_empty);
        assert!(empty, "node {node:?} 的缓存必须被清空");
    }
    assert!(
        tree.layout_cache(sibling)
            .is_some_and(|cache| !cache.is_empty()),
        "无关兄弟的缓存必须保留"
    );
}

#[test]
fn remove_drops_cache_and_hoisted_entries() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let parent = boxed(&mut tree, root);
    let child = boxed(&mut tree, parent);
    let _ = tree.taffy_cache(child);
    tree.hoisted_push(parent, &[child]);

    tree.remove(parent).expect("卸载成功");

    assert!(
        tree.layout_cache(child).is_none(),
        "子树内每个节点的缓存都要清"
    );
    assert!(
        !tree.hoisted.contains_key(&parent),
        "包含块的 hoisted 列表要清"
    );
    assert!(!tree.hoisted.contains_key(&child));

    let other = boxed(&mut tree, root);
    let victim = boxed(&mut tree, root);
    tree.hoisted_push(other, &[victim]);
    tree.remove(victim).expect("卸载被记录的节点");
    assert!(
        tree.hoisted
            .get(&other)
            .is_none_or(|list| !list.contains(&victim)),
        "其它包含块的列表不得残留被卸载 id"
    );
}

#[test]
fn node_clip_reads_style() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let mut style = Style::default();
    style.clip = Clip::Bounds;
    let node = tree
        .insert_child(root, NodeSpec::new(NodeKind::Box, style, None))
        .expect("插入成功");

    assert_eq!(tree.get(node).expect("存在").clip(), Clip::Bounds);
}

#[test]
fn node_debug_prints_layout_fields() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = boxed(&mut tree, root);

    let debug = format!("{:?}", tree.get(node).expect("存在"));

    for field in [
        "layout_override",
        "local",
        "local_border",
        "laid_out_epoch",
        "measure",
        "hit_shape",
    ] {
        assert!(debug.contains(field), "Debug 缺字段 {field}: {debug}");
    }
}

#[test]
fn damage_covers_current_layout_on_touch() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = boxed(&mut tree, root);
    tree.set_layout_for_test(
        node,
        LayoutBox {
            border: LogicalEdges::ZERO,
            bounds: LogicalRect::new(px(10.0), px(0.0), px(4.0), px(4.0)),
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
        LogicalRect::new(px(10.0), px(0.0), px(4.0), px(4.0)),
        "touch 必须把当前布局盒并入 damage"
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
    for expected in [DirtyFlags::LAYOUT, DirtyFlags::PAINT, DirtyFlags::A11Y] {
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
            .contains(DirtyFlags::A11Y),
        "重挂是结构变更 ⇒ a11y 父链必须重发"
    );
}

fn text_tree(text: &str) -> (NodeTree, NodeId) {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = NodeBuilder::text(text, crate::text::TextStyle::new(px(16.0)))
        .build(&mut tree, root)
        .expect("挂载成功");
    (tree, node)
}

#[test]
fn text_kind_is_a_leaf_and_exposes_its_content() {
    let (tree, node) = text_tree("hello");

    assert!(tree.get(node).expect("存在").kind().is_leaf());
    let content = tree.text_content(node).expect("文本载荷");
    assert_eq!(content.text, "hello");
    assert!(tree.text_line(node).is_none(), "尚未 shape");
    assert!(tree.text_slot(node).is_none());
}

#[test]
fn remove_drops_the_text_slot() {
    let (mut tree, node) = text_tree("temp");
    assert!(tree.remove(node).is_ok());

    assert!(tree.text_slot(node).is_none(), "槽位必须随卸载清空（N5）");
    assert!(tree.text_line(node).is_none());
}

fn editable_spec(label: &'static str) -> EditableSpec {
    EditableSpec {
        role: A11yRole::TextInput,
        label: SmolStr::new_static(label),
        placeholder: SmolStr::new_static(""),
        multiline: false,
        masked: false,
        max_length: None,
        submit: None,
        text_style: TextStyle::new(px(16.0)),
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    }
}

fn editable_tree(label: &'static str) -> (NodeTree, NodeId) {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = NodeBuilder::editable_text(editable_spec(label))
        .build(&mut tree, root)
        .expect("挂载可编辑文本");
    (tree, node)
}

#[test]
fn remove_drops_the_editable_slot() {
    let (mut tree, node) = editable_tree("temp");
    assert!(tree.editable(node).is_some());
    assert!(tree.editable_slot_for_test(node).is_some());

    assert!(tree.remove(node).is_ok());

    assert!(
        tree.editable_slot_for_test(node).is_none(),
        "editable 槽位必须随卸载清空（N5）"
    );
    assert!(tree.editable(node).is_none());
}

#[test]
fn reused_index_replaces_the_editable_slot() {
    let (mut tree, first) = editable_tree("first");
    assert!(tree.remove(first).is_ok());

    let root = tree.root();
    let plain = NodeBuilder::text("plain", TextStyle::new(px(16.0)))
        .build(&mut tree, root)
        .expect("挂载文本");
    assert_eq!(plain.index(), first.index(), "前置：arena 必须复用 index");
    assert!(
        tree.editable_slot_for_test(plain).is_none(),
        "不可编辑节点不得继承旧槽位"
    );

    let root = tree.root();
    assert!(tree.remove(plain).is_ok());
    let second = NodeBuilder::editable_text(editable_spec("second"))
        .build(&mut tree, root)
        .expect("挂载可编辑文本");
    assert_eq!(second.index(), first.index(), "前置：arena 必须复用 index");
    let editable = tree
        .editable_slot_for_test(second)
        .expect("新可编辑节点必须重建槽位");
    assert_eq!(editable.committed(), "", "新节点的 committed 不得残留旧值");
    assert_eq!(editable.spec().label, "second", "槽位必须指向新的 spec");
    assert!(tree.editable(first).is_none(), "旧句柄不得命中新槽位");
}

#[test]
fn text_selection_rects_reports_preconditions() {
    let (tree, node) = text_tree("abcdef");

    let error = tree
        .text_selection_rects(node, 0, 3)
        .expect_err("尚未 shape 的节点必须报错");
    assert!(matches!(
        error,
        EngineError::TextShape { node: reported, .. } if reported == node
    ));

    let root = tree.root();
    let error = tree
        .text_selection_rects(root, 0, 3)
        .expect_err("非文本节点必须报错");
    assert!(matches!(error, EngineError::TextShape { .. }));
}

#[test]
fn text_caret_rect_reports_preconditions() {
    let (tree, node) = text_tree("abcdef");

    let error = tree
        .text_caret_rect(node, 0)
        .expect_err("尚未 shape 的节点必须报错");
    assert!(matches!(
        error,
        EngineError::TextShape { node: reported, .. } if reported == node
    ));

    let root = tree.root();
    let error = tree
        .text_caret_rect(root, 0)
        .expect_err("非文本节点必须报错");
    assert!(matches!(error, EngineError::TextShape { .. }));
}

#[test]
fn text_byte_index_at_reports_preconditions() {
    let (mut tree, node) = text_tree("abcdef");
    let root = tree.root();
    let point = LogicalPoint::new(px(1.0), px(1.0));

    let error = tree
        .text_byte_index_at(node, point)
        .expect_err("尚未 shape 的文本节点必须报错");
    assert!(matches!(
        error,
        EngineError::StaleNode { node: reported } if reported == node
    ));

    let error = tree
        .text_byte_index_at(root, point)
        .expect_err("盒子节点不是文本命中目标");
    assert!(matches!(
        error,
        EngineError::StaleNode { node: reported } if reported == root
    ));

    let canvas = NodeBuilder::canvas(crate::paint::canvas::Canvas::default())
        .build(&mut tree, root)
        .expect("挂载 Canvas 节点");
    let error = tree
        .text_byte_index_at(canvas, point)
        .expect_err("Canvas 节点不是文本命中目标");
    assert!(matches!(
        error,
        EngineError::StaleNode { node: reported } if reported == canvas
    ));

    assert!(tree.remove(node).is_ok());
    let error = tree
        .text_byte_index_at(node, point)
        .expect_err("已卸载节点必须报错");
    assert!(matches!(
        error,
        EngineError::StaleNode { node: reported } if reported == node
    ));
}

#[test]
fn text_content_updates_only_text_nodes() {
    let (mut tree, node) = text_tree("one");
    let root = tree.root();

    assert!(tree.set_text_content(
        node,
        crate::text::TextContent::new("two", crate::text::TextStyle::new(px(12.0)))
    ));
    assert_eq!(tree.text_content(node).expect("文本").text, "two");
    assert!(
        !tree.set_text_content(
            root,
            crate::text::TextContent::new("nope", crate::text::TextStyle::default())
        ),
        "非文本节点必须拒绝写载荷"
    );
}

#[test]
fn text_bind_text_round_trips() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = NodeBuilder::text("x", crate::text::TextStyle::new(px(16.0)))
        .bind_text(|_cx| smol_str::SmolStr::new_static("bound"))
        .build(&mut tree, root)
        .expect("挂载成功");

    let closure = tree.take_bind_text(node).expect("闭包必须存在");
    assert!(tree.take_bind_text(node).is_none(), "取走后为空");
    assert!(tree.put_bind_text(node, closure));
    assert!(tree.take_bind_text(node).is_some(), "放回后仍可取");
}

fn layout_box(bounds: LogicalRect) -> LayoutBox {
    LayoutBox {
        bounds,
        border: LogicalEdges::ZERO,
    }
}

#[test]
fn settle_frame_resets_damage_bits_and_prev_layout() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = boxed(&mut tree, root);
    let _ = tree.set_layout_for_test(
        child,
        layout_box(LogicalRect::new(px(1.0), px(2.0), px(3.0), px(4.0))),
    );
    let _ = tree.mark_paint(child);
    let _ = tree.mark_a11y(child);
    let flags = tree.get(child).expect("存在").dirty();
    assert!(flags.contains(DirtyFlags::PAINT));
    assert!(!tree.get(child).expect("存在").damage().is_empty());

    tree.settle_frame();

    let node = tree.get(child).expect("存在");
    assert!(node.damage().is_empty(), "damage 归零");
    assert!(
        !node.layout().bounds.is_empty(),
        "布局盒保留（不再有 prev_layout 字段）"
    );
    assert!(node.dirty().contains(DirtyFlags::REUSABLE));
    assert!(node.dirty().contains(DirtyFlags::PAINT));
    assert!(node.dirty().contains(DirtyFlags::LAYOUT));
    assert!(
        node.dirty().contains(DirtyFlags::A11Y),
        "A11Y 的消费者不在本帧（P11），结算必须保留"
    );
}

#[test]
fn remove_unions_subtree_bounds_into_parent_damage() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let parent = boxed(&mut tree, root);
    let child = boxed(&mut tree, parent);
    let _ = tree.set_layout_for_test(
        parent,
        layout_box(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0))),
    );
    let outside = LogicalRect::new(px(100.0), px(100.0), px(20.0), px(20.0));
    let _ = tree.set_layout_for_test(child, layout_box(outside));
    tree.settle_frame();

    tree.remove(child).expect("卸载成功");

    let damage = tree.get(parent).expect("存在").damage();
    assert!(
        damage.union(outside) == damage,
        "父 damage 必须覆盖被卸载子树的旧盒：{damage:?}"
    );
}

#[test]
fn remove_reports_released_lanes() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let parent = boxed(&mut tree, root);
    let child = boxed(&mut tree, parent);
    let lane = crate::paint::scene::LaneId::for_test(0, 1);
    tree.set_lane(parent, crate::paint::scene::LaneId::for_test(1, 1));
    tree.set_lane(child, lane);

    tree.remove(parent).expect("卸载成功");

    let removed: Vec<_> = tree.drain_removed_lanes().collect();
    assert_eq!(removed.len(), 2, "父 + 子各一条 lane");
    assert!(removed.contains(&lane));
    assert_eq!(tree.drain_removed_lanes().count(), 0, "取走即清空");
}

#[test]
fn remove_reports_every_lane_of_a_multi_lane_node() {
    use crate::paint::scene::LaneId;

    let mut tree = NodeTree::new();
    let root = tree.root();
    let parent = boxed(&mut tree, root);
    let node = boxed(&mut tree, parent);
    let lanes: Vec<LaneId> = (0..3).map(|chunk| LaneId::for_test(chunk, 1)).collect();
    tree.set_lane(node, lanes[0]);
    tree.push_lane(node, lanes[1]);
    tree.push_lane(node, lanes[2]);
    let parent_lane = LaneId::for_test(9, 1);
    tree.set_lane(parent, parent_lane);

    tree.remove(parent).expect("卸载多块几何节点");

    let removed: Vec<LaneId> = tree.drain_removed_lanes().collect();
    assert_eq!(
        removed.len(),
        4,
        "父 1 条 + 子节点 3 条块 lane 必须全部回收"
    );
    for lane in &lanes {
        assert!(removed.contains(lane), "块 lane {lane:?} 泄漏");
    }
    assert!(removed.contains(&parent_lane));
    assert_eq!(tree.drain_removed_lanes().count(), 0, "取走即清空");
}

#[test]
fn canvas_referenced_geometries_are_bound_and_unbound() {
    use crate::paint::{
        canvas::Canvas,
        color::Color,
        geometry::GeometryId,
        primitive::{GeometryDraw, Path},
    };

    let first = GeometryId::for_test(1, 1);
    let second = GeometryId::for_test(2, 1);
    let third = GeometryId::for_test(3, 1);
    let draw = |id| GeometryDraw::full(id, crate::math::Transform::IDENTITY, Color::WHITE);
    let path = |id| Path {
        bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
        geometry: id,
        fill: Color::WHITE,
    };
    let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)));
    canvas.push_geometry(draw(first));
    canvas.push_geometry(draw(first));
    canvas.push_path(path(third));
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = NodeBuilder::canvas(canvas)
        .build(&mut tree, root)
        .expect("挂载 Canvas");
    assert_eq!(
        tree.geometry_bindings
            .get(&first)
            .map_or(0, smallvec::SmallVec::len),
        1,
        "同一个几何在一个 Canvas 里重复引用只登记一次"
    );
    assert!(
        tree.geometry_bindings
            .get(&third)
            .is_some_and(|nodes| nodes.contains(&node)),
        "`push_path` 引用的几何也必须登记（第二条公开几何引用路径）"
    );
    assert!(!tree.geometry_bindings.contains_key(&second));

    let mut fresh = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)));
    fresh.push_path(path(second));
    assert!(tree.set_canvas(node, fresh), "替换 Canvas 内容");
    assert!(
        !tree.geometry_bindings.contains_key(&first),
        "旧内容引用的几何必须解绑"
    );
    assert!(
        !tree.geometry_bindings.contains_key(&third),
        "旧内容里 `push_path` 引用的几何也必须解绑"
    );
    assert!(
        tree.geometry_bindings
            .get(&second)
            .is_some_and(|nodes| nodes.contains(&node)),
        "新内容（`push_path`）引用的几何必须登记"
    );

    tree.clear_all_dirty_for_test();
    assert_eq!(tree.mark_geometry_updated(second), 1);
    let state = tree.get(node).expect("节点存在");
    assert!(
        state.dirty().contains(DirtyFlags::PAINT),
        "Canvas 节点必须被标 PAINT：{:?}",
        state.dirty()
    );
    assert!(
        !state.dirty().contains(DirtyFlags::REUSABLE),
        "Canvas 没有块级 revision 判据 ⇒ 必须清 REUSABLE 让 lane 重建"
    );

    tree.remove(node).expect("卸载 Canvas");
    assert!(
        !tree.geometry_bindings.contains_key(&second),
        "卸载后必须解绑"
    );
}

#[test]
fn debug_id_roundtrip_and_slot_reuse() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = NodeBuilder::boxed()
        .id("label")
        .build(&mut tree, root)
        .expect("挂载成功");
    assert_eq!(tree.debug_id(node), Some("label"));

    tree.remove(node).expect("卸载");
    let replacement = NodeBuilder::boxed()
        .build(&mut tree, root)
        .expect("挂载成功");
    assert_eq!(replacement.index(), node.index(), "槽位应复用");
    assert_eq!(tree.debug_id(replacement), None, "新节点不得继承 debug id");
}

#[test]
fn clip_key_roundtrip_and_clear() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = boxed(&mut tree, root);
    assert_eq!(tree.clip_key(child), None);
    let covering = crate::math::DevicePixelRatio::new(1.0)
        .expect("scale")
        .to_covering_device_rect(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)));
    tree.set_clip_key(child, (1, Some(covering)));
    assert_eq!(tree.clip_key(child), Some((1, Some(covering))));
    tree.clear_clip_key(child);
    assert_eq!(tree.clip_key(child), None);
}

#[test]
fn node_size_after_p06() {
    let size = size_of::<Node>();
    println!("size_of::<Node>() = {size}");
    assert_eq!(
        size, NODE_SIZE_BASELINE,
        "Node 尺寸基线：{size} B（备选瘦身：删掉未被读取的 `Node.id` 字段）"
    );
}

fn fixed_style(width: f32, height: f32) -> Style {
    StyleBuilder::new().size(px(width), px(height)).build()
}

fn layout_to(tree: &mut NodeTree, width: f32, height: f32) {
    let _ = tree
        .compute_layout(taffy::geometry::Size {
            width: Some(width),
            height: Some(height),
        })
        .expect("布局成功");
}

fn bounds_of(tree: &NodeTree, node: NodeId) -> LogicalRect {
    tree.get(node).expect("存在").layout().bounds
}

#[test]
fn layout_bounds_equal_the_taffy_layout() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = NodeBuilder::boxed()
        .style(fixed_style(10.3, 4.4))
        .build(&mut tree, root)
        .expect("挂载成功");
    layout_to(&mut tree, 200.0, 200.0);

    assert_eq!(
        bounds_of(&tree, child),
        LogicalRect::new(px(0.0), px(0.0), px(10.3), px(4.4)),
        "layout bounds 必须与 taffy 布局逐位相同（不得做设备像素吸附）"
    );
}

#[test]
fn a_layout_override_is_positioned_against_the_parents_logical_box() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let mut root_style = fixed_style(100.0, 100.0);
    root_style.flex_direction(taffy::style::FlexDirection::Column);
    tree.set_style(root, root_style).expect("根节点存活");
    let _spacer = NodeBuilder::boxed()
        .style(fixed_style(100.0, 10.25))
        .build(&mut tree, root)
        .expect("挂载成功");
    let parent = NodeBuilder::boxed()
        .style(fixed_style(20.0, 20.0))
        .build(&mut tree, root)
        .expect("挂载成功");
    let over = LogicalRect::new(px(0.5), px(0.75), px(5.0), px(5.0));
    let child = NodeBuilder::boxed()
        .layout_override(over)
        .build(&mut tree, parent)
        .expect("挂载成功");
    layout_to(&mut tree, 100.0, 100.0);

    let parent_origin = bounds_of(&tree, parent).origin;
    assert_eq!(
        parent_origin.y,
        px(10.25),
        "父盒原点必须保留 taffy 的小数布局值"
    );
    assert_eq!(
        bounds_of(&tree, child).origin,
        parent_origin + over.origin,
        "布局覆盖必须相对父的未吸附逻辑原点定位"
    );
}

#[test]
fn paint_closure_marks_dirty_ancestors() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let a = boxed(&mut tree, root);
    let a1 = boxed(&mut tree, a);
    let a2 = boxed(&mut tree, a);
    let b = boxed(&mut tree, root);

    tree.mark_paint_closure(&[a1]);
    assert!(tree.paint_marked(a1));
    assert!(tree.paint_marked(a));
    assert!(tree.paint_marked(root));
    assert!(!tree.paint_marked(a2), "兄弟不得入闭包");
    assert!(!tree.paint_marked(b), "无关子树不得入闭包");

    tree.mark_paint_closure(&[a2, b]);
    assert!(tree.paint_marked(a2));
    assert!(tree.paint_marked(b));
    assert!(tree.paint_marked(a));
    assert!(tree.paint_marked(root));
    assert!(!tree.paint_marked(a1));
}

#[test]
fn paint_closure_is_epoch_scoped() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = boxed(&mut tree, root);

    assert!(!tree.paint_marked(child), "没有闭包时任何节点都不在闭包里");
    tree.mark_paint_closure(&[child]);
    assert!(tree.paint_marked(child));
    tree.mark_paint_closure(&[]);
    assert!(!tree.paint_marked(child), "空闭包之后旧标记必须失效");
}

#[test]
fn structure_and_scroll_epochs_force_the_next_full_frame() {
    let mut tree = NodeTree::new();
    assert!(
        !tree.paint_epoch_changed(),
        "新树没有结构变化 ⇒ 代际判据不强制全量（首帧由 delta 的 full 与脏节点状态承担）"
    );
    tree.note_paint_frame(true, 1);
    assert!(!tree.paint_epoch_changed(), "没有变化 ⇒ 允许增量");

    let root = tree.root();
    let child = boxed(&mut tree, root);
    assert!(tree.paint_epoch_changed(), "结构变化必须全量");
    tree.note_paint_frame(true, 1);
    assert!(!tree.paint_epoch_changed());

    tree.note_scroll_container(child);
    assert!(tree.paint_epoch_changed(), "滚动容器登记必须全量");
    tree.note_paint_frame(true, 2);
    assert!(!tree.paint_epoch_changed());

    tree.force_full_paint = true;
    assert!(tree.paint_epoch_changed(), "强制全量标志必须被消费");
    tree.note_paint_frame(true, 2);
    assert!(!tree.paint_epoch_changed(), "强制标志只消费一次");
}

#[test]
fn scroll_slot_cache_is_invalidated_by_the_full_entry() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = boxed(&mut tree, root);

    assert_eq!(tree.scroll_slot_cached(child), 0, "初始未分配");
    tree.set_scroll_slot(child, 7);
    assert_eq!(tree.scroll_slot_cached(child), 7);
    tree.begin_full_scroll_slots();
    assert_eq!(tree.scroll_slot_cached(child), 0, "全量入口必须清空缓存");
}

#[test]
fn paint_counters_track_frames_visits_and_order_rebuilds() {
    let mut tree = NodeTree::new();
    assert_eq!(tree.paint_counters(), super::PaintCounters::default());
    tree.note_paint_frame(true, 7);
    tree.note_paint_frame(false, 3);
    tree.note_order_rebuild();
    let counters = tree.paint_counters();
    assert_eq!(counters.visited_nodes, 3, "visited 是最近一帧的值");
    assert_eq!(counters.full_frames, 1);
    assert_eq!(counters.incremental_frames, 1);
    assert_eq!(counters.order_rebuilds, 1);
}
