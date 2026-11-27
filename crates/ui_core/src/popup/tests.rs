use smol_str::SmolStr;

use crate::{
    a11y::{A11yOverride, A11yRole, identity::A11yIds},
    math::{DevicePixelRatio, LogicalPoint, LogicalRect, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        tree::{HitFocusSkip, NodeTree},
    },
    popup::{
        Anchor, DismissPolicy, FocusPolicy, MAX_POPUP_DEPTH, MAX_POPUP_ENTRIES, PlacementSpec,
        PopupA11y, PopupAnimation, PopupDirty, PopupEntry, PopupError, PopupId, PopupStack,
        PopupState, PopupTier, ZPolicy,
    },
    window::WindowId,
};
#[cfg(feature = "test-support")]
use crate::{
    app::App,
    event::Action,
    math::LogicalSize,
    paint::color::Color,
    paint::style::{Clip, PaintSpec},
    paint::style_builder::StyleBuilder,
    popup::{DismissOutcome, DismissReason, InitialFocus, PopupSpec, RestoreFocus, ScrimSpec},
    test_support::app::TestApp,
};

fn test_window() -> WindowId {
    WindowId::for_test(1)
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> LogicalRect {
    LogicalRect::new(px(x), px(y), px(width), px(height))
}

fn test_anchor() -> Anchor {
    Anchor::WindowRect(rect(10.0, 10.0, 100.0, 20.0))
}

fn empty_stack() -> (NodeTree, PopupStack) {
    let mut tree = NodeTree::new();
    let popup_root = tree.install_popup_root();
    let stack = PopupStack::install(test_window(), popup_root);
    (tree, stack)
}

fn push_popup(
    tree: &mut NodeTree,
    stack: &mut PopupStack,
    z: u64,
    tier: PopupTier,
    parent: Option<PopupId>,
) -> PopupId {
    let node = NodeBuilder::boxed()
        .build(tree, stack.root())
        .expect("content");
    stack
        .insert(|id| PopupEntry {
            id,
            node,
            owner_modal: None,
            parent,
            child: None,
            tier,
            scrim_for: None,
            state: PopupState::Open,
            anchor: test_anchor(),
            placement: PlacementSpec::default(),
            dismiss: DismissPolicy::default(),
            focus: FocusPolicy::None,
            opener: None,
            pointer_transparent: false,
            animation: PopupAnimation::None,
            a11y: PopupA11y::default(),
            z,
            hit_rect: LogicalRect::default(),
            paint_rect: LogicalRect::default(),
            a11y_rect: LogicalRect::default(),
            paint_offset: LogicalPoint::ZERO,
            constrained_size: None,
        })
        .expect("insert")
}

fn z_of(stack: &PopupStack, id: PopupId) -> u64 {
    stack.entry(id).expect("entry").z
}

#[test]
fn the_popup_root_is_the_last_root_child() {
    let mut tree = NodeTree::new();
    let popup_root = tree.install_popup_root();
    let root = tree.root();
    let content = NodeBuilder::boxed()
        .build(&mut tree, root)
        .expect("content");
    let stack = PopupStack::install(test_window(), popup_root);
    assert_eq!(
        tree.get(root).expect("root").children(),
        &[content, stack.root()]
    );
    assert!(stack.is_empty());
    assert_eq!(stack.window(), test_window());
    assert_eq!(tree.hit_focus_skip(stack.root()), HitFocusSkip::Container);
    assert_eq!(
        tree.get(stack.root()).expect("popup root").layout().bounds,
        LogicalRect::default()
    );
}

#[test]
fn opening_popups_keeps_a_strictly_increasing_z_order() {
    let (mut tree, mut stack) = empty_stack();
    let first = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let second = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, None);
    let third = push_popup(&mut tree, &mut stack, 3, PopupTier::Auto, None);

    assert_eq!(stack.order(), &[first, second, third]);
    assert_eq!(z_of(&stack, first), 1);
    assert_eq!(z_of(&stack, second), 2);
    assert_eq!(z_of(&stack, third), 3);
    assert_eq!(stack.len(), 3);
    assert!(!stack.is_empty());
}

#[test]
fn opening_above_an_existing_popup_shifts_the_entries_above_it() {
    let (mut tree, mut stack) = empty_stack();
    let first = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let second = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, None);
    let third = push_popup(&mut tree, &mut stack, 3, PopupTier::Auto, None);

    let inserted = stack.reserve_z(ZPolicy::Above(first)).expect("z");
    let above = push_popup(&mut tree, &mut stack, inserted, PopupTier::Hint, None);

    assert_eq!(z_of(&stack, first), 1);
    assert_eq!(z_of(&stack, above), 2);
    assert_eq!(z_of(&stack, second), 3);
    assert_eq!(z_of(&stack, third), 4);
    assert_eq!(stack.order(), &[first, above, second, third]);
}

#[test]
fn slots_from_other_windows_and_unknown_slots_are_rejected() {
    let (mut tree, mut stack) = empty_stack();
    let id = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);

    let foreign = PopupId::new(WindowId::for_test(9), id.slot(), id.generation());
    assert!(matches!(
        stack.entry(foreign),
        Err(PopupError::UnknownPopup { id: reported }) if reported == foreign
    ));

    let out_of_range = PopupId::new(test_window(), 99, 1);
    assert!(matches!(
        stack.entry(out_of_range),
        Err(PopupError::UnknownPopup { id: reported }) if reported == out_of_range
    ));
}

#[test]
fn reused_slots_bump_the_generation_and_invalidate_old_ids() {
    let (mut tree, mut stack) = empty_stack();
    let first = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    stack.remove(first).expect("remove");
    let second = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, None);

    assert_eq!(first.slot(), second.slot(), "空闲槽位必须复用");
    assert_eq!(second.generation(), first.generation() + 1);
    assert!(matches!(
        stack.entry(first),
        Err(PopupError::StalePopup { id: reported }) if reported == first
    ));
    assert!(stack.entry(second).is_ok());
}

