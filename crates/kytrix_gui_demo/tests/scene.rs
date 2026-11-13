//! demo 场景的集成测试（`design/phase-15-demo-showcase.md` §4）。
//!
//! 只走公开 API：`TestApp`（无 GPU 帧驱动 + a11y 快照捕获）+ demo 库的 `scene` 模块。
//! 覆盖四类展示能力的**可断言事实**：整树提交、CJK 无豆腐、Tab 序、无障碍角色与选中态、IME 状态机接线。

use kytrix_gui::{
    a11y::{A11yAction, A11yRole},
    event::{Event, ImeEvent, InputEvent, Key, KeyState, Modifiers, PointerButton},
    math::{Point, Scale, Size},
    test_support::{app::TestApp, record::PrimitiveKind},
    text::TextDiagnostic,
    window::{WindowId, WindowOptions},
};
use kytrix_gui_demo::scene::{self, Scene};

/// 建一个与真实窗口同尺寸的 headless 场景（未安装交互）。
fn setup() -> (TestApp, WindowId, Scene) {
    let mut test = TestApp::with_options(WindowOptions {
        title: "Kytrix GUI · 展示".into(),
        size: Size::new(scene::LOGICAL_WIDTH, scene::LOGICAL_HEIGHT),
        scale: Scale::ONE,
    });
    let window = test.window_id();
    let scene = Scene::build_into(test.app_mut(), window);
    (test, window, scene)
}

/// 建场景 + 安装交互（动作处理器 / 初始焦点 / 帧钩子）。
fn setup_installed() -> (TestApp, WindowId, Scene) {
    let (mut test, window, scene) = setup();
    Scene::install(test.app_mut(), scene.clone());
    (test, window, scene)
}

/// 一帧：跑 styles/text/layout/paint 并提交。
fn frame(test: &mut TestApp) {
    let _ = test.drain().expect("帧必须提交");
}

fn press(key: Key) -> InputEvent {
    InputEvent::Key {
        key,
        state: KeyState::Down,
        modifiers: Modifiers::empty(),
    }
}

