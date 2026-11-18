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

fn line(test: &TestApp, index: usize) -> String {
    const LINE_IDS: [&str; 11] = [
        "debug-hud.line-0",
        "debug-hud.line-1",
        "debug-hud.line-2",
        "debug-hud.line-3",
        "debug-hud.line-4",
        "debug-hud.line-5",
        "debug-hud.line-6",
        "debug-hud.line-7",
        "debug-hud.line-8",
        "debug-hud.line-9",
        "debug-hud.line-10",
    ];
    node_text(test, Selector::Id(LINE_IDS[index]))
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

/// **P-C 契约：HUD 是纯观察者，不是帧源。**
///
/// 旧实现（R01-10 之前）每帧无条件请求 HUD damage ⇒ 静止窗口按刷新率提交帧；R01-10 之后改成
/// 「采样点请求一帧」，帧率仍是 2 Hz。现在 HUD **完全不产生帧**：不请求 damage、不参与
/// `needs_continuous_frames`、没有任何定时器/周期唤醒 —— 安装帧之后，静止窗口无论推进多少时间
/// 都必须零提交（`FrameIndex` 不变、`rendered == false`）。
#[test]
fn hud_is_a_passive_observer_and_never_produces_frames() {
    let mut test = TestApp::new();
    install(&mut test);
    assert!(
        test.drain().expect("安装帧").rendered,
        "安装 HUD 必须提交一帧"
    );
    let settled = test.frame_record().frame_index;

    // 2.4 s：跨过四个 500 ms 采样窗口。采样只在「应用自己出帧」时发生，没有帧就没有采样，
    // 也就没有任何理由把窗口从 Idle 拉起来。
    for index in 0..12 {
        test.advance(Duration::from_millis(200));
        assert!(
            !test.drain().expect("静止帧").rendered,
            "第 {index} 个静止帧不得提交：HUD 又变成了帧源（采样定时器 / 无条件 damage）"
        );
    }
    assert_eq!(
        test.frame_record().frame_index,
        settled,
        "静止期间 FrameIndex 不得推进（每帧一帧或每 500 ms 一帧的旧行为都会让它 +N）"
    );
}

/// 安装帧就必须产出第一份**真实**文本：`last_sample_at` 在安装时预置了一个采样间隔。
///
/// 否则纯观察者模式下，静止 demo 永远没有第二帧，HUD 会一直停在 `--` 占位。
#[test]
fn the_installation_frame_already_samples_real_text() {
    let mut test = TestApp::new();
    install(&mut test);
    let _ = test.drain().expect("安装帧");

    let size = line(&test, 10);
    assert_ne!(
        size.trim(),
        "0x0 @ 0.00x",
        "安装帧必须写出真实渲染目标尺寸，而不是占位行：{size}"
    );
    assert!(
        !line(&test, 8).contains("--"),
        "安装帧必须写出真实图元计数：{}",
        line(&test, 8)
    );
}

/// **FPS / Frame 的真实口径：完成帧数 / 采样窗口。**
///
/// headless 窗口没有 GPU 完成反馈（`poll_renderer` 恒 `None`）⇒ 采样窗口内完成帧数为 0 ⇒
/// 两行都必须显示 `--`。旧口径（两次提交帧入口的间隔）在这里会算出 2.0 FPS / 500 ms 的自指值。
#[test]
fn hud_reports_no_frame_rate_without_completed_frames() {
    let mut test = TestApp::new();
    let window = test.window_id();
    install(&mut test);
    let _ = test.drain().expect("安装帧");

    // 一个采样窗口之后由应用主动驱动一帧（HUD 自己不会出帧）。
    test.advance(Duration::from_millis(600));
    test.app_mut().request_repaint(window).expect("请求一帧");
    assert!(test.drain().expect("应用帧").rendered);

    assert_eq!(line(&test, 0).trim(), "FPS     --");
    assert_eq!(line(&test, 1).trim(), "Frame   --");
}

/// 应用自己出帧时 HUD 顺带刷新，但**不得**额外增帧。
#[test]
fn application_frames_refresh_the_hud_without_extra_frames() {
    let mut test = TestApp::new();
    let window = test.window_id();
    install(&mut test);
    let _ = test.drain().expect("安装帧");
    let base = test.frame_record().frame_index.get();

    for index in 0..10 {
        test.advance(Duration::from_millis(60));
        test.app_mut().request_repaint(window).expect("请求一帧");
        assert!(
            test.drain().expect("应用帧").rendered,
            "第 {index} 个应用帧必须提交"
        );
    }
    assert_eq!(
        test.frame_record().frame_index.get(),
        base + 10,
        "HUD 不得额外增帧：10 个应用帧就是 10 次提交"
    );

    // 采样窗口（500 ms）已经过去：文本随应用帧刷新成真实值（帧率仍是 --，没有完成帧）。
    assert!(
        !line(&test, 8).contains("--"),
        "应用帧上必须刷新出真实图元计数：{}",
        line(&test, 8)
    );
    assert_eq!(line(&test, 0).trim(), "FPS     --");

    // 应用不再出帧 ⇒ 立刻回到 Idle（HUD 不补帧）。
    assert!(!test.drain().expect("静止帧").rendered);
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