#[test]
fn chain_reports_depth_and_cycle_errors() {
    let (mut tree, mut stack) = empty_stack();
    let mut parent: Option<PopupId> = None;
    let mut deepest = None;
    for _ in 0..=MAX_POPUP_DEPTH {
        let z = stack.next_z();
        let id = push_popup(&mut tree, &mut stack, z, PopupTier::Auto, parent);
        parent = Some(id);
        deepest = Some(id);
    }
    let deepest = deepest.expect("deepest");
    assert!(matches!(
        stack.chain(deepest),
        Err(PopupError::DepthExceeded { depth }) if depth == MAX_POPUP_DEPTH + 1
    ));

    let (mut tree, mut stack) = empty_stack();
    let first = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let second = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, None);
    stack.entry_mut(first).expect("first").parent = Some(second);
    stack.entry_mut(second).expect("second").parent = Some(first);
    assert!(matches!(
        stack.chain(first),
        Err(PopupError::Cycle { id }) if id == first || id == second
    ));
}

#[test]
fn chain_and_subtree_walk_the_parent_links() {
    let (mut tree, mut stack) = empty_stack();
    let a = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let b = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, Some(a));
    let c = push_popup(&mut tree, &mut stack, 3, PopupTier::Auto, Some(b));
    stack.set_child(a, Some(b)).expect("child a");
    stack.set_child(b, Some(c)).expect("child b");
    assert_eq!(stack.chain(c).expect("chain").as_slice(), &[a, b, c]);
    assert_eq!(stack.subtree(a).expect("subtree").as_slice(), &[a, b, c]);
    assert_eq!(stack.subtree(c).expect("subtree").as_slice(), &[c]);
    assert_eq!(stack.nearest_modal(Some(b)), None);
}

#[test]
fn raising_moves_the_popup_and_its_children_above_everything() {
    let (mut tree, mut stack) = empty_stack();
    let a = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let b = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, Some(a));
    let c = push_popup(&mut tree, &mut stack, 3, PopupTier::Auto, None);
    stack.set_child(a, Some(b)).expect("child");

    stack.raise(a).expect("raise");

    let a_z = z_of(&stack, a);
    let b_z = z_of(&stack, b);
    let c_z = z_of(&stack, c);
    assert!(a_z > c_z, "父 popup 必须抬到最上");
    assert!(b_z > a_z, "子 popup 必须保持在父之上");
    assert_eq!(stack.order(), &[c, a, b]);
}

#[test]
fn a_scrim_pair_reserves_two_consecutive_z_values() {
    let (mut _tree, mut stack) = empty_stack();
    let (scrim, modal) = stack.reserve_z_pair(ZPolicy::Newest).expect("pair");
    assert_eq!(scrim + 1, modal);
    assert_eq!(stack.next_z(), modal + 1);

    let missing = PopupId::new(test_window(), u32::MAX, 1);
    assert!(matches!(
        stack.reserve_z_pair(ZPolicy::Above(missing)),
        Err(PopupError::UnknownPopup { id }) if id == missing
    ));
}

#[test]
fn reserve_z_pair_above_keeps_the_pair_consecutive() {
    let (mut tree, mut stack) = empty_stack();
    let first = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let second = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, None);

    let (scrim, modal) = stack.reserve_z_pair(ZPolicy::Above(first)).expect("pair");

    assert_eq!(modal, scrim + 1);
    assert_eq!(z_of(&stack, first), 1);
    assert_eq!(z_of(&stack, second), modal + 1);
}

#[test]
fn the_stack_limit_is_enforced() {
    let (mut tree, mut stack) = empty_stack();
    for index in 0..MAX_POPUP_ENTRIES {
        let z = u64::try_from(index).expect("z") + 1;
        let _ = push_popup(&mut tree, &mut stack, z, PopupTier::Auto, None);
    }
    let node = NodeBuilder::boxed()
        .build(&mut tree, stack.root())
        .expect("content");
    let result = stack.insert(|id| PopupEntry {
        id,
        node,
        owner_modal: None,
        parent: None,
        child: None,
        tier: PopupTier::Auto,
        scrim_for: None,
        state: PopupState::Open,
        anchor: test_anchor(),
        placement: PlacementSpec::default(),
        dismiss: DismissPolicy::default(),
        focus: FocusPolicy::None,
        opener: None,
        pointer_transparent: false,
        animation: PopupAnimation::None,
        a11y: PopupA11y::default(),
        z: 0,
        hit_rect: LogicalRect::default(),
        paint_rect: LogicalRect::default(),
        a11y_rect: LogicalRect::default(),
        paint_offset: LogicalPoint::ZERO,
        constrained_size: None,
    });
    assert!(matches!(
        result,
        Err(PopupError::LimitReached {
            max: MAX_POPUP_ENTRIES
        })
    ));
}

#[test]
fn sync_order_mirrors_the_stack_order_in_the_popup_root() {
    let (mut tree, mut stack) = empty_stack();
    let a = push_popup(&mut tree, &mut stack, 1, PopupTier::Auto, None);
    let b = push_popup(&mut tree, &mut stack, 2, PopupTier::Auto, None);
    stack.sync_order(&mut tree).expect("sync");
    let a_node = stack.entry(a).expect("a").node;
    let b_node = stack.entry(b).expect("b").node;
    assert_eq!(
        tree.get(stack.root()).expect("popup root").children(),
        &[a_node, b_node]
    );

    stack.raise(a).expect("raise");
    stack.sync_order(&mut tree).expect("sync");
    assert_eq!(
        tree.get(stack.root()).expect("popup root").children(),
        &[b_node, a_node]
    );
}