#[test]
fn the_first_frame_commits_every_card() {
    let (mut test, window, _scene) = setup();
    test.enable_a11y_capture();
    frame(&mut test);

    let record = test.frame_record();
    assert!(record.rendered, "首帧必须产出像素");
    assert!(
        record.stats.primitives > 60,
        "六张卡片 + 色块 + 分段 + 滚动行 + 文字应当远超 60 个图元：{}",
        record.stats.primitives
    );

    let frame = record.frame.as_ref().expect("提交帧必须有绘制快照");
    let glyph_runs = frame
        .primitives
        .iter()
        .filter(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
        .count();
    assert!(
        glyph_runs >= 12,
        "标题 + 6 张卡片 + CJK 段落至少 12 个字形 run：{glyph_runs}"
    );

    // 自绘画布（两个 PNG 图标）：`Canvas` 内部图元是**绝对窗口坐标**，必须落在 ⑥ 卡片
    // （x=624,y=484,w=312,h=242）内，而不是被误当成父相对坐标画到窗口左上角。
    let canvases: Vec<_> = frame
        .primitives
        .iter()
        .filter(|primitive| primitive.kind == PrimitiveKind::Canvas)
        .map(|primitive| primitive.bounds)
        .collect();
    assert_eq!(canvases.len(), 2, "两个图标都是 Canvas 节点");
    for bounds in canvases {
        assert!(
            bounds.origin.x >= 624.0 && bounds.origin.y >= 484.0,
            "图标必须画在 ⑥ 卡片内：{bounds:?}"
        );
        assert!(bounds.max().y <= 726.0, "图标不得溢出卡片底边：{bounds:?}");
    }

    let app = test.app();
    assert!(
        !app.text_diagnostics()
            .iter()
            .any(|diagnostic| matches!(diagnostic, TextDiagnostic::MissingGlyph { .. })),
        "CJK 样例不得出现豆腐块：{:?}",
        app.text_diagnostics()
    );
    assert!(
        app.a11y_diagnostics().is_empty(),
        "所有交互节点都有可访问名、自绘节点都有替代文本：{:?}",
        app.a11y_diagnostics()
    );
    assert_eq!(app.focused(window), None, "`build_into` 不预置焦点");
}

#[test]
fn the_a11y_tree_exposes_roles_names_and_values() {
    let (mut test, _window, _scene) = setup();
    test.enable_a11y_capture();
    frame(&mut test);

    let snapshot = test.a11y().expect("捕获打开时首帧必有 a11y 交付");
    assert!(snapshot.full, "首笔必须是全树");

    let labels: Vec<(&str, A11yRole)> = snapshot
        .nodes()
        .iter()
        .filter_map(|node| node.label_text().map(|text| (text, node.role)))
        .collect();
    for expected in [
        ("输入法测试输入框", A11yRole::TextInput),
        ("缩放比例", A11yRole::Slider),
        ("选项 A", A11yRole::RadioButton),
        ("选项 B", A11yRole::RadioButton),
        ("色块 1（红）", A11yRole::RadioButton),
        ("色块 3（蓝）", A11yRole::RadioButton),
        ("Kytrix 图标（2× 放大）", A11yRole::Image),
    ] {
        assert!(
            labels.contains(&expected),
            "a11y 树缺少 {expected:?}；实际标签：{labels:?}"
        );
    }

    let input = snapshot
        .nodes()
        .iter()
        .find(|node| node.role == A11yRole::TextInput)
        .expect("输入框必须在 a11y 树里");
    assert_eq!(input.value.as_deref(), Some(""), "未输入时 value 是空串");
    assert!(
        input.actions.contains(&A11yAction::SetValue),
        "TextInput 必须支持 SetValue：{:?}",
        input.actions
    );

    let slider = snapshot
        .nodes()
        .iter()
        .find(|node| node.role == A11yRole::Slider)
        .expect("滑块必须在 a11y 树里");
    assert_eq!(slider.value.as_deref(), Some("60"), "默认 6 档 = 60%");

    let radio_a = snapshot
        .nodes()
        .iter()
        .find(|node| node.label_text() == Some("选项 A"))
        .expect("选项 A");
    assert_eq!(radio_a.checked, Some(true), "默认选中选项 A");
    let radio_b = snapshot
        .nodes()
        .iter()
        .find(|node| node.label_text() == Some("选项 B"))
        .expect("选项 B");
    assert_eq!(radio_b.checked, Some(false), "选项 B 必须显式未勾选");
}

#[test]
fn tab_order_visits_the_documented_seven_labels() {
    let (mut test, window, _scene) = setup();
    test.enable_a11y_capture();
    // 先跑一帧：布局盒与 `bind_style` 的 `interactive` 都要先落地。
    frame(&mut test);

    let mut visited = Vec::new();
    let mut labels = Vec::new();
    for _ in 0..7 {
        let focused = test.app_mut().focus_next(window).expect("下一个可聚焦节点");
        visited.push(focused);
        frame(&mut test);
        let snapshot = test.a11y().expect("焦点变化必须产生 a11y 交付");
        let label = snapshot
            .node_for(focused)
            .and_then(|node| node.label_text())
            .unwrap_or("<unknown>")
            .to_owned();
        labels.push(label);
    }
    assert_eq!(
        labels,
        [
            "色块 1（红）",
            "色块 2（绿）",
            "色块 3（蓝）",
            "选项 A",
            "选项 B",
            // 焦点面板用短名（「缩放滑块」/「输入框」），a11y 名字是完整可访问名。
            "缩放比例",
            "输入法测试输入框",
        ],
        "Tab 序必须与设计文档 §1.1 一致"
    );

    let wrapped = test.app_mut().focus_next(window).expect("回绕");
    assert_eq!(wrapped, visited[0], "第 8 次 Tab 必须回绕到第一个");
}

#[test]
fn clicking_a_swatch_selects_it_for_the_a11y_tree() {
    let (mut test, window, _scene) = setup_installed();
    test.enable_a11y_capture();
    frame(&mut test);

    // 色块 2 的中心：卡片 (24,80) + 卡片内 (202,42,150,88)。
    let center = Point::new(24.0 + 202.0 + 75.0, 80.0 + 42.0 + 44.0);
    let _ = test.inject(InputEvent::PointerDown {
        position: center,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
    });
    let _ = test.inject(InputEvent::PointerUp {
        position: center,
        button: PointerButton::Primary,
        modifiers: Modifiers::empty(),
    });
    frame(&mut test);

    let focused = test
        .app()
        .focused(window)
        .expect("点击色块后焦点落在它上面");
    let snapshot = test.a11y().expect("选中态变化必须产生 a11y 交付");
    let record = snapshot
        .node_for(focused)
        .expect("被点中的色块必须出现在增量交付里");
    assert_eq!(record.role, A11yRole::RadioButton);
    assert_eq!(
        record.checked,
        Some(true),
        "点击后必须写 Toggled（RadioButton 的选中态通道是 checked，不是 selected）"
    );
    assert_eq!(record.selected, None, "selected 不参与 RadioButton 语义");
    assert!(test.app().a11y_diagnostics().is_empty());
}

#[test]
fn ime_events_are_applied_only_while_the_input_is_focused() {
    let (mut test, window, mut scene) = setup_installed();
    frame(&mut test);

    let app = test.app_mut();
    // Tab 序的最后一个可聚焦节点是输入框。
    let focused = app.focus_prev(window).expect("输入框");
    assert_eq!(app.focused(window), Some(focused));

    scene::dispatch_event(&mut scene, app, &Event::Ime(ImeEvent::Start));
    scene::dispatch_event(&mut scene, app, &Event::Ime(ImeEvent::Preedit("ni".into())));
    assert_eq!(scene::ime_text(&scene, app).as_str(), "", "组合串不进正文");

    scene::dispatch_event(
        &mut scene,
        app,
        &Event::Ime(ImeEvent::Commit("你好".into())),
    );
    assert_eq!(scene::ime_text(&scene, app).as_str(), "你好");

    scene::dispatch_event(
        &mut scene,
        app,
        &Event::Input(InputEvent::Text { text: "a".into() }),
    );
    assert_eq!(scene::ime_text(&scene, app).as_str(), "你好a");

    scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::Backspace)));
    assert_eq!(scene::ime_text(&scene, app).as_str(), "你好");

    scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::Enter)));
    assert_eq!(
        scene::ime_text(&scene, app).as_str(),
        "",
        "Enter 提交后清空输入"
    );

    // 焦点移走后，IME / 文本事件都必须被忽略。
    let _ = app.set_focus(window, None);
    scene::dispatch_event(
        &mut scene,
        app,
        &Event::Ime(ImeEvent::Commit("干扰".into())),
    );
    scene::dispatch_event(
        &mut scene,
        app,
        &Event::Input(InputEvent::Text { text: "x".into() }),
    );
    assert_eq!(scene::ime_text(&scene, app).as_str(), "");
}

