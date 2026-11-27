use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use accesskit::{ActionData, ActionRequest, NodeId as AkNodeId, TreeId};
use smol_str::SmolStr;

use crate::{
    a11y::{
        A11yAction, A11yDiagnostic, A11yNodeId, A11yOverride, A11yRole, A11yTreeSnapshot,
        DISMISS_CUSTOM_ACTION_ID, action,
    },
    app::App,
    event::Action,
    node::{builder::NodeBuilder, id::NodeId},
    popup::{
        Anchor, DismissOutcome, DismissReason, FocusPolicy, HasPopup, InitialFocus, PlacementSpec,
        PopupA11y, PopupDismiss, PopupId, PopupSnapshotEntry, PopupSpec, PopupState, PopupTier,
        RestoreFocus, ScrimSpec,
    },
    test_support::{
        a11y::{assert_no_hidden_ancestors, assert_relations_resolve},
        app::TestApp,
    },
    window::WindowId,
};

fn window(app: &TestApp) -> WindowId {
    app.window_id()
}

fn mount(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    role: A11yRole,
    label: &'static str,
) -> NodeId {
    let tree = app.window_mut(window).expect("window").tree_mut();
    NodeBuilder::boxed()
        .a11y(A11yOverride {
            role: Some(role),
            label: Some(SmolStr::new_static(label)),
            ..A11yOverride::default()
        })
        .tab_stop(true)
        .on_click(Action::custom("activate"))
        .build(tree, parent)
        .expect("mount")
}

fn plain(app: &mut App, window: WindowId, parent: NodeId, role: A11yRole) -> NodeId {
    let tree = app.window_mut(window).expect("window").tree_mut();
    NodeBuilder::boxed()
        .a11y(A11yOverride {
            role: Some(role),
            ..A11yOverride::default()
        })
        .build(tree, parent)
        .expect("mount")
}

fn spec(opener: NodeId, role: A11yRole, label: &'static str) -> PopupSpec {
    let mut spec = PopupSpec::new(Anchor::Node { node: opener }, PlacementSpec::default());
    spec.opener = Some(opener);
    spec.a11y = PopupA11y {
        role: Some(role),
        label: Some(SmolStr::new_static(label)),
        ..PopupA11y::default()
    };
    spec
}

fn open_root(app: &mut TestApp, trigger: NodeId, role: A11yRole, label: &'static str) -> PopupId {
    let mut spec = spec(trigger, role, label);
    spec.a11y.has_popup = Some(HasPopup::Menu);
    let window = window(app);
    app.app_mut()
        .open_popup(window, spec, |_app, _window, _content| Ok(()), None)
        .expect("root popup")
}

fn popup_node(app: &TestApp, id: PopupId) -> NodeId {
    app.app()
        .popup_stack(window(app))
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("open popup entry")
        .node
}

fn popup_entries(app: &TestApp) -> Vec<PopupSnapshotEntry> {
    app.app().popup_stack(window(app)).expect("stack").entries
}

