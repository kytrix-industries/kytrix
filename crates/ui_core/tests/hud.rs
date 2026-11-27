use std::time::Duration;

use ui_core::{
    error::EngineError,
    event::Action,
    math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
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

fn activate(test: &mut TestApp) -> u32 {
    install(test);
    let mut frames = 0;
    while test.drain().expect("激活帧").rendered {
        frames += 1;
        assert!(
            frames <= 1,
            "HUD 激活最多 1 帧（只画面板，不采样、不补帧），实际已提交 {frames} 帧"
        );
    }
    frames
}

fn prims_line(test: &TestApp) -> usize {
    line(test, 8)
        .trim()
        .strip_prefix("Prims")
        .expect("Prims 行格式")
        .trim()
        .parse()
        .expect("Prims 必须是数字")
}

#[test]
fn hud_installs_as_a_top_right_non_interactive_overlay() {
    let mut test = TestApp::new();
    let window = test.window_id();
    let root = test.app().window(window).expect("窗口").tree().root();
    let _target = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(700.0), px(20.0), px(90.0), px(40.0)))
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
        .hit_test(LogicalPoint::new(px(720.0), px(30.0)))
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
fn hud_is_a_passive_observer_and_never_produces_frames() {
    let mut test = TestApp::new();
    let frames = activate(&mut test);
    assert_eq!(frames, 1, "激活只画一帧面板（不采样、不补帧）");
    let settled = test.frame_record().frame_index;

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

#[test]
fn the_installation_frame_does_not_sample_but_shows_the_real_target() {
    let mut test = TestApp::new();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板（不采样、不补帧）");

    for index in 0..10 {
        assert!(
            line(&test, index).trim().ends_with("--"),
            "静止 HUD 的第 {index} 行必须是缺省 `--`：{}",
            line(&test, index)
        );
    }
    let size = line(&test, 10);
    assert!(
        size.starts_with("800x600 @ 1.00x "),
        "安装帧的按帧刷新必须写出真实渲染目标（headless covering = 800x600 @ 1.00x）：{size}"
    );
    assert!(
        !size.trim_end().ends_with(' '),
        "尺寸行必须带后端名：{size}"
    );
}

#[test]
fn the_size_line_follows_the_window_on_the_next_application_frame() {
    let mut test = TestApp::new();
    let window = test.window_id();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板");
    assert!(
        line(&test, 10).starts_with("800x600 @ 1.00x "),
        "{}",
        line(&test, 10)
    );

    test.app_mut()
        .window_mut(window)
        .expect("窗口")
        .set_size(LogicalSize::new(px(1000.0), px(700.0)));
    assert!(test.drain().expect("尺寸变化帧").rendered);
    assert!(
        line(&test, 10).starts_with("1000x700 @ 1.00x "),
        "逻辑尺寸变化后的第一个应用帧必须更新尺寸行：{}",
        line(&test, 10)
    );

    test.app_mut()
        .window_mut(window)
        .expect("窗口")
        .set_device_pixel_ratio(DevicePixelRatio::new(1.25).expect("scale"));
    assert!(test.drain().expect("DPI 变化帧").rendered);
    assert!(
        line(&test, 10).starts_with("1250x875 @ 1.25x "),
        "DPI 变化后的第一个应用帧必须更新尺寸行：{}",
        line(&test, 10)
    );

    assert_eq!(line(&test, 0).trim(), "FPS     --");
    assert_eq!(line(&test, 8).trim(), "Prims   --");
}

#[test]
fn a_frame_after_a_long_idle_does_not_pollute_the_sample_window() {
    let mut test = TestApp::new();
    let window = test.window_id();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板");
    assert_eq!(line(&test, 8).trim(), "Prims   --", "静止时还没有样本");

    test.advance(Duration::from_secs(3));
    test.app_mut().request_repaint(window).expect("请求一帧");
    assert!(test.drain().expect("恢复帧").rendered);
    assert_eq!(
        line(&test, 8).trim(),
        "Prims   --",
        "跨过 3 s 静止的窗口必须丢弃：这一帧不得产出样本"
    );

    test.advance(Duration::from_millis(500));
    test.app_mut().request_repaint(window).expect("请求一帧");
    assert!(test.drain().expect("采样帧").rendered);
    assert!(
        prims_line(&test) > 0,
        "干净窗口之后必须产出真实样本：{}",
        line(&test, 8)
    );
}

#[test]
fn hud_reports_no_frame_rate_without_completed_frames() {
    let mut test = TestApp::new();
    let window = test.window_id();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板");

    test.advance(Duration::from_millis(600));
    test.app_mut().request_repaint(window).expect("请求一帧");
    assert!(test.drain().expect("应用帧").rendered);

    assert_eq!(line(&test, 0).trim(), "FPS     --");
    assert_eq!(line(&test, 1).trim(), "Frame   --");
    assert_eq!(
        line(&test, 7).trim(),
        "Draw    --",
        "没有完成帧就没有 Draw 数据"
    );
    assert_eq!(
        line(&test, 9).trim(),
        "Inst    --",
        "没有完成帧就没有 Inst 数据"
    );
    assert!(
        prims_line(&test) > 0,
        "Prims 必须是真实值：{}",
        line(&test, 8)
    );
}

#[test]
fn application_frames_refresh_the_hud_without_extra_frames() {
    let mut test = TestApp::new();
    let window = test.window_id();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板（不采样、不补帧）");
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

    assert!(
        prims_line(&test) > 0,
        "应用帧上必须刷新出真实图元计数：{}",
        line(&test, 8)
    );
    assert_eq!(line(&test, 0).trim(), "FPS     --");

    assert!(!test.drain().expect("静止帧").rendered);
}

#[test]
fn disabling_the_hud_erases_its_pixels_once() {
    let mut test = TestApp::new();
    let window = test.window_id();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板（不采样、不补帧）");

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

#[test]
fn the_hud_stays_above_the_fixed_popup_root() {
    let mut test = TestApp::new();
    let window = test.window_id();
    let root = test.app().window(window).expect("窗口").tree().root();
    install(&mut test);
    let mut spec = ui_core::popup::PopupSpec::new(
        ui_core::popup::Anchor::WindowRect(LogicalRect::new(
            px(10.0),
            px(10.0),
            px(40.0),
            px(20.0),
        )),
        ui_core::popup::PlacementSpec::default(),
    );
    spec.tier = ui_core::popup::PopupTier::Auto;
    test.app_mut()
        .open_popup(
            window,
            spec,
            |_app, _window, _node| Ok::<(), EngineError>(()),
            None,
        )
        .expect("弹层");
    let _ = test.drain().expect("布局帧");

    let children = test
        .app()
        .window(window)
        .expect("窗口")
        .tree()
        .get(root)
        .expect("根")
        .children()
        .to_vec();
    let popup_root = test
        .probe()
        .by_selector(Selector::Id("popup-root"))
        .expect("popup root");
    let hud = test
        .probe()
        .by_selector(Selector::Id("debug-hud"))
        .expect("HUD 根");
    assert_eq!(
        &children[children.len() - 2..],
        &[popup_root, hud],
        "固定层顺序必须是 [内容..., popup_root, debug_hud]"
    );
}