#[test]
fn ime_cursor_area_tracks_the_input_caret() {
    let (mut test, window, mut scene) = setup_installed();
    frame(&mut test);

    let empty = {
        let app = test.app_mut();
        let _ = app.focus_prev(window).expect("输入框");
        scene::tick(&mut scene, app);
        app.ime_cursor_area(window)
            .expect("窗口存在")
            .expect("输入框聚焦后必须有 caret hint")
    };
    // 输入框：卡片 (24,484) + 字段 (18,40) = (42,524,544,54)；正文子节点再内缩 (14,12)。
    assert!(empty.origin.x >= 56.0, "{empty:?}");
    assert!(empty.origin.y >= 536.0, "{empty:?}");
    assert!(empty.max().y <= 578.0, "{empty:?}");

    {
        let app = test.app_mut();
        scene::dispatch_event(
            &mut scene,
            app,
            &Event::Ime(ImeEvent::Commit("你好".into())),
        );
        scene::tick(&mut scene, app);
    }
    // 文本信号改了，必须让 text pass 重建 ShapedText 后 caret 几何才可解析。
    assert_eq!(scene::ime_text(&scene, test.app()).as_str(), "你好");
    frame(&mut test);
    let after = test
        .app()
        .ime_cursor_area(window)
        .expect("窗口存在")
        .expect("提交文字后仍有 caret hint");
    assert!(
        after.origin.x > empty.origin.x + 20.0,
        "caret 必须跟随已提交文本向右移动：{empty:?} -> {after:?}"
    );

    {
        let app = test.app_mut();
        let _ = app.set_focus(window, None);
        scene::tick(&mut scene, app);
    }
    assert_eq!(
        test.app().ime_cursor_area(window).expect("窗口存在"),
        None,
        "焦点离开输入框必须清除 hint"
    );
}