fn full_a11y(app: &mut TestApp) -> A11yTreeSnapshot {
    let window = window(app);
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .a11y_force_full();
    let _ = app.drain().expect("frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("a11y delivery").clone();
    assert_relations_resolve(&snapshot);
    snapshot
}

fn a11y_id(snapshot: &A11yTreeSnapshot, node: NodeId) -> A11yNodeId {
    snapshot.node_for(node).map_or_else(
        || {
            let engines: Vec<NodeId> = snapshot
                .nodes()
                .iter()
                .map(|record| record.engine)
                .collect();
            panic!(
                "missing a11y record for {node:?}; full={} nodes={} delivered engines: {engines:?}",
                snapshot.full,
                snapshot.nodes().len()
            )
        },
        |record| record.id,
    )
}

fn settle_close(app: &mut TestApp, id: PopupId) {
    for _ in 0..40 {
        if !popup_entries(app).iter().any(|entry| entry.id == id) {
            return;
        }
        app.advance(Duration::from_millis(100));
        let _ = app.drain().expect("close frame");
    }
    assert!(
        !popup_entries(app).iter().any(|entry| entry.id == id),
        "popup 未在动画/关闭事务内移除"
    );
}

fn close_popup(app: &mut TestApp, id: PopupId) {
    app.app_mut().close_popup(id).expect("close popup");
    let _ = app.drain().expect("close frame");
    settle_close(app, id);
}

fn assert_unique_parents(snapshot: &A11yTreeSnapshot) {
    let ids: ahash::HashMap<A11yNodeId, &crate::a11y::A11yNode> = snapshot
        .nodes()
        .iter()
        .map(|node| (node.id, node))
        .collect();
    let mut appearances: ahash::HashMap<A11yNodeId, usize> = ahash::HashMap::default();
    for node in snapshot.nodes() {
        if let Some(parent) = node.parent {
            let parent_node = ids
                .get(&parent)
                .expect("parent record in the same snapshot");
            assert_eq!(
                parent_node
                    .children
                    .iter()
                    .filter(|child| **child == node.id)
                    .count(),
                1,
                "{:?} 必须在父 {:?} 的 children 中恰好出现一次",
                node.id,
                parent
            );
        }
        for child in &node.children {
            *appearances.entry(*child).or_default() += 1;
        }
    }
    for (child, count) in appearances {
        assert_eq!(count, 1, "{child:?} 只能有一个 a11y 父（实际 {count} 个）");
    }
    for node in snapshot.nodes() {
        if node.parent.is_none() {
            assert_eq!(node.id, snapshot.root, "只有根可以没有 a11y 父");
        }
    }
}

fn accesskit_node(update: &accesskit::TreeUpdate, id: A11yNodeId) -> &accesskit::Node {
    update
        .nodes
        .iter()
        .find(|(candidate, _)| *candidate == AkNodeId(id.get()))
        .map(|(_, node)| node)
        .expect("node in tree update")
}

#[test]
fn a_popup_chain_nests_the_a11y_tree_and_keeps_the_physical_tree_flat() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "更多");
    let owned = plain(app.app_mut(), window, root, A11yRole::Label);

    let mut first_spec = spec(trigger, A11yRole::Menu, "一级");
    first_spec.a11y.has_popup = Some(HasPopup::Menu);
    first_spec.a11y.owns = smallvec::smallvec![owned];
    let first = app
        .app_mut()
        .open_popup(window, first_spec, |_app, _window, _content| Ok(()), None)
        .expect("first popup");
    let first_node = popup_node(&app, first);
    let item = mount(
        app.app_mut(),
        window,
        first_node,
        A11yRole::MenuItem,
        "二级",
    );
    let mut second_spec = spec(item, A11yRole::Menu, "二级菜单");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(item);
    let second = app
        .app_mut()
        .open_popup(
            window,
            second_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "三级");
                Ok(())
            },
            None,
        )
        .expect("second popup");
    let second_node = popup_node(&app, second);
    let sub = mount(
        app.app_mut(),
        window,
        second_node,
        A11yRole::MenuItem,
        "更深",
    );
    let mut third_spec = spec(sub, A11yRole::Menu, "三级菜单");
    third_spec.parent = Some(second);
    third_spec.a11y.expanded_of = Some(sub);
    let third = app
        .app_mut()
        .open_popup(window, third_spec, |_app, _window, _content| Ok(()), None)
        .expect("third popup");
    let third_node = popup_node(&app, third);

    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot.node_for(first_node).expect("first").parent,
        Some(a11y_id(&snapshot, trigger)),
        "根 popup 的 a11y 父是 trigger"
    );
    assert_eq!(
        snapshot.node_for(second_node).expect("second").parent,
        Some(a11y_id(&snapshot, first_node)),
        "子 popup 的 a11y 父是父 popup 的内容根"
    );
    assert_eq!(
        snapshot.node_for(third_node).expect("third").parent,
        Some(a11y_id(&snapshot, second_node)),
        "第三级沿链嵌套"
    );
    assert_unique_parents(&snapshot);

    let first_record = snapshot.node_for(first_node).expect("first");
    assert_eq!(first_record.role, A11yRole::Menu);
    assert_eq!(first_record.label_text(), Some("一级"));
    assert_eq!(
        first_record.owns.as_slice(),
        &[a11y_id(&snapshot, owned)],
        "PopupA11y.owns 必须进入 accesskit owns（非后代才暴露）"
    );
    let item_record = snapshot.node_for(item).expect("item");
    assert_eq!(item_record.expanded, Some(true), "父 menuitem 展开态");
    assert_eq!(item_record.has_popup, Some(HasPopup::Menu));
    let trigger_record = snapshot.node_for(trigger).expect("trigger");
    assert!(
        trigger_record.controls.is_empty(),
        "默认嵌套（popup 是 trigger 的 a11y 子节点）不重复暴露 controls：{:?}",
        trigger_record.controls
    );
    assert_eq!(trigger_record.expanded, Some(true));
    assert_eq!(trigger_record.has_popup, Some(HasPopup::Menu));
    let update = app.a11y_update().expect("update");
    let trigger_ak = accesskit_node(update, trigger_record.id);
    assert!(trigger_ak.controls().is_empty());
    assert_eq!(trigger_ak.has_popup(), Some(accesskit::HasPopup::Menu));

    let popup_root = app.app().popup_stack(window).expect("stack").root;
    let physical: Vec<NodeId> = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(popup_root)
        .expect("popup root")
        .children()
        .to_vec();
    assert_eq!(
        physical,
        vec![first_node, second_node, third_node],
        "物理树仍平铺在 popup_root 下（z 序）"
    );

    close_popup(&mut app, third);
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot.node_for(item).expect("item").expanded,
        Some(true),
        "关闭最深层不得影响父级展开态"
    );

    close_popup(&mut app, second);
    let snapshot = full_a11y(&mut app);
    let item_record = snapshot.node_for(item).expect("item re-delivered");
    assert_eq!(
        item_record.expanded, None,
        "关闭开始同帧清除父 menuitem 的 expanded"
    );
    assert_eq!(item_record.has_popup, None, "关闭后 haspopup 一并清除");
}

#[test]
fn popup_a11y_parent_override_attaches_the_submenu_to_its_menuitem() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "打开");
    let first = open_root(&mut app, trigger, A11yRole::Menu, "菜单");
    let first_node = popup_node(&app, first);
    let item = mount(
        app.app_mut(),
        window,
        first_node,
        A11yRole::MenuItem,
        "子菜单",
    );
    let mut second_spec = spec(item, A11yRole::Menu, "子菜单");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(item);
    second_spec.a11y.a11y_parent = Some(item);
    let second = app
        .app_mut()
        .open_popup(window, second_spec, |_app, _window, _content| Ok(()), None)
        .expect("submenu");
    let second_node = popup_node(&app, second);
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot.node_for(second_node).expect("submenu").parent,
        Some(a11y_id(&snapshot, item)),
        "PopupA11y.a11y_parent 覆盖父 popup 内容根"
    );
    assert!(
        snapshot
            .node_for(item)
            .expect("menuitem")
            .children
            .contains(&a11y_id(&snapshot, second_node)),
        "menuitem 的 children 必须包含子菜单"
    );
    assert_unique_parents(&snapshot);

    let mut invalid_spec = spec(item, A11yRole::Menu, "坏父");
    invalid_spec.parent = Some(first);
    invalid_spec.a11y.expanded_of = Some(item);
    invalid_spec.a11y.a11y_parent = Some(NodeId::for_test(9_999, 1));
    let invalid = app
        .app_mut()
        .open_popup(window, invalid_spec, |_app, _window, _content| Ok(()), None)
        .expect("popup with invalid parent");
    let invalid_node = popup_node(&app, invalid);
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot.node_for(invalid_node).expect("invalid").parent,
        Some(a11y_id(&snapshot, first_node)),
        "非法 a11y_parent 必须回退到父 popup 内容根"
    );
    assert!(
        app.app().a11y_diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            A11yDiagnostic::InvalidA11yParent { node: reported, .. } if *reported == invalid_node
        )),
        "非法 a11y_parent 必须留诊断：{:?}",
        app.app().a11y_diagnostics()
    );
}

