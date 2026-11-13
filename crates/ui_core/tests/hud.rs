//! P17 Debug HUD 的公开 API 集成测试（`hud` + `test-support`）。

use std::time::Duration;

use ui_core::{
    error::EngineError,
    event::Action,
    interaction::OverlayOptions,
    math::{Point, Rect},
    node::{builder::NodeBuilder, tree::NodeKind},
    test_support::{app::TestApp, probe::Selector},
};

fn install(test: &mut TestApp) {
    let window = test.window_id();
    test.app_mut()
        .set_debug_hud(window, true)
        .expect("HUD 安装");
}

fn node_text(test: &TestApp, selector: Selector) -> String {
    let node = test.probe().by_selector(selector).expect("节点必须存在");
    let window = test.app().window(test.window_id()).expect("窗口");
    match window.tree().get(node).expect("节点").kind() {
        NodeKind::Text(content) => content.text.to_string(),
        other => panic!("expected text node, got {other:?}"),
    }
}

#[test]
fn hud_installs_as_a_top_right_non_interactive_overlay() {
    let mut test = TestApp::new();
    let window = test.window_id();
    let root = test.app().window(window).expect("窗口").tree().root();
    let _target = NodeBuilder::boxed()
        .layout_override(Rect::new(700.0, 20.0, 90.0, 40.0))
        .on_action(Action::custom("hud.underlay"))
        .build(
            test.app_mut().window_mut(window).expect("窗口").tree_mut(),
            root,
        )
        .expect("底层节点");
    install(&mut test);
    assert!(test.app().debug_hud_enabled(window));

    let _ = test.drain().expect("首帧");
    let hud = test
        .probe()
        .by_selector(Selector::Id("debug-hud"))
        .expect("HUD 根");
    let hit = test
        .app()
        .window(window)
        .expect("窗口")
        .tree()
        .hit_test(Point::new(720.0, 30.0))
        .expect("HUD 内仍能命中下层");
    assert_ne!(hit.node, hud, "HUD 不能成为命中目标");
    assert!(!node_text(&test, Selector::Id("debug-hud.line-0")).is_empty());
}

#[test]
fn hud_is_hidden_from_the_accessibility_tree() {
    let mut test = TestApp::new();
    test.enable_a11y_capture();
    install(&mut test);
    let _ = test.drain().expect("首帧");
    let hud = test
        .probe()
        .by_selector(Selector::Id("debug-hud"))
        .expect("HUD 根");
    let snapshot = test.a11y().expect("a11y 捕获");
    let record = snapshot.node_for(hud).expect("HUD 节点在快照里");
    assert!(record.hidden, "HUD 必须标记 hidden，而不是冒充可访问内容");
}

#[test]
fn hud_keeps_committing_frames_while_the_scene_is_idle() {
    let mut test = TestApp::new();
    let window = test.window_id();
    install(&mut test);
    assert!(test.drain().expect("第一帧").rendered);
    assert!(test.drain().expect("HUD 持续帧").rendered);

    test.app_mut()
        .set_debug_hud(window, false)
        .expect("关闭 HUD");
    assert!(!test.app().debug_hud_enabled(window));
    assert!(
        test.drain().expect("擦除 HUD 的提交帧").rendered,
        "关闭 HUD 必须请求一帧把旧像素擦掉"
    );
    assert!(
        !test.drain().expect("空闲帧").rendered,
        "擦除之后窗口必须重新回到 Idle"
    );
}

#[test]
fn hud_samples_fps_after_the_500ms_window() {
    let mut test = TestApp::new();
    install(&mut test);
    let _ = test.drain().expect("首帧");
    test.advance(Duration::from_millis(600));
    let _ = test.drain().expect("第二帧");
    test.advance(Duration::from_millis(600));
    let _ = test.drain().expect("第三帧");

    assert!(
        test.frame_record().stats.layout.is_none(),
        "HUD 文本（LayoutOverride）变化不得触发窗口重排"
    );
    let fps = node_text(&test, Selector::Id("debug-hud.line-0"));
    assert!(fps.starts_with("FPS"), "{fps}");
    assert_ne!(
        fps.trim(),
        "FPS     --",
        "至少两个提交帧后必须有样本：{fps}"
    );
}

#[test]
fn removing_the_subtree_is_detected_and_reinstall_recovers() {
    let mut test = TestApp::new();
    let window = test.window_id();
    install(&mut test);
    let _ = test.drain().expect("首帧");
    let hud = test
        .probe()
        .by_selector(Selector::Id("debug-hud"))
        .expect("HUD 根");
    test.app_mut()
        .window_mut(window)
        .expect("窗口")
        .tree_mut()
        .remove(hud)
        .expect("移除 HUD");
    assert!(
        !test.app().debug_hud_enabled(window),
        "子树被外部删除后查询必须为 false"
    );
    test.app_mut()
        .set_debug_hud(window, true)
        .expect("重新安装");
    assert!(test.app().debug_hud_enabled(window));
    assert!(
        test.probe()
            .by_selector(Selector::Id("debug-hud"))
            .is_some(),
        "重新安装后节点必须存在"
    );
}

#[test]
fn hud_stays_above_overlays() {
    let mut test = TestApp::new();
    let window = test.window_id();
    install(&mut test);
    let _ = test.drain().expect("首帧");
    let hud = test
        .probe()
        .by_selector(Selector::Id("debug-hud"))
        .expect("HUD 根");
    let _ = test
        .app_mut()
        .create_overlay(window, OverlayOptions::default())
        .expect("overlay");
    let root = test.app().window(window).expect("窗口").tree().root();
    let children = test
        .app()
        .window(window)
        .expect("窗口")
        .tree()
        .get(root)
        .expect("root")
        .children();
    assert_eq!(children.last().copied(), Some(hud), "HUD 必须保持在最上层");
}

#[test]
fn unknown_window_is_an_error_and_disable_is_idempotent() {
    let mut test = TestApp::new();
    let window = test.window_id();
    test.app_mut().close_window(window);
    assert!(matches!(
        test.app_mut().set_debug_hud(window, true),
        Err(EngineError::UnknownWindow { .. })
    ));
    assert!(!test.app().debug_hud_enabled(window));

    let mut live = TestApp::new();
    let live_window = live.window_id();
    install(&mut live);
    live.app_mut()
        .set_debug_hud(live_window, false)
        .expect("关闭");
    live.app_mut()
        .set_debug_hud(live_window, false)
        .expect("重复关闭是 no-op");
}