#[test]
fn escape_during_composition_does_not_close_the_window() {
    let (mut test, window, mut scene) = setup_installed();
    frame(&mut test);
    let app = test.app_mut();
    let _ = app.focus_prev(window).expect("输入框");

    // 覆盖「平台在组合开始时发 Start」的路径（Windows IMM32 IME）；Linux/macOS 平台层不发 Start，
    // 经 Preedit 进入组合态。本机 Windows 11 MSPY 组合期间不发 Start，属 V-104 的平台边界。
    scene::dispatch_event(&mut scene, app, &Event::Ime(ImeEvent::Start));
    scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::Escape)));
    assert!(
        app.window(window).is_some(),
        "组合中的 Esc 是取消组合（平台处理），不得关窗"
    );

    scene::dispatch_event(&mut scene, app, &Event::Ime(ImeEvent::End));
    scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::Escape)));
    assert!(app.window(window).is_none(), "组合结束后的 Esc 关窗退出");
}

#[test]
fn the_cjk_paragraph_wraps_within_its_card() {
    let (mut test, _window, _scene) = setup();
    frame(&mut test);
    let frame = test
        .frame_record()
        .frame
        .as_ref()
        .expect("提交帧必须有绘制快照");

    // 混排段落：卡片 x=24 + 18 = 42 起、宽 544；只数 CJK 卡片（x < 604）里的字形 run，
    // 否则右侧 ④ 卡片同 y 区间的状态/提示文字也会被算进来。
    let wrapped = frame
        .primitives
        .iter()
        .filter(|primitive| {
            primitive.kind == PrimitiveKind::GlyphRun
                && primitive.bounds.min().x < 604.0
                && primitive.bounds.min().y >= 405.0
                && primitive.bounds.max().y <= 462.0
                && primitive.bounds.size.width > 40.0
        })
        .count();
    assert!(
        wrapped >= 2,
        "544px 内的长段落必须折行（至少 2 条 run）：{wrapped}"
    );
}

#[test]
fn arrow_key_updates_the_slider_and_its_a11y_value() {
    let (mut test, window, mut scene) = setup_installed();
    test.enable_a11y_capture();
    frame(&mut test);

    {
        let app = test.app_mut();
        // 输入框是第 7 个、滑块是第 6 个可聚焦节点：反向两次落在滑块上。
        let _ = app.focus_prev(window).expect("输入框");
        let focused = app.focus_prev(window).expect("滑块");
        assert_eq!(app.focused(window), Some(focused));
        scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::ArrowRight)));
    }
    frame(&mut test);
    // 第二步：70% → 80%，状态行与上一笔**等宽**（只靠 LAYOUT 变化触发 a11y 重发是不够的）。
    {
        let app = test.app_mut();
        scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::ArrowRight)));
    }
    frame(&mut test);

    let snapshot = test.a11y().expect("A11Y 变化必须产生交付");
    let slider = snapshot
        .nodes()
        .iter()
        .find(|node| node.role == A11yRole::Slider)
        .expect("滑块的增量记录");
    assert_eq!(
        slider.value.as_deref(),
        Some("80"),
        "方向键改状态后 `bind_a11y` 的 value 必须重发"
    );
    let status = snapshot
        .nodes()
        .iter()
        .find(|node| {
            node.label_text()
                .is_some_and(|text| text.starts_with("最近 AT 动作"))
        })
        .expect("等宽替换后的状态行也必须重发（文本内容变化 → A11Y）");
    assert_eq!(status.value.as_deref(), Some("最近 AT 动作：方向键 → 80%"));
}