#[test]
fn popup_projection_updates_expanded_and_haspopup_incrementally() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "下拉");
    let popup = open_root(&mut app, trigger, A11yRole::Menu, "选项");
    let _ = app.drain().expect("open frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("incremental delivery").clone();
    let record = snapshot.node_for(trigger).expect("trigger re-delivered");
    assert_eq!(record.expanded, Some(true));
    assert_eq!(record.has_popup, Some(HasPopup::Menu));
    let trigger_id = record.id;
    let update = app.a11y_update().expect("tree update");
    let ak = accesskit_node(update, trigger_id);
    assert_eq!(ak.is_expanded(), Some(true));
    assert_eq!(ak.has_popup(), Some(accesskit::HasPopup::Menu));

    app.app_mut().close_popup(popup).expect("close");
    let _ = app.drain().expect("close frame");
    app.assert_a11y_contract();
    let clearing = app.a11y().expect("close delivery").clone();
    let clearing_record = clearing.node_for(trigger).expect("trigger on close");
    assert_ne!(
        clearing_record.expanded,
        Some(true),
        "关闭开始同帧清除 expanded（允许 false/None）"
    );
    assert_eq!(clearing_record.has_popup, None, "关闭开始同帧清除 haspopup");
    settle_close(&mut app, popup);
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(trigger).expect("trigger on close");
    assert_eq!(record.expanded, None, "关闭同帧清除 expanded");
    assert_eq!(record.has_popup, None, "关闭同帧清除 haspopup");
    let update = app.a11y_update().expect("tree update");
    let ak = accesskit_node(update, trigger_id);
    assert_eq!(ak.is_expanded(), None);
    assert_eq!(ak.has_popup(), None);
}

#[test]
fn a_tooltip_describes_its_trigger_and_at_hide_tooltip_closes_only_the_tooltip() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "悬停");
    let menu_trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let menu = open_root(&mut app, menu_trigger, A11yRole::Menu, "菜单");
    let mut tooltip_spec = spec(trigger, A11yRole::Tooltip, "提示");
    tooltip_spec.tier = PopupTier::Hint;
    tooltip_spec.focus = FocusPolicy::None;
    tooltip_spec.a11y.described_by = Some(trigger);
    let tooltip = app
        .app_mut()
        .open_popup(window, tooltip_spec, |_app, _window, _content| Ok(()), None)
        .expect("tooltip");
    let tooltip_node = popup_node(&app, tooltip);

    let snapshot = full_a11y(&mut app);
    let trigger_record = snapshot.node_for(trigger).expect("trigger");
    assert_eq!(
        trigger_record.described_by.as_slice(),
        &[a11y_id(&snapshot, tooltip_node)],
        "tooltip → trigger 的 described_by 必须落在 trigger 上"
    );
    assert!(
        !trigger_record.actions.contains(&A11yAction::ShowTooltip),
        "引擎无法自行展示 tooltip，不得向 AT 广告 ShowTooltip：{:?}",
        trigger_record.actions
    );
    let tooltip_record = snapshot.node_for(tooltip_node).expect("tooltip");
    assert_eq!(tooltip_record.role, A11yRole::Tooltip);
    assert!(
        tooltip_record.actions.contains(&A11yAction::HideTooltip),
        "tooltip 暴露 HideTooltip：{:?}",
        tooltip_record.actions
    );
    let update = app.a11y_update().expect("update");
    assert_eq!(
        accesskit_node(update, trigger_record.id).described_by(),
        &[AkNodeId(tooltip_record.id.get())],
        "accesskit described_by 必须指向 tooltip 节点"
    );

    let request = ActionRequest {
        action: accesskit::Action::HideTooltip,
        target_tree: TreeId::ROOT,
        target_node: AkNodeId(tooltip_record.id.get()),
        data: None,
    };
    action::dispatch(app.app_mut(), window, request);
    let _ = app.drain().expect("hide tooltip frame");
    settle_close(&mut app, tooltip);
    app.assert_a11y_contract();
    let stack = popup_entries(&app);
    assert!(
        !stack.iter().any(|entry| entry.id == tooltip),
        "HideTooltip 必须关闭 tooltip"
    );
    assert!(
        stack.iter().any(|entry| entry.id == menu),
        "HideTooltip 不得关闭菜单"
    );
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot
            .node_for(trigger)
            .expect("trigger")
            .described_by
            .as_slice(),
        &[],
        "tooltip 关闭后 trigger 的 described_by 必须同步清除"
    );

    let request = ActionRequest {
        action: accesskit::Action::ShowTooltip,
        target_tree: TreeId::ROOT,
        target_node: AkNodeId(a11y_id(&snapshot, trigger).get()),
        data: None,
    };
    action::dispatch(app.app_mut(), window, request);
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::UnhandledAction {
                    action: Some(A11yAction::ShowTooltip),
                    ..
                }
            )),
        "没有 handler 的 ShowTooltip 必须留未处理诊断"
    );
}

#[test]
fn an_at_dismiss_custom_action_closes_the_menu_with_an_a11y_reason() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let seen: Arc<Mutex<Option<DismissReason>>> = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&seen);
    let mut dismiss_spec = spec(trigger, A11yRole::Menu, "操作");
    dismiss_spec.a11y.has_popup = Some(HasPopup::Menu);
    let popup = app
        .app_mut()
        .open_popup(
            window,
            dismiss_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "重命名");
                Ok(())
            },
            Some(Box::new(move |_app, dismiss: PopupDismiss| {
                *captured.lock().expect("reason lock") = Some(dismiss.reason);
                DismissOutcome::Close
            })),
        )
        .expect("menu");
    let menu_node = popup_node(&app, popup);
    let _ = app.drain().expect("open frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("delivery").clone();
    let menu_id = a11y_id(&snapshot, menu_node);
    let update = app.a11y_update().expect("update");
    let ak = accesskit_node(update, menu_id);
    assert!(
        ak.custom_actions()
            .iter()
            .any(|custom| custom.id == DISMISS_CUSTOM_ACTION_ID),
        "menu 必须暴露保留 id 的 dismiss 自定义动作：{:?}",
        ak.custom_actions()
    );
    assert!(ak.supports_action(accesskit::Action::CustomAction));

    let request = ActionRequest {
        action: accesskit::Action::CustomAction,
        target_tree: TreeId::ROOT,
        target_node: AkNodeId(menu_id.get()),
        data: Some(ActionData::CustomAction(DISMISS_CUSTOM_ACTION_ID)),
    };
    action::dispatch(app.app_mut(), window, request);
    assert_eq!(
        *seen.lock().expect("reason lock"),
        Some(DismissReason::A11yDismiss),
        "AT dismiss 必须以 A11yDismiss 进入 on_dismiss"
    );
    let _ = app.drain().expect("dismiss frame");
    settle_close(&mut app, popup);
    assert!(popup_entries(&app).is_empty(), "dismiss 后栈必须清空");
}