#[test]
fn sync_rects_reads_the_layout_and_hit_rects() {
    let (mut tree, mut stack) = empty_stack();
    let node = NodeBuilder::boxed()
        .layout_override(rect(30.0, 40.0, 50.0, 60.0))
        .build(&mut tree, stack.root())
        .expect("content");
    let id = stack
        .insert(|id| PopupEntry {
            id,
            node,
            owner_modal: None,
            parent: None,
            child: None,
            tier: PopupTier::Auto,
            scrim_for: None,
            state: PopupState::Open,
            anchor: test_anchor(),
            placement: PlacementSpec::default(),
            dismiss: DismissPolicy::default(),
            focus: FocusPolicy::None,
            opener: None,
            pointer_transparent: false,
            animation: PopupAnimation::None,
            a11y: PopupA11y::default(),
            z: 1,
            hit_rect: LogicalRect::default(),
            paint_rect: LogicalRect::default(),
            a11y_rect: LogicalRect::default(),
            paint_offset: LogicalPoint::ZERO,
            constrained_size: None,
        })
        .expect("insert");
    let _ = tree.compute_layout(taffy::geometry::Size {
        width: Some(200.0),
        height: Some(200.0),
    });

    stack.sync_rects(&tree);

    let entry = stack.entry(id).expect("entry");
    assert_eq!(entry.hit_rect, rect(30.0, 40.0, 50.0, 60.0));
    assert_eq!(entry.a11y_rect, rect(30.0, 40.0, 50.0, 60.0));
    assert_eq!(entry.paint_rect, rect(30.0, 40.0, 50.0, 60.0));
}

#[test]
fn reordering_the_popup_root_keeps_the_a11y_children_in_paint_order() {
    let (mut tree, stack) = empty_stack();
    let popup_root = stack.root();
    let label = |tree: &mut NodeTree, text: &'static str| {
        NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Label),
                label: Some(SmolStr::new_static(text)),
                ..A11yOverride::default()
            })
            .id(text)
            .build(tree, popup_root)
            .expect("label")
    };
    let first = label(&mut tree, "first");
    let second = label(&mut tree, "second");
    let _ = tree.compute_layout(taffy::geometry::Size {
        width: Some(200.0),
        height: Some(200.0),
    });

    tree.reorder_children(popup_root, &[second, first])
        .expect("reorder");

    let mut ids = A11yIds::new();
    let mut diagnostics = Vec::new();
    let root = tree.root();
    let record = crate::a11y::tree::node_a11y(
        &tree,
        root,
        DevicePixelRatio::ONE,
        "",
        &mut ids,
        &mut diagnostics,
    )
    .expect("root record");
    let engines: Vec<NodeId> = record
        .children
        .iter()
        .filter_map(|id| ids.engine_of(*id))
        .collect();
    assert_eq!(engines, vec![second, first], "a11y 顺序必须与绘制/命中一致");
}

#[test]
fn the_dirty_flags_accumulate_and_are_consumed_once() {
    let (mut _tree, mut stack) = empty_stack();
    assert_eq!(stack.take_dirty(), PopupDirty::default());
    stack.mark(PopupDirty {
        geometry: true,
        order: false,
    });
    stack.mark(PopupDirty {
        geometry: false,
        order: true,
    });
    let dirty = stack.take_dirty();
    assert!(dirty.geometry && dirty.order);
    assert!(!stack.take_dirty().any());
}

#[cfg(feature = "test-support")]
fn open_popup_with_content(
    app: &mut TestApp,
    tier: PopupTier,
    parent: Option<PopupId>,
    size: (f32, f32),
    on_dismiss: Option<Box<dyn Fn(&mut App, crate::popup::PopupDismiss) -> DismissOutcome>>,
) -> PopupId {
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = tier;
    spec.parent = parent;
    let window = app.window_id();
    app.app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(size.0), px(size.1)).build())
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            on_dismiss,
        )
        .expect("open popup")
}

#[cfg(feature = "test-support")]
fn open_popup(
    app: &mut TestApp,
    tier: PopupTier,
    parent: Option<PopupId>,
    size: (f32, f32),
) -> PopupId {
    open_popup_with_content(app, tier, parent, size, None)
}

#[cfg(feature = "test-support")]
fn build_painted(app: &mut TestApp, parent: NodeId, style: crate::paint::style::Style) -> NodeId {
    let window = app.window_id();
    NodeBuilder::boxed()
        .style(style)
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            parent,
        )
        .expect("node")
}

#[cfg(feature = "test-support")]
fn window_root(app: &TestApp) -> NodeId {
    let window = app.window_id();
    app.app().window(window).expect("window").tree().root()
}

#[cfg(feature = "test-support")]
fn popup_root(app: &TestApp) -> NodeId {
    let window = app.window_id();
    app.app().window(window).expect("window").popups().root()
}

#[cfg(feature = "test-support")]
fn damage_covers(app: &TestApp, damage: &[crate::math::DeviceRect], area: LogicalRect) -> bool {
    let mut union = crate::math::DeviceRect::default();
    for rect in damage {
        union = union.union(*rect);
    }
    let window = app.window_id();
    let dpr = app
        .app()
        .window(window)
        .expect("window")
        .device_pixel_ratio();
    let target = dpr.to_covering_device_rect(area);
    !target.is_empty()
        && union.x0 <= target.x0
        && union.y0 <= target.y0
        && union.x1 >= target.x1
        && union.y1 >= target.y1
}

