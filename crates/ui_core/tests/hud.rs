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

/// **R01-10 的契约：HUD 打开 ≠「永远有一帧」。**
///
/// 旧实现 `prepare_frame` 末尾无条件 `request_damage(HUD 面板)`，而帧门禁读的正是
/// `pending_damage` ⇒ 窗口只要开着 HUD 就按刷新率提交帧、整块重画 HUD（静止画面也如此）。
/// 现在只在「有新文本要写」时请求 damage（采样点 = 500 ms；以及提交失败的重试帧），于是：
/// - 静止画面：每次 `drain` 都是 `Idle`，`FrameIndex` 不推进；
/// - 每个 500 ms 采样窗口：**恰好一帧**（不是每轮一帧）。
///
/// 判据全部是**计数**（`rendered` / `FrameIndex`），与机器负载无关。
#[test]
fn hud_costs_nothing_when_the_scene_is_idle() {
    let mut test = TestApp::new();
    install(&mut test);
    assert!(
        test.drain().expect("安装帧").rendered,
        "安装 HUD 必须提交一帧"
    );

    let settled = test.frame_record().frame_index;
    for index in 0..5 {
        assert!(
            !test.drain().expect("静止帧").rendered,
            "第 {index} 个静止帧不得提交：HUD 又在每帧无条件请求 damage 了"
        );
    }
    assert_eq!(
        test.frame_record().frame_index,
        settled,
        "静止期间 FrameIndex 不得推进（每帧一帧的旧行为会让它 +5）"
    );

    // 一个采样窗口（500 ms）恰好一帧：文本刷新真的发生了，但只有这一帧。
    test.advance(Duration::from_millis(600));
    let sample = test.drain().expect("采样帧").clone();
    assert!(sample.rendered, "到达 500 ms 采样点必须提交一帧刷新文本");
    let sampled = test.frame_record().frame_index;
    assert_eq!(
        sampled.get(),
        settled.get() + 1,
        "采样点只允许推进一帧（旧行为是每轮一帧）"
    );

    // HUD 的 damage 必须落在面板内（不是整窗）：面板 = (544,12,244,200) @ scale 1，含 1 px
    // AA 外扩。整窗 damage 会让 HUD 打开时每次都重画整屏。
    let panel = ui_core::math::PhysicalRect::new(543, 11, 789, 213);
    assert!(!sample.damage.is_empty(), "采样帧必须有 damage 才会提交");
    for rect in &sample.damage {
        assert!(
            rect.intersect(panel) == *rect,
            "HUD damage 必须落在面板内：{rect:?}（整窗 damage = R01-10 回归）"
        );
    }

    for index in 0..3 {
        assert!(
            !test.drain().expect("采样后的静止帧").rendered,
            "采样之后第 {index} 帧必须回到 Idle"
        );
    }

    // 第二个采样窗口：同样恰好一帧（帧率上限 = 2 Hz，不是刷新率）。
    test.advance(Duration::from_millis(600));
    assert!(
        test.drain().expect("第二个采样帧").rendered,
        "第二个 500 ms 窗口必须再提交一帧"
    );
    assert_eq!(
        test.frame_record().frame_index.get(),
        settled.get() + 2,
        "两个采样窗口只允许两帧"
    );
}

/// 关闭 HUD 必须**请求一帧**把旧像素擦掉，之后窗口重新回到 Idle。
#[test]
fn disabling_the_hud_erases_its_pixels_once() {
    let mut test = TestApp::new();
    let window = test.window_id();
    install(&mut test);
    assert!(test.drain().expect("第一帧").rendered);

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