#[test]
fn a_modal_popup_marks_the_dialog_hides_the_background_and_hides_the_scrim() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let background = mount(app.app_mut(), window, root, A11yRole::Button, "背景");
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "打开对话框");
    let mut dialog_spec = spec(trigger, A11yRole::Dialog, "确认");
    dialog_spec.tier = PopupTier::Modal;
    dialog_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::DefaultButton,
    };
    dialog_spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let dialog = app
        .app_mut()
        .open_popup(window, dialog_spec, |_app, _window, _content| Ok(()), None)
        .expect("dialog");
    let dialog_node = popup_node(&app, dialog);
    let scrim_node = popup_entries(&app)
        .iter()
        .find(|entry| entry.tier == PopupTier::Scrim)
        .expect("scrim entry")
        .node;

    let snapshot = full_a11y(&mut app);
    assert!(snapshot.node_for(background).expect("background").hidden);
    let dialog_record = snapshot.node_for(dialog_node).expect("dialog");
    assert_eq!(dialog_record.role, A11yRole::Dialog);
    assert!(dialog_record.modal, "aria-modal 由 FocusPolicy::Trap 派生");
    assert!(!dialog_record.hidden);
    assert_eq!(
        dialog_record.parent,
        Some(a11y_id(&snapshot, root)),
        "模态 popup 的逻辑父必须是 window root（trigger 被 modal scope hidden）"
    );
    assert_no_hidden_ancestors(&snapshot);
    assert_relations_resolve(&snapshot);
    let trigger_record = snapshot.node_for(trigger).expect("dialog trigger");
    assert_eq!(
        trigger_record.has_popup,
        Some(HasPopup::Dialog),
        "对话框 opener 的 haspopup=Dialog"
    );
    assert_eq!(
        trigger_record.controls.as_slice(),
        &[dialog_record.id],
        "对话框 opener 的 controls 指向对话框"
    );
    assert!(snapshot.node_for(scrim_node).expect("scrim").hidden);
    assert!(
        app.app()
            .window(window)
            .expect("window")
            .tree()
            .get(background)
            .is_ok(),
        "模态下层只隐藏不销毁"
    );
    let update = app.a11y_update().expect("update");
    assert!(accesskit_node(update, dialog_record.id).is_modal());
    let background_id = a11y_id(&snapshot, background);
    assert!(
        accesskit_node(update, background_id).is_hidden(),
        "背景 hidden 必须在本笔更新里"
    );

    close_popup(&mut app, dialog);
    let snapshot = full_a11y(&mut app);
    assert!(!snapshot.node_for(background).expect("background").hidden);
    assert!(
        popup_entries(&app).is_empty(),
        "关闭 modal 必须一并回收 scrim"
    );
}

#[test]
fn a_menu_separator_is_a_hidden_focusless_splitter() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let separator = plain(app.app_mut(), window, root, A11yRole::Separator);
    let popup = open_root(&mut app, trigger, A11yRole::Menu, "菜单");
    let menu_node = popup_node(&app, popup);
    let menu_separator = plain(app.app_mut(), window, menu_node, A11yRole::Separator);

    let snapshot = full_a11y(&mut app);
    let inside = snapshot.node_for(menu_separator).expect("menu separator");
    assert_eq!(inside.role, A11yRole::Separator);
    assert!(inside.hidden, "菜单分隔线 a11y hidden（平台限制）");
    assert!(
        !inside.actions.contains(&A11yAction::Focus),
        "菜单分隔线不可聚焦：{:?}",
        inside.actions
    );
    let outside = snapshot.node_for(separator).expect("plain separator");
    assert_eq!(outside.role, A11yRole::Separator);
    assert!(!outside.hidden, "非菜单容器的分隔线保持可见");
    assert!(
        !outside.actions.contains(&A11yAction::Focus),
        "分隔线不得到达 Focus 动作"
    );
    let update = app.a11y_update().expect("update");
    let inside_ak = accesskit_node(update, inside.id);
    assert_eq!(inside_ak.role(), accesskit::Role::Splitter);
    assert!(inside_ak.is_hidden());
}

#[test]
fn the_active_descendant_follows_the_focused_menu_item() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let popup_spec = spec(trigger, A11yRole::Menu, "菜单");
    let popup = app
        .app_mut()
        .open_popup(
            window,
            popup_spec,
            |app, window, content| {
                let first = mount(app, window, content, A11yRole::MenuItem, "一");
                let second = mount(app, window, content, A11yRole::MenuItem, "二");
                let tree = app.window_mut(window).expect("window").tree_mut();
                crate::a11y::tree::write_a11y_patch(
                    tree,
                    content,
                    crate::a11y::A11yPatch {
                        owns: Some(smallvec::smallvec![first, second]),
                        ..crate::a11y::A11yPatch::default()
                    },
                );
                Ok(())
            },
            None,
        )
        .expect("menu");
    let menu_node = popup_node(&app, popup);
    let _ = app.drain().expect("open frame");
    app.assert_a11y_contract();
    let children: Vec<NodeId> = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(menu_node)
        .expect("menu")
        .children()
        .to_vec();
    let first = children[0];
    let second = children[1];
    assert!(app.app_mut().set_focus(window, Some(second)));
    let _ = app.drain().expect("focus frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("focus delivery").clone();
    assert_eq!(
        snapshot
            .node_for(menu_node)
            .expect("menu")
            .active_descendant,
        Some(a11y_id(&snapshot, second)),
        "active_descendant 跟随焦点菜单项"
    );
    assert!(app.app_mut().set_focus(window, Some(first)));
    let _ = app.drain().expect("second focus frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("focus delivery").clone();
    assert_eq!(
        snapshot
            .node_for(menu_node)
            .expect("menu")
            .active_descendant,
        Some(a11y_id(&snapshot, first))
    );
    assert!(app.app_mut().set_focus(window, None));
    let _ = app.drain().expect("blur frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("blur delivery").clone();
    assert_eq!(
        snapshot
            .node_for(menu_node)
            .expect("menu")
            .active_descendant,
        None,
        "焦点离开后 active_descendant 必须清除"
    );
}

#[test]
fn a11y_popup_projection_never_breaks_the_accesskit_contract() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "更多");
    let first = open_root(&mut app, trigger, A11yRole::Menu, "一级");
    let first_node = popup_node(&app, first);
    let item = mount(
        app.app_mut(),
        window,
        first_node,
        A11yRole::MenuItem,
        "二级",
    );
    let mut second_spec = spec(item, A11yRole::Menu, "二级");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(item);
    let second = app
        .app_mut()
        .open_popup(
            window,
            second_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "动作");
                Ok(())
            },
            None,
        )
        .expect("second");
    let _ = app.drain().expect("open frame");
    app.assert_a11y_contract();
    let summaries = app.a11y_shadow_summaries();
    assert!(
        summaries
            .values()
            .any(|summary| summary.contains("role=Menu")),
        "consumer 树里必须出现菜单节点"
    );
    app.assert_a11y_oracle();
    app.assert_a11y_contract();
    close_popup(&mut app, second);
    app.assert_a11y_contract();
    app.assert_a11y_oracle();
    app.assert_a11y_contract();
}