#[cfg(feature = "test-support")]
#[test]
fn opening_a_popup_does_not_change_the_main_tree_layout() {
    let mut app = TestApp::new();
    let root = window_root(&app);
    let main = build_painted(
        &mut app,
        root,
        StyleBuilder::new().size(px(200.0), px(100.0)).build(),
    );
    app.drain().expect("layout frame");
    let before = app.probe().bounds(main).expect("bounds");

    let id = open_popup(&mut app, PopupTier::Auto, None, (120.0, 80.0));
    app.drain().expect("open frame");

    assert_eq!(app.probe().bounds(main).expect("bounds"), before);
    let window = app.window_id();
    let children = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(window_root(&app))
        .expect("root")
        .children()
        .to_vec();
    assert_eq!(children, vec![main, popup_root(&app)]);
    assert_eq!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .len(),
        1
    );
    assert!(!app.frame_record().damage_full);
    let _ = id;
}

#[cfg(feature = "test-support")]
#[test]
fn popup_content_auto_sizes_to_its_children() {
    let mut app = TestApp::new();
    let id = open_popup(&mut app, PopupTier::Auto, None, (120.0, 80.0));
    app.drain().expect("frame");

    let rect = app.app().popup_rect(id).expect("popup rect");
    assert_eq!(
        rect.hit.origin,
        LogicalPoint::new(px(10.0), px(34.0)),
        "settle 必须按锚点 + gap 放置"
    );
    assert_eq!(rect.hit.size, LogicalSize::new(px(120.0), px(80.0)));
    assert_eq!(rect.a11y, rect.hit);
    assert!(!rect.paint.is_empty());
}

#[cfg(feature = "test-support")]
#[test]
fn absolute_popup_children_are_positioned_in_window_coordinates() {
    let mut app = TestApp::new();
    app.drain().expect("initial frame");
    let window = app.window_id();
    let container = popup_root(&app);
    let child = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .absolute()
                .left(px(100.0))
                .top(px(50.0))
                .size(px(120.0), px(80.0))
                .build(),
        )
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("child");
    app.drain().expect("layout frame");

    assert_eq!(
        app.probe().bounds(child).expect("bounds"),
        rect(100.0, 50.0, 120.0, 80.0)
    );
}

#[cfg(feature = "test-support")]
#[test]
fn the_popup_root_never_wins_the_hit_test() {
    let mut app = TestApp::new();
    let root = window_root(&app);
    let main = build_painted(
        &mut app,
        root,
        StyleBuilder::new().size(px(800.0), px(600.0)).build(),
    );
    app.drain().expect("frame");
    let window = app.window_id();
    let container = popup_root(&app);
    let child = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .absolute()
                .left(px(100.0))
                .top(px(50.0))
                .size(px(120.0), px(80.0))
                .build(),
        )
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("child");
    app.drain().expect("layout frame");

    let inside_popup = LogicalPoint::new(px(110.0), px(60.0));
    assert_eq!(
        app.probe().hit(inside_popup).map(|hit| hit.node),
        Some(child),
        "popup 子节点必须命中"
    );
    let background = LogicalPoint::new(px(700.0), px(500.0));
    let hit = app.probe().hit(background).expect("background hit");
    assert_ne!(hit.node, container);
    assert_eq!(hit.node, main);
}

#[cfg(feature = "test-support")]
#[test]
fn tab_focus_skips_the_popup_root_container() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = window_root(&app);
    let container = popup_root(&app);
    let main = NodeBuilder::boxed()
        .on_click(Action::custom("main"))
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            root,
        )
        .expect("main");
    let item = NodeBuilder::boxed()
        .on_click(Action::custom("item"))
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("item");
    app.drain().expect("frame");

    let tree = app.app().window(window).expect("window").tree();
    assert!(
        !crate::interaction::focus::focusable(tree, container),
        "容器自身不得可聚焦"
    );
    assert!(crate::interaction::focus::focusable(tree, main));
    app.app_mut().set_focus(window, Some(main));
    let next = app.app_mut().focus_next(window).expect("next focus");
    assert_eq!(next, item, "Tab 必须跳过 popup root 容器");
}

