//! P17 Debug HUD 的公开 API 集成测试（`hud` + `test-support`）。

use std::time::Duration;

use ui_core::{
    error::EngineError,
    event::Action,
    interaction::OverlayOptions,
    math::{Point, Rect, Scale, Size},
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

/// 安装 HUD 并 drain 完激活帧（只有 1 帧：画面板；不采样、不补帧），返回提交帧数。
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
/// `needs_continuous_frames`、没有任何定时器/周期唤醒 —— 激活也只画一帧面板（不采样、不补帧）。
#[test]
fn hud_is_a_passive_observer_and_never_produces_frames() {
    let mut test = TestApp::new();
    let frames = activate(&mut test);
    assert_eq!(frames, 1, "激活只画一帧面板（不采样、不补帧）");
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

/// **安装帧不采样，但尺寸行已经是真实值。**
///
/// 安装帧只画面板、不采样：10 行指标是缺省 `--`，但尺寸行已经由**按帧刷新**路径写成真实值。
///
/// 尤其是 `Prims` / `Draw` / `Inst` 不能显示 0（0 会被读成「真的没有内容/draw call」）。
/// 真实指标只在应用自己出帧且距上一样本 ≥500 ms 时填入。
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

/// 尺寸行**按帧刷新**：窗口尺寸 / DPI 变化后的**下一个应用帧**就更新，不用等 500 ms 采样窗口，
/// 也不会多产生帧（这两帧都是 `set_size` / `set_scale` 自己污染的 damage 帧）。
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

    // 逻辑尺寸变化：headless 窗口的渲染目标 = covering(size × scale)。
    test.app_mut()
        .window_mut(window)
        .expect("窗口")
        .set_size(Size::new(1000.0, 700.0));
    assert!(test.drain().expect("尺寸变化帧").rendered);
    assert!(
        line(&test, 10).starts_with("1000x700 @ 1.00x "),
        "逻辑尺寸变化后的第一个应用帧必须更新尺寸行：{}",
        line(&test, 10)
    );

    // DPI 变化：scale 1.25 ⇒ 物理目标 1250×875。
    test.app_mut()
        .window_mut(window)
        .expect("窗口")
        .set_scale(Scale::new(1.25).expect("scale"));
    assert!(test.drain().expect("DPI 变化帧").rendered);
    assert!(
        line(&test, 10).starts_with("1250x875 @ 1.25x "),
        "DPI 变化后的第一个应用帧必须更新尺寸行：{}",
        line(&test, 10)
    );

    // 10 行指标没有被这两帧污染（没有采样窗口、也没有完成帧）。
    assert_eq!(line(&test, 0).trim(), "FPS     --");
    assert_eq!(line(&test, 8).trim(), "Prims   --");
}

/// 长时间静止后恢复出帧：**静止时长不算进帧率分母**（真机曾出现真实但误导的
/// `FPS 4.0 / Frame 251.09 ms`）。
///
/// 规则：跨过 `2 × 500 ms` 的采样窗口整窗丢弃 —— 恢复后的第一帧不采样、文本保持上一次；
/// 之后一个干净的 500 ms 窗口才产出样本。
#[test]
fn a_frame_after_a_long_idle_does_not_pollute_the_sample_window() {
    let mut test = TestApp::new();
    let window = test.window_id();
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板");
    assert_eq!(line(&test, 8).trim(), "Prims   --", "静止时还没有样本");

    // 3 s 静止后的第一个应用帧：窗口被丢弃 ⇒ 不采样、文本保持占位。
    test.advance(Duration::from_secs(3));
    test.app_mut().request_repaint(window).expect("请求一帧");
    assert!(test.drain().expect("恢复帧").rendered);
    assert_eq!(
        line(&test, 8).trim(),
        "Prims   --",
        "跨过 3 s 静止的窗口必须丢弃：这一帧不得产出样本"
    );

    // 干净的 500 ms 窗口之后：正常采样。
    test.advance(Duration::from_millis(500));
    test.app_mut().request_repaint(window).expect("请求一帧");
    assert!(test.drain().expect("采样帧").rendered);
    assert!(
        prims_line(&test) > 0,
        "干净窗口之后必须产出真实样本：{}",
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
    assert_eq!(activate(&mut test), 1, "激活只画一帧面板");

    // 一个采样窗口之后由应用主动驱动一帧（HUD 自己不会出帧）：这一帧会采样。
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
    // 采样帧已经把真实图元数填进来了（它不依赖完成反馈）。
    assert!(
        prims_line(&test) > 0,
        "Prims 必须是真实值：{}",
        line(&test, 8)
    );
}

/// 应用自己出帧时 HUD 顺带刷新，但**不得**额外增帧。
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

    // 采样窗口（500 ms）已经过去：文本随应用帧刷新成真实值（帧率仍是 --，没有完成帧）。
    assert!(
        prims_line(&test) > 0,
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