#[test]
fn a11y_patch_relations_and_reparenting_are_incremental() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let anchor = mount(app.app_mut(), window, root, A11yRole::Button, "锚点");
    let node = mount(app.app_mut(), window, root, A11yRole::Button, "目标");
    let owner = plain(app.app_mut(), window, root, A11yRole::Label);
    let _ = app.drain().expect("first frame");
    app.assert_a11y_contract();

    app.app_mut()
        .set_a11y_patch(
            window,
            node,
            crate::a11y::A11yPatch {
                has_popup: Some(HasPopup::Menu),
                owns: Some(smallvec::smallvec![owner]),
                described_by: Some(owner),
                a11y_parent: Some(anchor),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("set a11y patch");
    let _ = app.drain().expect("patch frame");
    app.assert_a11y_contract();
    assert!(
        app.a11y()
            .expect("incremental delivery")
            .node_for(node)
            .is_some(),
        "写 a11y patch 后节点必须增量重发"
    );
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(node).expect("patched node re-delivered");
    assert_eq!(record.has_popup, Some(HasPopup::Menu));
    assert_eq!(record.owns.as_slice(), &[a11y_id(&snapshot, owner)]);
    assert_eq!(record.described_by.as_slice(), &[a11y_id(&snapshot, owner)]);
    assert_eq!(
        record.parent,
        Some(a11y_id(&snapshot, anchor)),
        "A11yPatch.a11y_parent 必须重组 a11y 树"
    );
    assert!(
        snapshot
            .node_for(anchor)
            .expect("anchor")
            .children
            .contains(&record.id),
        "新父的 children 必须包含被重组的节点"
    );
    assert_unique_parents(&snapshot);
    let update = app.a11y_update().expect("update");
    let ak = accesskit_node(update, record.id);
    assert_eq!(ak.has_popup(), Some(accesskit::HasPopup::Menu));
    assert_eq!(ak.owns(), &[AkNodeId(a11y_id(&snapshot, owner).get())]);
    assert_eq!(
        ak.described_by(),
        &[AkNodeId(a11y_id(&snapshot, owner).get())]
    );

    app.app_mut()
        .set_a11y_patch(window, node, crate::a11y::A11yPatch::default())
        .expect("clear a11y patch");
    let _ = app.drain().expect("clear frame");
    app.assert_a11y_contract();
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(node).expect("cleared node re-delivered");
    assert_eq!(record.has_popup, None, "增量 patch 必须清除 haspopup");
    assert!(record.owns.is_empty(), "增量 patch 必须清除 owns");
    assert!(
        record.described_by.is_empty(),
        "增量 patch 必须清除 described_by"
    );
    assert_eq!(
        record.parent,
        Some(a11y_id(&snapshot, root)),
        "清除 a11y_parent 必须回到物理父"
    );
    assert!(
        !snapshot
            .node_for(anchor)
            .expect("anchor")
            .children
            .contains(&record.id),
        "旧父的 children 必须移除被重组的节点"
    );
    assert_unique_parents(&snapshot);
    let update = app.a11y_update().expect("update");
    let ak = accesskit_node(update, record.id);
    assert_eq!(ak.has_popup(), None);
    assert!(ak.owns().is_empty());
    assert!(ak.described_by().is_empty());
}

#[test]
fn a_modal_dialog_and_its_inner_popup_accept_at_actions() {
    use std::{cell::Cell, rc::Rc};

    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let outside = mount(app.app_mut(), window, root, A11yRole::Button, "外部");
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "打开");
    let select_hits = Rc::new(Cell::new(0u32));
    let select_counter = Rc::clone(&select_hits);
    app.app_mut()
        .on_action(Action::custom("activate"), move |_ctx| {
            select_counter.set(select_counter.get() + 1);
        });
    let mut inner = None;
    let mut select_trigger = None;
    let mut dialog_spec = spec(trigger, A11yRole::Dialog, "对话框");
    dialog_spec.tier = PopupTier::Modal;
    dialog_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::FirstFocusable,
    };
    dialog_spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let dialog = app
        .app_mut()
        .open_popup(
            window,
            dialog_spec,
            |app, window, content| {
                inner = Some(mount(app, window, content, A11yRole::Button, "内部"));
                select_trigger = Some(mount(app, window, content, A11yRole::Button, "选择"));
                Ok(())
            },
            None,
        )
        .expect("dialog");
    let dialog_node = popup_node(&app, dialog);
    let inner = inner.expect("inner button");
    let select_trigger = select_trigger.expect("select trigger");
    let mut select_spec = spec(select_trigger, A11yRole::ListBox, "选项");
    select_spec.focus = FocusPolicy::None;
    let select = app
        .app_mut()
        .open_popup(
            window,
            select_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "选项 A");
                Ok(())
            },
            None,
        )
        .expect("select");
    let select_node = popup_node(&app, select);
    let snapshot = full_a11y(&mut app);
    assert_no_hidden_ancestors(&snapshot);
    assert_relations_resolve(&snapshot);
    let item = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(select_node)
        .expect("select content")
        .children()[0];
    let item_id = a11y_id(&snapshot, item);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::Click,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(item_id.get()),
            data: None,
        },
    );
    assert_eq!(
        select_hits.get(),
        1,
        "Dialog 内 Select popup 的 AT Click 必须生效; diagnostics={:?}",
        app.app().a11y_diagnostics()
    );
    let inner_id = a11y_id(&snapshot, inner);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::Focus,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(inner_id.get()),
            data: None,
        },
    );
    assert_eq!(
        app.app().focused(window),
        Some(inner),
        "Dialog 内 AT Focus 必须生效"
    );
    let outside_id = a11y_id(&snapshot, outside);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::Click,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(outside_id.get()),
            data: None,
        },
    );
    assert_eq!(select_hits.get(), 1, "模态 scope 外的 AT Click 必须被拒绝");
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::UnhandledAction {
                    action: Some(A11yAction::Click),
                    ..
                }
            )),
        "scope 外动作必须留 UnhandledAction：{:?}",
        app.app().a11y_diagnostics()
    );
    let dialog_id = a11y_id(&snapshot, dialog_node);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::CustomAction,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(dialog_id.get()),
            data: Some(ActionData::CustomAction(DISMISS_CUSTOM_ACTION_ID)),
        },
    );
    settle_close(&mut app, dialog);
    assert!(
        popup_entries(&app).is_empty(),
        "AT dismiss 必须关闭对话框及其 owner_modal 后代 popup：{:?}",
        popup_entries(&app)
    );
}