#[cfg(feature = "test-support")]
#[test]
fn the_a11y_tree_omits_the_popup_root_container() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = app.window_id();
    let root = window_root(&app);
    let container = popup_root(&app);
    let main = NodeBuilder::boxed()
        .a11y(A11yOverride {
            role: Some(A11yRole::Label),
            label: Some(SmolStr::new_static("main")),
            ..A11yOverride::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            root,
        )
        .expect("main");
    let item = NodeBuilder::boxed()
        .a11y(A11yOverride {
            role: Some(A11yRole::Label),
            label: Some(SmolStr::new_static("popup item")),
            ..A11yOverride::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("item");
    app.drain().expect("frame");

    let snapshot = app.a11y().expect("a11y snapshot");
    assert!(
        snapshot.nodes.iter().all(|node| node.engine != container),
        "容器不得生成 a11y 节点"
    );
    let root_record = snapshot
        .nodes
        .iter()
        .find(|node| node.engine == root)
        .expect("root record");
    let item_record = snapshot
        .nodes
        .iter()
        .find(|node| node.engine == item)
        .expect("popup item record");
    assert!(
        root_record.children.contains(&item_record.id),
        "popup 子节点的 a11y 父必须是窗口根"
    );
    assert!(
        snapshot.nodes.iter().any(|node| node.engine == main),
        "主树 a11y 节点必须保留"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn raising_a_popup_reorders_paint_and_hit_without_a_full_repaint() {
    let mut app = TestApp::new();
    let first = open_popup(&mut app, PopupTier::Auto, None, (120.0, 80.0));
    let second = open_popup(&mut app, PopupTier::Hint, None, (120.0, 80.0));
    app.drain().expect("open frame");
    app.advance(std::time::Duration::from_millis(130));
    app.drain().expect("opening animation settled");
    let window = app.window_id();
    let snapshot = app.app().popup_stack(window).expect("snapshot");
    let first_node = snapshot.entries[0].node;
    let second_node = snapshot.entries[1].node;
    let first_rect = app.app().popup_rect(first).expect("first rect");
    let second_rect = app.app().popup_rect(second).expect("second rect");
    let first_leaf = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(first_node)
        .expect("first content")
        .children()[0];
    let second_leaf = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(second_node)
        .expect("second content")
        .children()[0];
    let point = LogicalPoint::new(
        px(first_rect.hit.origin.x.value() + 10.0),
        px(first_rect.hit.origin.y.value() + 10.0),
    );
    assert_eq!(
        app.probe().hit(point).map(|hit| hit.node),
        Some(second_leaf),
        "后开的 popup 在最上"
    );
    let counters_before = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .paint_counters();

    app.app_mut().raise_popup(first).expect("raise");
    let (damage_full, damage) = {
        let record = app.drain().expect("raise frame");
        (record.damage_full, record.damage.clone())
    };

    assert!(!damage_full, "raise 不得退化为全量重绘");
    assert!(
        damage_covers(&app, &damage, first_rect.hit),
        "raise 必须 damage 被提升的 popup：{damage:?}"
    );
    assert!(
        damage_covers(&app, &damage, second_rect.hit),
        "raise 必须 damage 被压下的 popup：{damage:?}"
    );
    let counters_after = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .paint_counters();
    assert_eq!(counters_after.full_frames, counters_before.full_frames);
    assert_eq!(
        counters_after.order_rebuilds,
        counters_before.order_rebuilds + 1,
        "raise 必须只重建绘制顺序"
    );
    assert_eq!(
        app.probe().hit(point).map(|hit| hit.node),
        Some(first_leaf),
        "提升后命中顺序必须跟随绘制顺序"
    );
    let scene = app.app().window(window).expect("window").scene();
    let tree = app.app().window(window).expect("window").tree();
    let first_lane = tree.lane_of(first_leaf).expect("first lane");
    let second_lane = tree.lane_of(second_leaf).expect("second lane");
    let first_position = scene
        .order()
        .iter()
        .position(|lane| *lane == first_lane)
        .expect("first lane in order");
    let second_position = scene
        .order()
        .iter()
        .position(|lane| *lane == second_lane)
        .expect("second lane in order");
    assert!(
        first_position > second_position,
        "绘制顺序必须与 z 一致：{first_position} vs {second_position}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn closing_a_popup_damages_its_old_rect_without_a_full_repaint() {
    let mut app = TestApp::new();
    let id = open_popup(&mut app, PopupTier::Auto, None, (120.0, 80.0));
    app.drain().expect("open frame");
    let rect = app.app().popup_rect(id).expect("popup rect");

    app.app_mut().close_popup(id).expect("close");
    let (damage_full, damage) = {
        let record = app.drain().expect("close frame");
        (record.damage_full, record.damage.clone())
    };

    assert!(!damage_full, "关闭非模态 popup 不得全量重绘");
    assert!(
        damage_covers(&app, &damage, rect.hit),
        "关闭必须 damage 旧矩形：{damage:?}"
    );
    let window = app.window_id();
    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty()
    );
    let children = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(window_root(&app))
        .expect("root")
        .children()
        .to_vec();
    assert_eq!(children, vec![popup_root(&app)]);
}

#[cfg(feature = "test-support")]
#[test]
fn open_popup_rolls_back_when_the_content_closure_fails() {
    let mut app = TestApp::new();
    app.drain().expect("initial frame");
    let window = app.window_id();
    let len_before = app.app().window(window).expect("window").tree().len();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Auto;
    let error = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |_app, _window, node| Err(crate::error::EngineError::StaleNode { node }),
            None,
        )
        .expect_err("content failure must propagate");

    assert!(matches!(
        error,
        PopupError::ContentBuild { node, source }
            if node != popup_root(&app)
                && matches!(source, crate::error::EngineError::StaleNode { .. })
    ));
    assert_eq!(
        app.app().window(window).expect("window").tree().len(),
        len_before,
        "部分子树必须删除"
    );
    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty()
    );
}

#[cfg(feature = "test-support")]
#[test]
fn open_popup_rejects_invalid_inputs() {
    let mut app = TestApp::new();
    app.drain().expect("initial frame");
    let window = app.window_id();

    let unknown = app.app_mut().open_popup(
        WindowId::for_test(99),
        PopupSpec::new(test_anchor(), PlacementSpec::default()),
        |_app, _window, _node| Ok(()),
        None,
    );
    assert!(matches!(
        unknown,
        Err(PopupError::UnknownWindow { window: reported }) if reported == WindowId::for_test(99)
    ));

    let root = window_root(&app);
    let transient = build_painted(
        &mut app,
        root,
        StyleBuilder::new().size(px(10.0), px(10.0)).build(),
    );
    app.app_mut()
        .remove_node(window, transient)
        .expect("remove");
    let mut missing_anchor =
        PopupSpec::new(Anchor::Node { node: transient }, PlacementSpec::default());
    missing_anchor.tier = PopupTier::Auto;
    let error = app
        .app_mut()
        .open_popup(window, missing_anchor, |_app, _window, _node| Ok(()), None)
        .expect_err("missing anchor");
    assert!(matches!(
        error,
        PopupError::AnchorNodeMissing { node } if node == transient
    ));

    let mut invalid_geometry = PopupSpec::new(
        Anchor::WindowRect(rect(f32::NAN, 0.0, 10.0, 10.0)),
        PlacementSpec::default(),
    );
    invalid_geometry.tier = PopupTier::Auto;
    let error = app
        .app_mut()
        .open_popup(
            window,
            invalid_geometry,
            |_app, _window, _node| Ok(()),
            None,
        )
        .expect_err("invalid geometry");
    assert!(matches!(error, PopupError::InvalidGeometry { .. }));

    let mut invalid_placement = PopupSpec::new(test_anchor(), PlacementSpec::default());
    invalid_placement.tier = PopupTier::Auto;
    invalid_placement.placement.viewport_margin = px(f32::NAN);
    let error = app
        .app_mut()
        .open_popup(
            window,
            invalid_placement,
            |_app, _window, _node| Ok(()),
            None,
        )
        .expect_err("invalid placement");
    assert!(matches!(error, PopupError::InvalidPlacement { .. }));

    let id = open_popup(&mut app, PopupTier::Auto, None, (20.0, 20.0));
    app.app_mut().close_popup(id).expect("close");
    let mut stale_parent = PopupSpec::new(test_anchor(), PlacementSpec::default());
    stale_parent.tier = PopupTier::Auto;
    stale_parent.parent = Some(id);
    let error = app
        .app_mut()
        .open_popup(window, stale_parent, |_app, _window, _node| Ok(()), None)
        .expect_err("stale parent");
    assert!(matches!(error, PopupError::UnknownPopup { id: reported } if reported == id));
}

#[cfg(feature = "test-support")]
#[test]
fn open_popup_enforces_the_depth_limit() {
    let mut app = TestApp::new();
    let mut parent = None;
    for _ in 0..MAX_POPUP_DEPTH {
        parent = Some(open_popup(&mut app, PopupTier::Auto, parent, (10.0, 10.0)));
    }
    let window = app.window_id();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Auto;
    spec.parent = parent;
    let error = app
        .app_mut()
        .open_popup(window, spec, |_app, _window, _node| Ok(()), None)
        .expect_err("depth limit");
    assert!(matches!(
        error,
        PopupError::DepthExceeded { depth } if depth == MAX_POPUP_DEPTH + 1
    ));
}

#[cfg(feature = "test-support")]
#[test]
fn a_parent_holds_at_most_one_open_child() {
    let mut app = TestApp::new();
    let parent = open_popup(&mut app, PopupTier::Auto, None, (100.0, 40.0));
    let first_child = open_popup(&mut app, PopupTier::Auto, Some(parent), (80.0, 30.0));
    let second_child = open_popup(&mut app, PopupTier::Auto, Some(parent), (60.0, 30.0));
    let window = app.window_id();

    let snapshot = app.app().popup_stack(window).expect("snapshot");
    let ids: Vec<PopupId> = snapshot.entries.iter().map(|entry| entry.id).collect();
    assert_eq!(ids, vec![parent, second_child]);
    assert!(ids.iter().all(|id| *id != first_child), "旧子级必须关闭");
    assert_eq!(
        app.app()
            .window(window)
            .expect("window")
            .popups()
            .entry(parent)
            .expect("parent")
            .child,
        Some(second_child)
    );
}

#[cfg(feature = "test-support")]
#[test]
fn opening_a_new_auto_root_closes_the_previous_auto_chain() {
    let mut app = TestApp::new();
    let first = open_popup(&mut app, PopupTier::Auto, None, (100.0, 40.0));
    let child = open_popup(&mut app, PopupTier::Auto, Some(first), (80.0, 30.0));
    let second = open_popup(&mut app, PopupTier::Auto, None, (60.0, 30.0));
    let window = app.window_id();

    let snapshot = app.app().popup_stack(window).expect("snapshot");
    let ids: Vec<PopupId> = snapshot.entries.iter().map(|entry| entry.id).collect();
    assert_eq!(ids, vec![second]);
    assert!(ids.iter().all(|id| *id != first && *id != child));
}

#[cfg(feature = "test-support")]
#[test]
fn hint_popups_do_not_replace_auto_roots() {
    let mut app = TestApp::new();
    let auto = open_popup(&mut app, PopupTier::Auto, None, (100.0, 40.0));
    let hint = open_popup(&mut app, PopupTier::Hint, None, (40.0, 20.0));
    let window = app.window_id();

    let snapshot = app.app().popup_stack(window).expect("snapshot");
    let ids: Vec<PopupId> = snapshot.entries.iter().map(|entry| entry.id).collect();
    assert_eq!(ids, vec![auto, hint]);
}

#[cfg(feature = "test-support")]
#[test]
fn close_popup_closes_children_before_the_parent() {
    let mut app = TestApp::new();
    let root = open_popup(&mut app, PopupTier::Auto, None, (100.0, 40.0));
    let child = open_popup(&mut app, PopupTier::Auto, Some(root), (80.0, 30.0));
    let grandchild = open_popup(&mut app, PopupTier::Auto, Some(child), (60.0, 30.0));
    let window = app.window_id();

    app.app_mut().close_popup(root).expect("close");

    let snapshot = app.app().popup_stack(window).expect("snapshot");
    assert!(snapshot.entries.is_empty());
    assert!(snapshot.entries.iter().all(|entry| entry.id != grandchild));
    let children = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(window_root(&app))
        .expect("root")
        .children()
        .to_vec();
    assert_eq!(children, vec![popup_root(&app)]);
}

#[cfg(feature = "test-support")]
#[test]
fn close_popup_chain_closes_the_whole_chain() {
    let mut app = TestApp::new();
    let root = open_popup(&mut app, PopupTier::Auto, None, (100.0, 40.0));
    let child = open_popup(&mut app, PopupTier::Auto, Some(root), (80.0, 30.0));
    let grandchild = open_popup(&mut app, PopupTier::Auto, Some(child), (60.0, 30.0));
    let window = app.window_id();

    app.app_mut()
        .close_popup_chain(grandchild)
        .expect("close chain");

    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty()
    );
}

#[cfg(feature = "test-support")]
#[test]
fn raising_a_parent_keeps_its_children_above_it() {
    let mut app = TestApp::new();
    let parent = open_popup(&mut app, PopupTier::Auto, None, (100.0, 40.0));
    let child = open_popup(&mut app, PopupTier::Auto, Some(parent), (80.0, 30.0));
    let other = open_popup(&mut app, PopupTier::Hint, None, (60.0, 30.0));
    let window = app.window_id();

    app.app_mut().raise_popup(parent).expect("raise");
    app.drain().expect("frame");

    let snapshot = app.app().popup_stack(window).expect("snapshot");
    let ids: Vec<PopupId> = snapshot.entries.iter().map(|entry| entry.id).collect();
    let parent_z = snapshot
        .entries
        .iter()
        .find(|entry| entry.id == parent)
        .expect("parent")
        .z;
    let child_z = snapshot
        .entries
        .iter()
        .find(|entry| entry.id == child)
        .expect("child")
        .z;
    let other_z = snapshot
        .entries
        .iter()
        .find(|entry| entry.id == other)
        .expect("other")
        .z;
    assert!(child_z > parent_z);
    assert!(parent_z > other_z);
    assert_eq!(ids, vec![other, parent, child]);
}

#[cfg(feature = "test-support")]
#[test]
fn a_modal_scrim_is_synthesized_and_closed_with_the_modal() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let modal = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(120.0), px(80.0)).build())
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("open modal");

    let snapshot = app.app().popup_stack(window).expect("snapshot");
    assert_eq!(snapshot.entries.len(), 2);
    let scrim = snapshot.entries[0];
    let modal_entry = snapshot.entries[1];
    assert_eq!(scrim.tier, PopupTier::Scrim);
    assert_eq!(modal_entry.tier, PopupTier::Modal);
    assert_eq!(modal_entry.z, scrim.z + 1);
    assert_eq!(modal_entry.id, modal);
    assert_eq!(scrim.parent, None);
    assert_eq!(
        app.app()
            .window(window)
            .expect("window")
            .popups()
            .scrim_of(modal),
        Some(scrim.id)
    );

    app.app_mut().close_popup(modal).expect("close");
    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty()
    );
}

