use smallvec::smallvec;

use super::{LayoutBox, Node, NodeKind, NodeSpec, NodeTree};
use crate::{
    error::EngineError,
    math::{Edges, Rect},
    node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId, signal::SignalId},
    paint::style::{Clip, Style, StylePatch},
};

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
        "size_of::<Option<(u8, Option<PhysicalRect>)>>() = {}",
        size_of::<Option<(u8, Option<crate::math::PhysicalRect>)>>()
    );
    println!("size_of::<taffy::Cache>() = {}", size_of::<taffy::Cache>());
    println!(
        "size_of::<Option<Box<crate::a11y::tree::NodeA11y>>>() = {}",
        size_of::<Option<Box<crate::a11y::tree::NodeA11y>>>()
    );
    assert!(
        node_size <= 320,
        "Node 尺寸超预算: {node_size} B（上限 320 B：冷字段必须装箱，见 R03-3）"
    );
    assert_eq!(
        size_of::<crate::text::TextContent>(),
        96,
        "TextContent 的尺寸基线（P1-2.3：family + font_features 收进 TextStyleExtras 后从 112 降到 96）"
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
        size_of::<Rect>() + size_of::<Edges>()
    );
    assert_eq!(size_of::<LayoutBox>(), 32);
}

#[test]
fn root_style_is_percent_100() {
    use taffy::style::{Dimension, ExpandedDimension};

    let tree = NodeTree::new();
    let root = tree.get(tree.root()).expect("root 存在");
    let size = root.style().inner.size;

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
    // 建一个缓存条目
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

    tree.set_style(child, Style::default()).expect("节点存活");

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

    // N5：别的包含块若把被卸载节点记在自己的列表里，也必须被逐表剔除
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
    let style = Style {
        clip: Clip::Bounds,
        ..Style::default()
    };
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
    // B5 之后不再保存 `prev_layout`：**旧位置**由 `place_subtree` 在写新盒时并入
    // （`damage ∪ old ∪ new`，见 `layout::place_subtree` 与 taffy 侧的
    // `set_layout_override_updates_bounds_and_damage`）。`touch` 只补当前盒。
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = boxed(&mut tree, root);
    tree.set_layout_for_test(
        node,
        LayoutBox {
            border: Edges::ZERO,
            bounds: Rect::new(10.0, 0.0, 4.0, 4.0),
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
        Rect::new(10.0, 0.0, 4.0, 4.0),
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

// ── P05：文本节点 ────────────────────────────────────────────────────────

fn text_tree(text: &str) -> (NodeTree, NodeId) {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let node = NodeBuilder::text(text, crate::text::TextStyle::new(16.0))
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
fn text_content_updates_only_text_nodes() {
    let (mut tree, node) = text_tree("one");
    let root = tree.root();

    assert!(tree.set_text_content(
        node,
        crate::text::TextContent::new("two", crate::text::TextStyle::new(12.0))
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
    let node = NodeBuilder::text("x", crate::text::TextStyle::new(16.0))
        .bind_text(|_cx| smol_str::SmolStr::new_static("bound"))
        .build(&mut tree, root)
        .expect("挂载成功");

    let closure = tree.take_bind_text(node).expect("闭包必须存在");
    assert!(tree.take_bind_text(node).is_none(), "取走后为空");
    assert!(tree.put_bind_text(node, closure));
    assert!(tree.take_bind_text(node).is_some(), "放回后仍可取");
}

// ── P06：settle / lane / debug id / clip key ──

fn layout_box(bounds: Rect) -> LayoutBox {
    LayoutBox {
        bounds,
        border: Edges::ZERO,
    }
}

#[test]
fn settle_frame_resets_damage_bits_and_prev_layout() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = boxed(&mut tree, root);
    let _ = tree.set_layout_for_test(child, layout_box(Rect::new(1.0, 2.0, 3.0, 4.0)));
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
    // R03-9：帧结算**不再**清 `PAINT`（唯一清除点是 paint pass）与 `LAYOUT`（唯一清除点是
    // `set_local_layout`）。这里保留脏位是正确语义：本测试没有跑 paint / layout pass。
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
    // 出流子节点在父盒之外。
    let _ = tree.set_layout_for_test(parent, layout_box(Rect::new(0.0, 0.0, 10.0, 10.0)));
    let outside = Rect::new(100.0, 100.0, 20.0, 20.0);
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
    let covering = Rect::new(0.0, 0.0, 10.0, 10.0)
        .to_physical_covering(crate::math::Scale::new(1.0).expect("scale"));
    tree.set_clip_key(child, (1, Some(covering)));
    assert_eq!(tree.clip_key(child), Some((1, Some(covering))));
    tree.clear_clip_key(child);
    assert_eq!(tree.clip_key(child), None);
}

#[test]
fn node_size_after_p06() {
    let size = size_of::<Node>();
    println!("size_of::<Node>() = {size}");
    // wave-2 瘦身后预算 320 B；wave-6 的 NodeId 带窗口身份（8→12 B）后仍必须守住
    // （`Node.id` 与 `Node.parent` 各 +4 B；`Option<NodeId>` 靠 NonZeroU32 的 niche 保持 12 B）。
    assert!(
        size <= 320,
        "Node 超预算：{size} B（备选瘦身：删掉未被读取的 `Node.id` 字段）"
    );
}

// ── B7：设备像素吸附（只吸附边界）────────────────────────────────────────

fn fixed_style(width: f32, height: f32) -> Style {
    let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
    inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(width),
        height: taffy::style::Dimension::length(height),
    };
    Style {
        inner,
        clip: Clip::None,
    }
}

fn layout_to(tree: &mut NodeTree, width: f32, height: f32) {
    let _ = tree
        .compute_layout(taffy::geometry::Size {
            width: Some(width),
            height: Some(height),
        })
        .expect("布局成功");
}

fn bounds_of(tree: &NodeTree, node: NodeId) -> Rect {
    tree.get(node).expect("存在").layout().bounds
}

#[test]
fn device_scale_snaps_committed_bounds_not_taffy_layout() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = NodeBuilder::boxed()
        .style(fixed_style(10.3, 4.4))
        .build(&mut tree, root)
        .expect("挂载成功");
    tree.set_device_scale(Some(2.0));
    layout_to(&mut tree, 200.0, 200.0);

    // 10.3dp × 2 = 20.6px → 21px → 10.5dp；高度同理。
    assert_eq!(bounds_of(&tree, child), Rect::new(0.0, 0.0, 10.5, 4.5));
}

#[test]
fn device_scale_off_keeps_fractional_layout() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = NodeBuilder::boxed()
        .style(fixed_style(10.3, 4.4))
        .build(&mut tree, root)
        .expect("挂载成功");
    layout_to(&mut tree, 200.0, 200.0);

    assert_eq!(
        bounds_of(&tree, child),
        Rect::new(0.0, 0.0, 10.3, 4.4),
        "未开启吸附时机 taffy 的布局盒原样保留"
    );
}

#[test]
fn device_scale_does_not_accumulate_rounding_across_depth() {
    // 三层嵌套、每层偏移 0.3dp：递归引用系必须是**未圆整**坐标，否则误差逐层累积。
    let mut tree = NodeTree::new();
    let root = tree.root();
    let mut parent = root;
    let mut chain = Vec::new();
    for _ in 0..3 {
        let node = NodeBuilder::boxed()
            .style({
                let mut style = fixed_style(50.0, 50.0);
                style.inner.margin = taffy::geometry::Rect {
                    left: taffy::style::LengthPercentageAuto::length(0.3),
                    right: taffy::style::LengthPercentageAuto::length(0.0),
                    top: taffy::style::LengthPercentageAuto::length(0.3),
                    bottom: taffy::style::LengthPercentageAuto::length(0.0),
                };
                style
            })
            .build(&mut tree, parent)
            .expect("挂载成功");
        chain.push(node);
        parent = node;
    }
    tree.set_device_scale(Some(1.0));
    layout_to(&mut tree, 200.0, 200.0);

    // 未圆整的绝对坐标：0.3 / 0.6 / 0.9 → 吸附后 0 / 1 / 1（floor(v+0.5)）。
    let origins: Vec<f32> = chain
        .iter()
        .map(|node| bounds_of(&tree, *node).origin.x)
        .collect();
    assert_eq!(
        origins,
        vec![0.0, 1.0, 1.0],
        "每层边界只做一次舍入（不是父的已舍入值 + 偏移的链式舍入）"
    );
}

#[test]
fn device_scale_change_republishes_bounds() {
    let mut tree = NodeTree::new();
    let root = tree.root();
    let child = NodeBuilder::boxed()
        .style(fixed_style(10.3, 4.4))
        .build(&mut tree, root)
        .expect("挂载成功");
    layout_to(&mut tree, 200.0, 200.0);
    assert_eq!(bounds_of(&tree, child).size.width, 10.3);

    tree.set_device_scale(Some(1.0));
    layout_to(&mut tree, 200.0, 200.0);

    assert_eq!(
        bounds_of(&tree, child).size.width,
        10.0,
        "开启/切换吸附比例后必须重新提交全部边界"
    );
}