#[test]
fn popup_a11y_parent_to_a_container_lifts_to_the_window_root() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let popup_root = app.app().popup_stack(window).expect("stack").root;
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let first = open_root(&mut app, trigger, A11yRole::Menu, "一级");
    let first_node = popup_node(&app, first);
    let item = mount(
        app.app_mut(),
        window,
        first_node,
        A11yRole::MenuItem,
        "二级",
    );
    let mut second_spec = spec(item, A11yRole::Menu, "二级");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(item);
    second_spec.a11y.a11y_parent = Some(popup_root);
    let second = app
        .app_mut()
        .open_popup(window, second_spec, |_app, _window, _content| Ok(()), None)
        .expect("second");
    let second_node = popup_node(&app, second);
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot.node_for(second_node).expect("second").parent,
        Some(a11y_id(&snapshot, root)),
        "指向 HitFocusSkip::Container 的 a11y_parent 必须提升到 window root"
    );
    assert_unique_parents(&snapshot);
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::InvalidA11yParent { node, parent }
                    if *node == second_node && *parent == popup_root
            )),
        "指向容器的 a11y_parent 必须留诊断：{:?}",
        app.app().a11y_diagnostics()
    );
    let patched = mount(app.app_mut(), window, root, A11yRole::Button, "补丁");
    app.app_mut()
        .set_a11y_patch(
            window,
            patched,
            crate::a11y::A11yPatch {
                a11y_parent: Some(popup_root),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("patch container parent");
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot.node_for(patched).expect("patched").parent,
        Some(a11y_id(&snapshot, root)),
        "A11yPatch.a11y_parent 指向容器同样提升到 window root"
    );
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::InvalidA11yParent { node, parent }
                    if *node == patched && *parent == popup_root
            )),
        "patch 通道的容器父必须留诊断：{:?}",
        app.app().a11y_diagnostics()
    );
}

#[test]
fn two_a11y_parent_channels_never_create_a_multi_parent_node() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let first = open_root(&mut app, trigger, A11yRole::Menu, "一级");
    let first_node = popup_node(&app, first);
    let first_item = mount(app.app_mut(), window, first_node, A11yRole::MenuItem, "一");
    let second_item = mount(app.app_mut(), window, first_node, A11yRole::MenuItem, "二");
    let mut second_spec = spec(first_item, A11yRole::Menu, "二级");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(first_item);
    second_spec.a11y.a11y_parent = Some(first_item);
    let second = app
        .app_mut()
        .open_popup(window, second_spec, |_app, _window, _content| Ok(()), None)
        .expect("second");
    let second_node = popup_node(&app, second);
    app.app_mut()
        .set_a11y_patch(
            window,
            second_node,
            crate::a11y::A11yPatch {
                a11y_parent: Some(second_item),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("patch parent");
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(second_node).expect("second record");
    assert_eq!(
        record.parent,
        Some(a11y_id(&snapshot, second_item)),
        "A11yPatch.a11y_parent 必须覆盖 PopupA11y.a11y_parent"
    );
    assert!(
        snapshot
            .node_for(second_item)
            .expect("second item")
            .children
            .contains(&record.id),
        "新父的 children 必须包含 popup"
    );
    assert!(
        !snapshot
            .node_for(first_item)
            .expect("first item")
            .children
            .contains(&record.id),
        "旧父的 children 不得再次包含 popup（多父）"
    );
    assert_unique_parents(&snapshot);
}

#[test]
fn a_cross_channel_a11y_parent_cycle_falls_back_to_the_physical_parent() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let first = open_root(&mut app, trigger, A11yRole::Menu, "一级");
    let first_node = popup_node(&app, first);
    let item = mount(
        app.app_mut(),
        window,
        first_node,
        A11yRole::MenuItem,
        "二级",
    );
    let mut second_spec = spec(item, A11yRole::Menu, "二级");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(item);
    let second = app
        .app_mut()
        .open_popup(
            window,
            second_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "环内");
                Ok(())
            },
            None,
        )
        .expect("second");
    let second_node = popup_node(&app, second);
    let inner = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(second_node)
        .expect("second content")
        .children()[0];
    app.app_mut()
        .set_popup_a11y(
            first,
            PopupA11y {
                role: Some(A11yRole::Menu),
                a11y_parent: Some(inner),
                ..PopupA11y::default()
            },
        )
        .expect("set popup a11y");
    let snapshot = full_a11y(&mut app);
    assert_unique_parents(&snapshot);
    assert_relations_resolve(&snapshot);
    assert!(
        snapshot
            .node_for(first_node)
            .expect("first")
            .parent
            .is_some(),
        "成环时必须回退物理父而不是留下无父节点"
    );
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(diagnostic, A11yDiagnostic::InvalidA11yParent { .. })),
        "成环的 a11y_parent 必须留诊断：{:?}",
        app.app().a11y_diagnostics()
    );
}