#[cfg(feature = "test-support")]
#[test]
fn scrim_requires_modal_tier_and_a_trap_focus_policy() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let nodes_before = app.app().window(window).expect("window").tree().len();

    let mut wrong_tier = PopupSpec::new(test_anchor(), PlacementSpec::default());
    wrong_tier.tier = PopupTier::Auto;
    wrong_tier.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    wrong_tier.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let error = app
        .app_mut()
        .open_popup(window, wrong_tier, |_app, _window, _node| Ok(()), None)
        .expect_err("scrim 与 Auto tier 组合必须被拒绝");
    assert!(matches!(
        error,
        PopupError::InvalidScrim {
            tier: PopupTier::Auto,
            ..
        }
    ));

    let mut wrong_focus = PopupSpec::new(test_anchor(), PlacementSpec::default());
    wrong_focus.tier = PopupTier::Modal;
    wrong_focus.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    wrong_focus.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let error = app
        .app_mut()
        .open_popup(window, wrong_focus, |_app, _window, _node| Ok(()), None)
        .expect_err("scrim 与 Take focus 组合必须被拒绝");
    assert!(matches!(
        error,
        PopupError::InvalidScrim {
            focus: FocusPolicy::Take { .. },
            ..
        }
    ));

    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty(),
        "非法 scrim 组合不得污染 popup 栈"
    );
    assert_eq!(
        app.app().window(window).expect("window").tree().len(),
        nodes_before,
        "非法 scrim 组合不得创建节点"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn closing_the_scrim_closes_its_modal() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let modal = app
        .app_mut()
        .open_popup(window, spec, |_app, _window, _node| Ok(()), None)
        .expect("open modal");
    let scrim = app.app().popup_stack(window).expect("snapshot").entries[0].id;

    app.app_mut().close_popup(scrim).expect("close scrim");

    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty(),
        "modal 与 scrim 必须成对回收"
    );
    assert!(app.app().popup_rect(modal).is_none(), "modal 也已关闭");
}