#[test]
fn leaving_the_input_cancels_the_composition_so_escape_still_quits() {
    let (mut test, window, mut scene) = setup_installed();
    frame(&mut test);
    let app = test.app_mut();
    let input = app.focus_prev(window).expect("输入框");
    assert_eq!(app.focused(window), Some(input));

    scene::dispatch_event(
        &mut scene,
        app,
        &Event::Ime(ImeEvent::Preedit("shi".into())),
    );
    // Tab 离开输入框：钩子必须收起组合态，否则 Esc 会被一直当成「组合中」。
    let _ = app.focus_next(window);
    scene::tick(&mut scene, app);
    scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::Escape)));

    assert!(
        app.window(window).is_none(),
        "失焦收起组合后 Esc 必须能关窗"
    );
}

#[test]
fn window_focus_loss_cancels_the_composition_too() {
    let (mut test, window, mut scene) = setup_installed();
    frame(&mut test);
    let app = test.app_mut();
    let _ = app.focus_prev(window).expect("输入框");

    scene::dispatch_event(&mut scene, app, &Event::Ime(ImeEvent::Preedit("ni".into())));
    scene::dispatch_event(&mut scene, app, &Event::FocusChanged { focused: false });
    scene::dispatch_event(&mut scene, app, &Event::Input(press(Key::Escape)));

    assert!(
        app.window(window).is_none(),
        "窗口失焦收起组合后 Esc 必须能关窗"
    );
}

#[test]
fn preview_seeds_the_snapshot_state() {
    let (mut test, window, scene) = setup();
    test.enable_a11y_capture();
    // 先跑一帧让 `interactive: true` 落地，否则 `set_focus` 会拒绝尚未求值的节点。
    frame(&mut test);
    scene.preview(test.app_mut());
    frame(&mut test);

    assert_eq!(scene::ime_text(&scene, test.app()).as_str(), "你好，Kytrix");

    let snapshot = test.a11y().expect("preview 的状态变化必须产生 a11y 交付");
    let slider = snapshot
        .nodes()
        .iter()
        .find(|node| node.role == A11yRole::Slider)
        .expect("滑块");
    assert_eq!(slider.value.as_deref(), Some("70"), "种子滑块必须是 70%");
    let radio_b = snapshot
        .nodes()
        .iter()
        .find(|node| node.label_text() == Some("选项 B"))
        .expect("选项 B");
    assert_eq!(radio_b.checked, Some(true), "种子必须选中单选 B");
    let swatch_2 = snapshot
        .nodes()
        .iter()
        .find(|node| node.label_text() == Some("色块 2（绿）"))
        .expect("色块 2");
    assert_eq!(swatch_2.checked, Some(true), "种子必须选中色块 2");

    let preview_focus = test.app().focused(window).expect("preview 必须给焦点");
    // Tab 序的最后一个可聚焦节点是输入框；清空焦点后 `focus_prev` 取到的就是它。
    let input = {
        let app = test.app_mut();
        let _ = app.set_focus(window, None);
        app.focus_prev(window).expect("输入框")
    };
    assert_eq!(
        preview_focus, input,
        "截图种子必须把焦点给输入框（光标 + 焦点环可见）"
    );
}