#[test]
fn a_dismiss_pending_popup_is_hidden_and_clears_expanded_before_removal() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let popup = open_root(&mut app, trigger, A11yRole::Menu, "面板");
    let popup_node = popup_node(&app, popup);
    let _ = full_a11y(&mut app);
    assert!(
        app.app_mut()
            .begin_close_popup(popup, DismissReason::Programmatic)
            .expect("begin close")
    );
    assert!(
        matches!(
            app.app().popup_state(popup),
            Some(PopupState::DismissPending | PopupState::Closing)
        ),
        "begin_close_popup 必须进入关闭事务：{:?}",
        app.app().popup_state(popup)
    );
    let _ = app.drain().expect("closing frame");
    app.assert_a11y_contract();
    let snapshot = app.a11y().expect("closing delivery").clone();
    let trigger_record = snapshot.node_for(trigger).expect("trigger");
    assert_eq!(
        trigger_record.expanded,
        Some(false),
        "关闭开始同帧清除 expanded（置 false）"
    );
    assert_eq!(
        trigger_record.has_popup,
        Some(HasPopup::Menu),
        "aria-haspopup 是静态能力，关闭事务中保持"
    );
    assert!(
        snapshot.node_for(popup_node).expect("closing popup").hidden,
        "DismissPending/Closing 的 popup 必须 a11y hidden"
    );
    settle_close(&mut app, popup);
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot
            .node_for(trigger)
            .expect("trigger after close")
            .expanded,
        None
    );
}

#[test]
fn at_dismiss_closes_the_popup_that_owns_the_target() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let background = mount(app.app_mut(), window, root, A11yRole::Button, "背景");
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let root_reason: Arc<Mutex<Option<DismissReason>>> = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&root_reason);
    let mut root_spec = spec(trigger, A11yRole::Menu, "一级");
    root_spec.a11y.has_popup = Some(HasPopup::Menu);
    let first = app
        .app_mut()
        .open_popup(
            window,
            root_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "子菜单");
                Ok(())
            },
            Some(Box::new(move |_app, dismiss: PopupDismiss| {
                *captured.lock().expect("reason lock") = Some(dismiss.reason);
                DismissOutcome::Close
            })),
        )
        .expect("first");
    let first_node = popup_node(&app, first);
    let item = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(first_node)
        .expect("first content")
        .children()[0];
    let mut second_spec = spec(item, A11yRole::Menu, "二级");
    second_spec.parent = Some(first);
    second_spec.a11y.expanded_of = Some(item);
    let second = app
        .app_mut()
        .open_popup(window, second_spec, |_app, _window, _content| Ok(()), None)
        .expect("second");
    let snapshot = full_a11y(&mut app);
    let first_id = a11y_id(&snapshot, first_node);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::CustomAction,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(first_id.get()),
            data: Some(ActionData::CustomAction(DISMISS_CUSTOM_ACTION_ID)),
        },
    );
    settle_close(&mut app, first);
    settle_close(&mut app, second);
    assert!(
        popup_entries(&app).is_empty(),
        "dismiss 目标层必须是目标节点所属 popup 而非 topmost：{:?}",
        popup_entries(&app)
    );
    assert_eq!(
        *root_reason.lock().expect("reason lock"),
        Some(DismissReason::A11yDismiss)
    );
    let background_id = a11y_id(&snapshot, background);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::CustomAction,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(background_id.get()),
            data: Some(ActionData::CustomAction(DISMISS_CUSTOM_ACTION_ID)),
        },
    );
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::UnhandledAction {
                    action: Some(A11yAction::Dismiss),
                    ..
                }
            )),
        "无 enclosing popup 的 dismiss 必须留诊断"
    );
}

#[test]
fn hide_tooltip_closes_the_tooltip_even_below_another_popup() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let hover = mount(app.app_mut(), window, root, A11yRole::Button, "悬停");
    let menu_trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let seen: Arc<Mutex<Option<DismissReason>>> = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&seen);
    let mut tooltip_spec = spec(hover, A11yRole::Tooltip, "提示");
    tooltip_spec.tier = PopupTier::Hint;
    tooltip_spec.focus = FocusPolicy::None;
    tooltip_spec.a11y.described_by = Some(hover);
    let tooltip = app
        .app_mut()
        .open_popup(
            window,
            tooltip_spec,
            |_app, _window, _content| Ok(()),
            Some(Box::new(move |_app, dismiss: PopupDismiss| {
                *captured.lock().expect("reason lock") = Some(dismiss.reason);
                DismissOutcome::Close
            })),
        )
        .expect("tooltip");
    let tooltip_node = popup_node(&app, tooltip);
    let menu = open_root(&mut app, menu_trigger, A11yRole::Menu, "菜单");
    let snapshot = full_a11y(&mut app);
    let tooltip_id = a11y_id(&snapshot, tooltip_node);
    action::dispatch(
        app.app_mut(),
        window,
        ActionRequest {
            action: accesskit::Action::HideTooltip,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(tooltip_id.get()),
            data: None,
        },
    );
    settle_close(&mut app, tooltip);
    assert_eq!(
        *seen.lock().expect("reason lock"),
        Some(DismissReason::A11yDismiss),
        "HideTooltip 必须走带 reason 的关闭事务"
    );
    assert!(
        popup_entries(&app).iter().any(|entry| entry.id == menu),
        "HideTooltip 不得关闭 tooltip 之上的菜单"
    );
}