#[cfg(feature = "test-support")]
#[test]
fn dismiss_topmost_invokes_the_registered_handler() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let seen = std::rc::Rc::new(std::cell::Cell::new(None));
    let recorded = std::rc::Rc::clone(&seen);
    let id = open_popup_with_content(
        &mut app,
        PopupTier::Auto,
        None,
        (60.0, 30.0),
        Some(Box::new(move |_app, dismiss| {
            recorded.set(Some(dismiss.reason));
            DismissOutcome::Close
        })),
    );

    let consumed = app.app_mut().dismiss_topmost(window, DismissReason::Escape);

    assert!(consumed);
    assert_eq!(seen.get(), Some(DismissReason::Escape));
    app.advance(std::time::Duration::from_millis(95));
    app.drain().expect("closing animation");
    assert!(app.app().popup_rect(id).is_none());
}

#[cfg(feature = "test-support")]
#[test]
fn dismiss_topmost_honours_ignore_and_keeps_the_handler() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&calls);
    let id = open_popup_with_content(
        &mut app,
        PopupTier::Auto,
        None,
        (60.0, 30.0),
        Some(Box::new(move |_app, _dismiss| {
            counter.set(counter.get() + 1);
            if counter.get() == 1 {
                DismissOutcome::Ignore
            } else {
                DismissOutcome::Close
            }
        })),
    );

    assert!(
        !app.app_mut()
            .dismiss_topmost(window, DismissReason::OutsidePress)
    );
    assert!(app.app().popup_rect(id).is_some(), "Ignore 不得关闭");
    assert!(
        app.app_mut()
            .dismiss_topmost(window, DismissReason::OutsidePress)
    );
    assert_eq!(calls.get(), 2);
    app.advance(std::time::Duration::from_millis(95));
    app.drain().expect("closing animation");
    assert!(app.app().popup_rect(id).is_none());
}

#[cfg(feature = "test-support")]
#[test]
fn dismiss_topmost_skips_scrims_and_closes_the_modal() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    app.app_mut()
        .open_popup(window, spec, |_app, _window, _node| Ok(()), None)
        .expect("open modal");

    assert!(app.app_mut().dismiss_topmost(window, DismissReason::Escape));
    app.advance(std::time::Duration::from_millis(100));
    app.drain().expect("closing animation");
    assert!(
        app.app()
            .popup_stack(window)
            .expect("snapshot")
            .entries
            .is_empty()
    );
}