#[test]
fn a11y_patch_active_descendant_is_a_hover_channel() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "菜单");
    let mut menu_spec = spec(trigger, A11yRole::Menu, "菜单");
    menu_spec.a11y.owns = smallvec::smallvec![];
    let menu = app
        .app_mut()
        .open_popup(
            window,
            menu_spec,
            |app, window, content| {
                mount(app, window, content, A11yRole::MenuItem, "一");
                mount(app, window, content, A11yRole::MenuItem, "二");
                Ok(())
            },
            None,
        )
        .expect("menu");
    let menu_node = popup_node(&app, menu);
    let children: Vec<NodeId> = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(menu_node)
        .expect("menu")
        .children()
        .to_vec();
    let second = children[1];
    app.app_mut()
        .set_a11y_patch(
            window,
            menu_node,
            crate::a11y::A11yPatch {
                active_descendant: Some(second),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("patch active descendant");
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot
            .node_for(menu_node)
            .expect("menu")
            .active_descendant,
        Some(a11y_id(&snapshot, second)),
        "A11yPatch.active_descendant 是 hover active_item 的 P1 通道"
    );
    app.app_mut()
        .set_a11y_patch(window, menu_node, crate::a11y::A11yPatch::default())
        .expect("clear patch");
    let snapshot = full_a11y(&mut app);
    assert_eq!(
        snapshot
            .node_for(menu_node)
            .expect("menu")
            .active_descendant,
        None
    );
}

#[test]
fn the_reserved_dismiss_action_id_cannot_be_registered_or_advertised() {
    use std::{cell::Cell, rc::Rc};

    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let hits = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&hits);
    assert!(
        !app.app_mut()
            .on_a11y_action(A11yAction::Custom(DISMISS_CUSTOM_ACTION_ID), move |_ctx| {
                counter.set(counter.get() + 1);
            }),
        "保留 id 不得注册为用户 a11y action"
    );
    let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
    NodeBuilder::boxed()
        .a11y(A11yOverride {
            role: Some(A11yRole::Button),
            label: Some(SmolStr::new_static("动作")),
            custom_actions: vec![crate::a11y::A11yCustomAction {
                id: DISMISS_CUSTOM_ACTION_ID,
                label: SmolStr::new_static("冲突"),
            }],
            ..A11yOverride::default()
        })
        .build(tree, root)
        .expect("node");
    let _ = app.drain().expect("frame");
    app.assert_a11y_contract();
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                A11yDiagnostic::ReservedCustomAction { id, .. } if *id == DISMISS_CUSTOM_ACTION_ID
            )),
        "保留 custom id 必须留诊断：{:?}",
        app.app().a11y_diagnostics()
    );
    let snapshot = app.a11y().expect("delivery").clone();
    let update = app.a11y_update().expect("update");
    for node in snapshot.nodes() {
        if let Some(ak) = update
            .nodes
            .iter()
            .find(|(id, _)| *id == AkNodeId(node.id.get()))
            .map(|(_, node)| node)
        {
            assert!(
                !ak.custom_actions()
                    .iter()
                    .any(|custom| custom.id == DISMISS_CUSTOM_ACTION_ID),
                "保留 id 不得作为用户 custom action 广播"
            );
        }
    }
    assert_eq!(hits.get(), 0);
}

#[test]
fn static_haspopup_controls_and_expanded_survive_the_popup_lifecycle() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let trigger = mount(app.app_mut(), window, root, A11yRole::Button, "下拉");
    let panel = plain(app.app_mut(), window, root, A11yRole::Group);
    app.app_mut()
        .set_a11y_patch(
            window,
            trigger,
            crate::a11y::A11yPatch {
                has_popup: Some(HasPopup::Menu),
                expanded: Some(false),
                controls: Some(smallvec::smallvec![panel]),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("static patch");
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(trigger).expect("trigger");
    assert_eq!(record.has_popup, Some(HasPopup::Menu));
    assert_eq!(record.expanded, Some(false));
    assert_eq!(record.controls.as_slice(), &[a11y_id(&snapshot, panel)]);
    let update = app.a11y_update().expect("update");
    let ak = accesskit_node(update, record.id);
    assert_eq!(ak.has_popup(), Some(accesskit::HasPopup::Menu));
    assert_eq!(ak.is_expanded(), Some(false));
    assert_eq!(ak.controls(), &[AkNodeId(a11y_id(&snapshot, panel).get())]);

    let popup = open_root(&mut app, trigger, A11yRole::Menu, "面板");
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(trigger).expect("open trigger");
    assert_eq!(record.expanded, Some(true), "打开时 popup 投影覆盖静态声明");
    assert!(
        record.controls.is_empty(),
        "打开时 popup 是 a11y 子节点，controls 不重复暴露：{:?}",
        record.controls
    );
    close_popup(&mut app, popup);
    let snapshot = full_a11y(&mut app);
    let record = snapshot.node_for(trigger).expect("closed trigger");
    assert_eq!(
        record.has_popup,
        Some(HasPopup::Menu),
        "关闭后静态 aria-haspopup 由 A11yPatch 保活"
    );
    assert_eq!(
        record.expanded,
        Some(false),
        "关闭后恢复 kit 声明的静态 expanded"
    );
    assert_eq!(
        record.controls.as_slice(),
        &[a11y_id(&snapshot, panel)],
        "关闭后恢复 kit 声明的静态 controls"
    );
}

#[test]
fn a_patch_only_a11y_parent_cycle_falls_back_to_the_physical_parent() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = window(&app);
    let root = app.app().window(window).expect("window").tree().root();
    let first = mount(app.app_mut(), window, root, A11yRole::Button, "甲");
    let second = mount(app.app_mut(), window, root, A11yRole::Button, "乙");
    app.app_mut()
        .set_a11y_patch(
            window,
            first,
            crate::a11y::A11yPatch {
                a11y_parent: Some(second),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("first parent");
    app.app_mut()
        .set_a11y_patch(
            window,
            second,
            crate::a11y::A11yPatch {
                a11y_parent: Some(first),
                ..crate::a11y::A11yPatch::default()
            },
        )
        .expect("second parent");
    let snapshot = full_a11y(&mut app);
    assert_unique_parents(&snapshot);
    assert_eq!(
        snapshot.node_for(first).expect("first").parent,
        Some(a11y_id(&snapshot, root)),
        "patch 通道成环必须回退物理父"
    );
    assert_eq!(
        snapshot.node_for(second).expect("second").parent,
        Some(a11y_id(&snapshot, root))
    );
    for node in [first, second] {
        assert!(
            app.app()
                .a11y_diagnostics()
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    A11yDiagnostic::InvalidA11yParent { node: reported, .. } if *reported == node
                )),
            "成环节点必须留诊断：{node:?} / {:?}",
            app.app().a11y_diagnostics()
        );
    }
}