#[cfg(feature = "test-support")]
#[test]
fn dismiss_topmost_without_popups_returns_false() {
    let mut app = TestApp::new();
    let window = app.window_id();
    assert!(!app.app_mut().dismiss_topmost(window, DismissReason::Escape));
    assert!(
        !app.app_mut()
            .dismiss_topmost(WindowId::for_test(9), DismissReason::Escape)
    );
}

#[cfg(feature = "test-support")]
#[test]
fn popup_pointer_tracks_the_recent_pointer_position() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let point = LogicalPoint::new(px(33.0), px(44.0));
    app.move_pointer(point);
    assert_eq!(app.app().popup_pointer(window), Some(point));
}

#[cfg(feature = "test-support")]
#[test]
fn set_popup_anchor_and_request_measure_validate_the_popup() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let id = open_popup(&mut app, PopupTier::Auto, None, (60.0, 30.0));
    app.app_mut()
        .set_popup_anchor(id, Anchor::WindowRect(rect(20.0, 20.0, 40.0, 10.0)))
        .expect("valid anchor");
    app.app_mut().request_popup_measure(id).expect("measure");
    let root = window_root(&app);
    let transient = build_painted(
        &mut app,
        root,
        StyleBuilder::new().size(px(5.0), px(5.0)).build(),
    );
    app.app_mut()
        .remove_node(window, transient)
        .expect("remove");
    let error = app
        .app_mut()
        .set_popup_anchor(id, Anchor::Node { node: transient })
        .expect_err("missing anchor node");
    assert!(matches!(
        error,
        PopupError::AnchorNodeMissing { node } if node == transient
    ));

    app.app_mut().close_popup(id).expect("close");
    assert!(matches!(
        app.app_mut().request_popup_measure(id),
        Err(PopupError::UnknownPopup { .. })
    ));
    assert!(app.app().popup_rect(id).is_none());
}

#[cfg(feature = "test-support")]
#[test]
fn popup_content_does_not_clip_overflowing_children() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Auto;
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                let tree = app.window_mut(window).expect("window").tree_mut();
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(100.0), px(100.0)).build())
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, node)?;
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .absolute()
                            .left(px(300.0))
                            .top(px(300.0))
                            .size(px(50.0), px(50.0))
                            .build(),
                    )
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, node)?;
                Ok(())
            },
            None,
        )
        .expect("open");
    app.drain().expect("frame");

    let tree = app.app().window(window).expect("window").tree();
    let content = app
        .app()
        .popup_stack(window)
        .expect("snapshot")
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("entry")
        .node;
    assert_ne!(
        tree.get(content).expect("content").style().clip,
        Clip::Bounds,
        "popup 内容根不得裁剪：阴影/外溢内容交给 kit 的 panel/body 裁剪"
    );
    let overflowing = tree.get(content).expect("content").children()[1];
    assert_eq!(
        app.probe()
            .hit(LogicalPoint::new(px(330.0), px(360.0)))
            .map(|hit| hit.node),
        Some(overflowing),
        "无祖先裁剪时内容根外的子节点仍可命中"
    );
    assert_eq!(
        app.probe()
            .hit(LogicalPoint::new(px(50.0), px(50.0)))
            .map(|hit| hit.node),
        Some(tree.get(content).expect("content").children()[0]),
        "内容根内的子节点照常命中"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn a_panel_shadow_is_painted_outside_the_popup_content_bounds() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let mut spec = PopupSpec::new(test_anchor(), PlacementSpec::default());
    spec.tier = PopupTier::Auto;
    spec.animation = crate::popup::PopupAnimation::None;
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                let tree = app.window_mut(window).expect("window").tree_mut();
                let mut paint = PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                };
                paint.shadows =
                    crate::paint::style::Shadows::new([crate::paint::style::ShadowSpec {
                        offset: LogicalPoint::new(px(0.0), px(6.0)),
                        blur: px(24.0),
                        spread: px(2.0),
                        color: Color::BLACK,
                    }]);
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(120.0), px(80.0)).build())
                    .paint(paint)
                    .build(tree, node)?;
                Ok(())
            },
            None,
        )
        .expect("open");
    app.drain().expect("frame");
    let content = app
        .app()
        .popup_stack(window)
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("entry")
        .node;
    let panel = {
        let tree = app.app().window(window).expect("window").tree();
        tree.get(content).expect("content").children()[0]
    };
    let lane = app.probe().lane_of(panel).expect("面板必须被绘制到 lane");
    let window_state = app.app().window(window).expect("window");
    let actions = window_state.scene().lane_primitives(lane).expect("lane");
    let (index, shadow) = actions
        .iter()
        .enumerate()
        .find_map(|(index, primitive)| match primitive {
            crate::paint::primitive::Primitive::Shadow(shadow) => Some((index, *shadow)),
            _ => None,
        })
        .expect("面板 shadow 图元必须存在");
    assert_eq!(shadow.rect, rect(8.0, 32.0, 124.0, 84.0));
    assert_eq!(shadow.blur, px(24.0));
    assert_eq!(shadow.offset, LogicalPoint::new(px(0.0), px(6.0)));
    let content_rect = rect(10.0, 34.0, 120.0, 80.0);
    let clipped_by_content = actions[..index].iter().any(|primitive| {
        matches!(
            primitive,
            crate::paint::primitive::Primitive::Clip(crate::paint::primitive::ClipOp::Push(clip))
                if clip.rect == content_rect
        )
    });
    assert!(
        !clipped_by_content,
        "popup 内容根不得出现在祖先 clip 链里（E-03）"
    );
}
