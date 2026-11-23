use std::time::{Duration, Instant};

use smol_str::SmolStr;

use crate::{
    a11y::{A11yOverride, A11yRole, A11yTreeSnapshot},
    event::{Action, ActionSource, Event, ImeEvent, InputEvent, KeyCode, Modifiers, PointerButton},
    interaction::{
        DragPhase, OverlayDismiss, OverlayOptions, OverlayPlacement, ScrollAxis, ScrollConfig,
    },
    math::{Point, Rect, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::Color,
        style::{Clip, FocusRing, PaintSpec, Style, StylePatch},
        style_builder::StyleBuilder,
    },
    test_support::{
        app::TestApp,
        pixels::{self, ScrollSlot},
        probe::Selector,
        record::FrameRecord,
        snapshot::SnapshotImage,
    },
    text::TextStyle,
    window::WindowOptions,
};

const WINDOW: Size = Size {
    width: 640.0,
    height: 480.0,
};
const COLOR_TOLERANCE: u8 = 2;

const BACKDROP: [u8; 4] = [16, 20, 28, 255];
const BUTTON_IDLE: [u8; 4] = [40, 48, 62, 255];
const BUTTON_ON: [u8; 4] = [46, 204, 113, 255];
const INPUT_FIELD: [u8; 4] = [24, 30, 42, 255];
const HEADER: [u8; 4] = [90, 110, 140, 255];
const ROW_COLORS: [[u8; 4]; 6] = [
    [200, 40, 40, 255],
    [40, 200, 40, 255],
    [40, 40, 200, 255],
    [200, 200, 40, 255],
    [200, 40, 200, 255],
    [40, 200, 200, 255],
];
const SLOT: [u8; 4] = [52, 60, 78, 255];
const CARD: [u8; 4] = [220, 70, 60, 255];
const ZONE: [u8; 4] = [30, 60, 110, 255];
const ZONE_HOT: [u8; 4] = [70, 200, 120, 255];
const RING: [u8; 4] = [0, 160, 255, 255];
const PANEL: [u8; 4] = [60, 52, 90, 255];

#[derive(Clone, Debug)]
pub struct ScenarioReport {
    pub name: &'static str,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SmokeReport {
    pub backend: &'static str,
    pub scenarios: Vec<ScenarioReport>,
}

impl SmokeReport {
    #[must_use]
    pub fn evidence_count(&self) -> usize {
        self.scenarios
            .iter()
            .map(|scenario| scenario.evidence.len())
            .sum()
    }
}

#[must_use]
pub fn backend_name() -> &'static str {
    #[cfg(windows)]
    {
        "WARP (D3D12 software device)"
    }
    #[cfg(target_os = "macos")]
    {
        "Metal (offscreen)"
    }
    #[cfg(target_os = "linux")]
    {
        "Vulkan (offscreen)"
    }
}

#[must_use]
pub fn run_all() -> SmokeReport {
    let _guard = crate::test_support::snapshot::gpu_guard();
    SmokeReport {
        backend: backend_name(),
        scenarios: vec![
            scenario_button_click_and_pointer_cancel(),
            scenario_text_input_keyboard_and_ime(),
            scenario_scroll_wheel_composites_without_reencoding(),
            scenario_drag_drop_with_pointer_capture(),
            scenario_focus_tab_and_modal_trap(),
            scenario_a11y_tree_with_focus(),
        ],
    }
}

fn scenario_button_click_and_pointer_cancel() -> ScenarioReport {
    let mut app = app();
    let root = scene(&mut app);
    let idle = app.app_mut().signal(color(BUTTON_IDLE));
    let clicks = app.app_mut().signal(0u32);
    let button_rect = Rect::new(140.0, 100.0, 260.0, 72.0);
    let (idle_style, idle_action) = (idle, idle);
    let (clicks_action, clicks_read) = (clicks, clicks);
    let button = mount(
        &mut app,
        root,
        NodeBuilder::boxed()
            .layout_override(button_rect)
            .paint(PaintSpec {
                background: color(BUTTON_IDLE),
                ..PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                background: Some(*cx.read(&idle_style)),
                ..StylePatch::default()
            })
            .on_click(Action::custom("smoke.button.press"))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(SmolStr::new_static("确认")),
                ..A11yOverride::default()
            })
            .id("button"),
    );
    app.app_mut()
        .on_action(Action::custom("smoke.button.press"), move |ctx| {
            let previous = *ctx.read(&clicks_action);
            ctx.write(&clicks_action, previous + 1);
            ctx.write(&idle_action, color(BUTTON_ON));
        });
    frame(&mut app);

    let window = app.window_id();
    let before = read_pixels(&app);
    assert_pixel(&before, 270, 136, BUTTON_IDLE, "初始按钮背景");
    let center = Point::new(270.0, 136.0);

    app.inject(InputEvent::pointer_down(
        center,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(
        read_u32(&app, &clicks_read),
        0,
        "按下（未抬起）不得产生点击"
    );
    let released = app.inject(InputEvent::pointer_up(
        center,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(
        released.action,
        Some(Action::custom("smoke.button.press")),
        "抬起必须在命中节点上派发点击动作"
    );
    assert_eq!(released.target, Some(button));
    assert_eq!(released.click_count, 1);
    assert_eq!(read_u32(&app, &clicks_read), 1, "点击计数器必须 +1");

    frame(&mut app);
    let after = read_pixels(&app);
    assert_pixel(&after, 270, 136, BUTTON_ON, "点击后的按钮背景");
    let clicked_span = pixels::region_diff(&before, &after, 150, 110, 390, 162);
    assert!(
        clicked_span > 128,
        "按钮内部区域必须真的变色（最大通道差 {clicked_span}）"
    );

    app.inject(InputEvent::pointer_down(
        center,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let cancelled = app.inject(InputEvent::PointerCancel {
        timestamp: Instant::now(),
    });
    assert_eq!(cancelled.action, None, "PointerCancel 不得派发动作");
    let released_after_cancel = app.inject(InputEvent::pointer_up(
        center,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(
        released_after_cancel.action, None,
        "取消之后的抬起不得再产生点击（否则「取消」形同虚设）"
    );
    assert_eq!(
        read_u32(&app, &clicks_read),
        1,
        "PointerCancel 路径不得增加点击计数"
    );
    assert_eq!(
        app.app().pointer_captured(window),
        None,
        "取消后指针会话必须整体清空"
    );

    frame(&mut app);
    let after_cancel = read_pixels(&app);
    assert_pixel(
        &after_cancel,
        270,
        136,
        BUTTON_ON,
        "取消后按钮保持已点击态（不得回滚视觉）",
    );

    app.assert_oracle();

    #[cfg(feature = "hud")]
    let render_stats_evidence: Option<String> = {
        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .enable_offscreen_renderer()
            .expect("离屏渲染器（本套件本来就要求真实 GPU）");
        let mut observed = (0u32, 0u32);
        for _ in 0..3 {
            app.app_mut().request_repaint(window).expect("请求一帧");
            frame(&mut app);
            let metrics = app.app().window(window).expect("窗口").debug_hud_metrics();
            observed = (metrics.draw_calls, metrics.instances);
            if observed.0 > 0 && observed.1 > 0 {
                break;
            }
        }
        assert!(
            observed.0 > 0,
            "离屏提交必须真的发出 draw call（HUD 的 Draw 读到 {}）—— 0 说明统计没有从后端回来",
            observed.0
        );
        assert!(
            observed.1 > 0,
            "实例缓冲必须非空（HUD 的 Inst 读到 {}）",
            observed.1
        );
        Some(format!(
            "P-B：离屏 inline 提交的绘制统计 draw_calls={} instances={}（后端录制计数，非默认值）",
            observed.0, observed.1
        ))
    };
    #[cfg(not(feature = "hud"))]
    let render_stats_evidence: Option<String> = None;

    let mut evidence = vec![
        format!(
            "idle={BUTTON_IDLE:?} → clicked={BUTTON_ON:?}（读回像素），区域最大通道差 {clicked_span}"
        ),
        format!(
            "action={:?} click_count={} target=#{}；取消后点击计数仍为 1，capture={:?}",
            released.action,
            released.click_count,
            released
                .target
                .map_or_else(|| String::from("-"), |node| node.index().to_string()),
            app.app().pointer_captured(window).map(|node| node.index()),
        ),
    ];
    evidence.extend(render_stats_evidence);

    ScenarioReport {
        name: "① 按钮点击 + PointerCancel",
        evidence,
    }
}

fn scenario_text_input_keyboard_and_ime() -> ScenarioReport {
    let mut app = app();
    let root = scene(&mut app);
    let text = app.app_mut().signal(SmolStr::new_static("AB"));
    let input_rect = Rect::new(80.0, 150.0, 320.0, 56.0);
    let input = mount(
        &mut app,
        root,
        NodeBuilder::boxed()
            .style(absolute_style(input_rect))
            .paint(PaintSpec {
                background: color(INPUT_FIELD),
                ..PaintSpec::default()
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .on_click(Action::custom("smoke.input.focus"))
            .id("input"),
    );
    let text_node = {
        let source = text;
        mount(
            &mut app,
            input,
            NodeBuilder::text(SmolStr::new_static(""), text_style(28.0))
                .layout_override(Rect::new(12.0, 10.0, 296.0, 36.0))
                .bind_text(move |cx| cx.read(&source).clone())
                .id("input.text"),
        )
    };
    let window = app.window_id();
    assert!(
        app.app_mut().set_focus(window, Some(input)),
        "输入框必须可获得焦点（声明了动作 ⇒ 可聚焦）"
    );
    frame(&mut app);
    let before = read_pixels(&app);
    let bright_before = bright_pixels(&before, 92, 160, 388, 196);
    assert!(
        bright_before > 0,
        "初始文本 AB 必须真的画出字形（亮像素 {bright_before}）"
    );

    app.dispatch_event(Event::Input(InputEvent::Text {
        text: SmolStr::new_static("C"),
        timestamp: Instant::now(),
    }));
    let delivered = app.app_mut().drain_events();
    assert!(
        delivered.iter().any(|(_, event)| matches!(
            event,
            Event::Input(InputEvent::Text { text, .. }) if text == "C"
        )),
        "Text 事件必须经观测队列到达应用侧（drain_events）：{delivered:?}"
    );
    assert_eq!(
        app.app().focused(window),
        Some(input),
        "文本输入必须先有焦点"
    );
    app.app_mut().write(&text, SmolStr::new_static("ABC"));
    frame(&mut app);
    assert!(
        app.probe().exists(Selector::Text("ABC")),
        "直接提交的字符必须进入文本节点的源文本"
    );
    let after_key = read_pixels(&app);
    let bright_key = bright_pixels(&after_key, 92, 160, 388, 196);
    assert!(
        bright_key > bright_before,
        "新字符必须画到文本区（亮像素 {bright_before} → {bright_key}）"
    );

    app.dispatch_event(Event::Ime(ImeEvent::Commit {
        text: SmolStr::new_static("中"),
        replace: None,
    }));
    let delivered = app.app_mut().drain_events();
    assert!(
        delivered.iter().any(|(_, event)| matches!(
            event,
            Event::Ime(ImeEvent::Commit { text, .. }) if text == "中"
        )),
        "IME 提交必须到达应用侧：{delivered:?}"
    );
    app.app_mut().write(&text, SmolStr::new_static("ABC中"));
    frame(&mut app);
    assert!(
        app.probe().exists(Selector::Text("ABC中")),
        "IME 提交的中文必须进入文本节点的源文本"
    );
    let after_ime = read_pixels(&app);
    let bright_ime = bright_pixels(&after_ime, 92, 160, 388, 196);
    assert!(
        bright_ime > bright_key,
        "IME 提交的中文必须画出字形（亮像素 {bright_key} → {bright_ime}）"
    );
    let ime_span = pixels::region_diff(&after_key, &after_ime, 92, 160, 388, 196);
    assert!(
        ime_span > 64,
        "IME 提交必须改变文本区像素（最大通道差 {ime_span}）"
    );

    let caret_end = "ABC中".len();
    app.app_mut()
        .set_ime_cursor_area(window, text_node, caret_end)
        .expect("文本已 shape ⇒ caret hint 可解析");
    let caret = app
        .app()
        .ime_cursor_area(window)
        .expect("caret 查询不得失败")
        .expect("刚登记过 hint");
    assert!(
        input_rect.contains(caret.origin),
        "IME caret 必须落在输入框内：caret={caret:?} input={input_rect:?}"
    );
    assert!(
        caret.size.height > 0.0,
        "caret hint 必须有非零高度（否则候选窗会摆成一条线）：{caret:?}"
    );

    app.app_mut().set_ime_enabled(window, false);
    app.dispatch_event(Event::Input(InputEvent::Text {
        text: SmolStr::new_static("D"),
        timestamp: Instant::now(),
    }));
    let delivered = app.app_mut().drain_events();
    assert!(
        delivered.iter().any(|(_, event)| matches!(
            event,
            Event::Input(InputEvent::Text { text, .. }) if text == "D"
        )),
        "无 IME 时键盘通路必须照常交付字符：{delivered:?}"
    );
    app.app_mut().write(&text, SmolStr::new_static("ABC中D"));
    frame(&mut app);
    assert!(
        app.probe().exists(Selector::Text("ABC中D")),
        "无 IME 时直接提交必须保留此前内容并追加字符"
    );
    app.app_mut().set_ime_enabled(window, true);

    ScenarioReport {
        name: "② 文本输入（键盘 + 中文 IME + 无 IME 退化）",
        evidence: vec![
            format!("亮像素：AB={bright_before} → ABC={bright_key} → ABC中={bright_ime}"),
            format!(
                "IME 提交使文本区最大通道差 {ime_span}；caret={caret:?} 落在输入框 {input_rect:?} 内"
            ),
            String::from("无 IME 退化：直接 Text 通路把 D 追加进源文本（ABC中D）"),
        ],
    }
}

fn scenario_scroll_wheel_composites_without_reencoding() -> ScenarioReport {
    let mut app = app();
    let root = scene(&mut app);
    let viewport_rect = Rect::new(40.0, 110.0, 400.0, 120.0);
    let _header = mount(
        &mut app,
        root,
        boxed(Rect::new(40.0, 60.0, 400.0, 36.0), HEADER),
    );
    let viewport = container(&mut app, root, viewport_rect, Clip::Bounds);
    let rows: Vec<NodeId> = (0..6)
        .map(|index| {
            mount(
                &mut app,
                viewport,
                boxed(
                    Rect::new(0.0, index as f32 * 40.0, 400.0, 40.0),
                    ROW_COLORS[index],
                ),
            )
        })
        .collect();
    frame(&mut app);

    let window = app.window_id();
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            viewport,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: 40.0,
            },
        )
        .expect("裁剪容器可以成为滚动容器");
    app.app_mut()
        .set_scroll_content(&handle, Size::new(400.0, 240.0))
        .expect("内容尺寸");
    app.invalidate_all_lanes();
    frame(&mut app);
    let before = read_pixels(&app);
    assert_pixel(&before, 240, 130, ROW_COLORS[0], "滚动前视口顶部是 row0");
    assert_pixel(&before, 240, 70, HEADER, "固定头部在视口外");

    let wheel = app.inject(InputEvent::scroll_lines(
        Point::new(240.0, 130.0),
        Point::new(0.0, -3.0),
        Modifiers::empty(),
    ));
    assert!(wheel.hit.is_some(), "滚轮必须命中滚动容器");

    let mut offset_frames = 0usize;
    let mut reencoded_frames = 0usize;
    let viewport_px = viewport_rect.to_physical_covering(Scale::ONE);
    let viewport_allowed = crate::math::PhysicalRect::new(
        viewport_px.x0 - 1,
        viewport_px.y0 - 1,
        viewport_px.x1 + 1,
        viewport_px.y1 + 1,
    );
    for _ in 0..40 {
        app.advance(Duration::from_millis(16));
        let record = frame(&mut app);
        if record.stats.scroll_offset_frames > 0 {
            offset_frames += 1;
            if record.stats.encoded_lanes != 0 || record.stats.uploaded_bytes != 0 {
                reencoded_frames += 1;
            }
            for rect in &record.damage {
                assert!(
                    viewport_allowed.intersect(*rect) == *rect,
                    "滚动帧的 damage 必须落在视口（含 1 px AA 外扩）内：{rect:?}"
                );
            }
        }
        let state = app.app().scroll_state(&handle).expect("滚动状态");
        if state.offset.y >= 120.0 {
            break;
        }
    }
    let state = app.app().scroll_state(&handle).expect("滚动状态");
    assert_eq!(
        state.offset.y, 120.0,
        "3 格滚轮 × line_height 40 必须滚满 120 px（clamp 到 max_offset 之后仍是 120）"
    );
    assert!(offset_frames >= 1, "必须至少有一帧真的携带偏移变化");
    assert_eq!(
        reencoded_frames, 0,
        "滚动帧不得重编码任何 lane / 上传实例字节（实测 {reencoded_frames}/{offset_frames} 帧违规）"
    );

    let baked = read_pixels(&app);
    assert_pixel(
        &baked,
        240,
        130,
        ROW_COLORS[0],
        "不喂偏移时视口顶部仍是烘焙的 row0（内容没有被重编码）",
    );

    let slots: Vec<ScrollSlot> = vec![
        (
            Point::ZERO,
            Rect::new(0.0, 0.0, WINDOW.width, WINDOW.height).to_physical_covering(Scale::ONE),
        ),
        (state.offset, viewport_rect.to_physical_covering(Scale::ONE)),
    ];
    let composited = read_pixels_with_scroll(&app, &slots);
    assert_pixel(
        &composited,
        240,
        130,
        ROW_COLORS[3],
        "合成端偏移必须把内容位移 3 行：视口顶部是 row3",
    );
    assert_pixel(
        &composited,
        240,
        210,
        ROW_COLORS[5],
        "合成端偏移后视口底部必须是 row5",
    );
    assert_pixel(&composited, 240, 70, HEADER, "固定头部不得随滚动移动");

    let hit = app
        .probe()
        .hit(Point::new(240.0, 130.0))
        .expect("视口内必须命中");
    assert_eq!(
        hit.node, rows[3],
        "滚动后的命中测试必须使用内容偏移（否则点到的还是 row0）"
    );

    let evidence = vec![
        format!(
            "offset={:?}（3 格 × 40）；偏移帧 {offset_frames} 个，重编码/上传违规 {reencoded_frames}",
            state.offset
        ),
        format!(
            "读回像素：烘焙态顶部 {:?}，喂偏移后顶部 {:?}（row3 = {:?}）、底部 {:?}（row5 = {:?}），固定头部 {:?}",
            pixels::pixel(&baked, 240, 130),
            pixels::pixel(&composited, 240, 130),
            ROW_COLORS[3],
            pixels::pixel(&composited, 240, 210),
            ROW_COLORS[5],
            pixels::pixel(&composited, 240, 70),
        ),
        format!("滚动后命中 row3 = #{}", hit.node.index()),
    ];
    ScenarioReport {
        name: "③ 滚动容器（滚轮 → 合成端偏移 → 不重编码）",
        evidence,
    }
}

fn scenario_drag_drop_with_pointer_capture() -> ScenarioReport {
    let mut app = app();
    let root = scene(&mut app);
    let source_rect = Rect::new(80.0, 140.0, 140.0, 100.0);
    let target_rect = Rect::new(380.0, 140.0, 160.0, 160.0);
    let location = app.app_mut().signal(0u8);
    let hot = app.app_mut().signal(false);
    let phases = app.app_mut().signal(Vec::<DragPhase>::new());

    let source = mount(
        &mut app,
        root,
        NodeBuilder::boxed()
            .style(absolute_style(source_rect))
            .paint(PaintSpec {
                background: color(SLOT),
                ..PaintSpec::default()
            })
            .id("slot"),
    );
    let target = {
        let highlight = hot;
        mount(
            &mut app,
            root,
            NodeBuilder::boxed()
                .style(absolute_style(target_rect))
                .paint(PaintSpec {
                    background: color(ZONE),
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    background: Some(if *cx.read(&highlight) {
                        color(ZONE_HOT)
                    } else {
                        color(ZONE)
                    }),
                    ..StylePatch::default()
                })
                .id("drop.target"),
        )
    };
    let card_source = {
        let shown = location;
        mount(
            &mut app,
            source,
            NodeBuilder::boxed()
                .layout_override(Rect::new(20.0, 10.0, 100.0, 80.0))
                .paint(PaintSpec {
                    background: color(CARD),
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    visible: Some(*cx.read(&shown) == 0),
                    ..StylePatch::default()
                })
                .on_drag(Action::custom("smoke.card.drag"))
                .id("card.source"),
        )
    };
    let _card_target = {
        let shown = location;
        mount(
            &mut app,
            target,
            NodeBuilder::boxed()
                .layout_override(Rect::new(30.0, 40.0, 100.0, 80.0))
                .paint(PaintSpec {
                    background: color(CARD),
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    visible: Some(*cx.read(&shown) == 1),
                    ..StylePatch::default()
                })
                .id("card.target"),
        )
    };
    {
        let highlight = hot;
        let drop = location;
        let seen = phases;
        app.app_mut()
            .on_action(Action::custom("smoke.card.drag"), move |ctx| {
                let ActionSource::Drag { phase, .. } = ctx.source() else {
                    return;
                };
                ctx.update(&seen, |list| list.push(phase));
                let over_target = ctx.hit().map(|hit| hit.node) == Some(target);
                match phase {
                    DragPhase::Start | DragPhase::Move => {
                        ctx.write(&highlight, over_target);
                    }
                    DragPhase::End => {
                        if over_target {
                            ctx.write(&drop, 1);
                        }
                        ctx.write(&highlight, false);
                    }
                    DragPhase::Cancel => {
                        ctx.write(&highlight, false);
                    }
                }
            });
    }
    frame(&mut app);
    let window = app.window_id();
    let before = read_pixels(&app);
    assert_pixel(&before, 150, 190, CARD, "源槽初始有卡片");
    assert_pixel(&before, 460, 220, ZONE, "目标区初始底色");

    let from = Point::new(150.0, 190.0);
    let to = Point::new(460.0, 220.0);
    app.inject(InputEvent::pointer_down(
        from,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(
        app.app().drag_state(window),
        None,
        "按下不得立刻成立拖拽（必须先越过 drag_distance 阈值）"
    );
    let started = app.inject(InputEvent::pointer_move(to, Modifiers::empty()));
    assert_eq!(
        started.action,
        Some(Action::custom("smoke.card.drag")),
        "越阈值必须派发拖拽 Start"
    );
    let drag = app.app().drag_state(window).expect("拖拽会话必须成立");
    assert_eq!(drag.node(), card_source);
    let captured = app.app().pointer_captured(window);
    assert_eq!(
        captured,
        Some(card_source),
        "拖拽必须拿到指针捕获（后续 Move/Up 都路由到源卡片）"
    );
    assert!(read_bool(&app, &hot), "拖到目标区上必须点亮 drop 指示");

    frame(&mut app);
    let over = read_pixels(&app);
    assert_pixel(
        &over,
        460,
        220,
        ZONE_HOT,
        "拖拽悬停在目标区上时目标区必须高亮",
    );

    let ended = app.inject(InputEvent::pointer_up(
        to,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(ended.action, Some(Action::custom("smoke.card.drag")));
    assert_eq!(ended.click_count, 0, "拖拽之后不得再算点击");
    assert_eq!(app.app().drag_state(window), None, "抬起必须结束会话");
    assert_eq!(
        app.app().pointer_captured(window),
        None,
        "抬起必须释放指针捕获"
    );
    assert_eq!(read_u8(&app, &location), 1, "落下必须把模型移到目标区");

    frame(&mut app);
    let dropped = read_pixels(&app);
    assert_pixel(&dropped, 460, 220, CARD, "落下后卡片必须出现在目标区");
    assert_pixel(&dropped, 150, 190, SLOT, "卡片搬走后源槽必须露出来");

    app.app_mut().write(&location, 0);
    frame(&mut app);
    app.inject(InputEvent::pointer_down(
        from,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    app.inject(InputEvent::pointer_move(to, Modifiers::empty()));
    assert_eq!(
        app.app().pointer_captured(window),
        Some(card_source),
        "第二次拖拽同样必须捕获"
    );
    app.inject(InputEvent::PointerCancel {
        timestamp: Instant::now(),
    });
    assert_eq!(
        app.app().drag_state(window),
        None,
        "PointerCancel 必须结束拖拽会话"
    );
    assert_eq!(
        app.app().pointer_captured(window),
        None,
        "PointerCancel 必须释放指针捕获"
    );
    assert_eq!(read_u8(&app, &location), 0, "取消不得改变落点");
    frame(&mut app);
    let after_cancel = read_pixels(&app);
    assert_pixel(&after_cancel, 150, 190, CARD, "取消后卡片留在源槽");
    assert_pixel(&after_cancel, 460, 220, ZONE, "取消后目标区必须撤销高亮");
    let seen = read_phases(&app, &phases);
    assert_eq!(
        seen,
        vec![
            DragPhase::Start,
            DragPhase::End,
            DragPhase::Start,
            DragPhase::Cancel
        ],
        "拖拽阶段序列必须是 Start/End + Start/Cancel"
    );

    ScenarioReport {
        name: "④ 拖拽（跨节点拖放 + 指针捕获）",
        evidence: vec![
            format!(
                "拖拽中 capture=#{:?}（源卡片）→ 悬停高亮 {:?} → 落下 {:?} / 源槽 {:?}",
                captured.map(|node| node.index()),
                pixels::pixel(&over, 460, 220),
                pixels::pixel(&dropped, 460, 220),
                pixels::pixel(&dropped, 150, 190),
            ),
            format!(
                "阶段序列 {seen:?}；取消后目标区 {:?}",
                pixels::pixel(&after_cancel, 460, 220)
            ),
        ],
    }
}

fn scenario_focus_tab_and_modal_trap() -> ScenarioReport {
    let mut app = app();
    let root = scene(&mut app);
    let counts = app.app_mut().signal([0u32; 4]);
    let mut buttons = Vec::new();
    for slot in 0..3usize {
        let action = match slot {
            0 => Action::custom("smoke.tab.a"),
            1 => Action::custom("smoke.tab.b"),
            _ => Action::custom("smoke.tab.c"),
        };
        let node = mount(
            &mut app,
            root,
            NodeBuilder::boxed()
                .layout_override(Rect::new(80.0 + slot as f32 * 160.0, 80.0, 140.0, 56.0))
                .paint(PaintSpec {
                    background: color(BUTTON_IDLE),
                    ..PaintSpec::default()
                })
                .focus_ring(focus_ring())
                .on_click(action.clone())
                .a11y(A11yOverride {
                    role: Some(A11yRole::Button),
                    label: Some(SmolStr::new_static(match slot {
                        0 => "按钮 A",
                        1 => "按钮 B",
                        _ => "按钮 C",
                    })),
                    ..A11yOverride::default()
                })
                .id(match slot {
                    0 => "tab.a",
                    1 => "tab.b",
                    _ => "tab.c",
                }),
        );
        let counter = counts;
        app.app_mut().on_action(action, move |ctx| {
            ctx.update(&counter, |values| values[slot] += 1);
        });
        buttons.push(node);
    }
    let window = app.window_id();
    frame(&mut app);

    let rects = [
        Rect::new(80.0, 80.0, 140.0, 56.0),
        Rect::new(240.0, 80.0, 140.0, 56.0),
        Rect::new(400.0, 80.0, 140.0, 56.0),
    ];
    assert_eq!(app.app().focused(window), None, "初始无焦点");
    for expected in [0usize, 1, 2, 0] {
        key(&mut app, KeyCode::Tab, Modifiers::empty());
        assert_eq!(
            app.app().focused(window),
            Some(buttons[expected]),
            "Tab 第 {} 次必须落到按钮 {}",
            expected + 1,
            expected
        );
    }
    key(&mut app, KeyCode::Tab, Modifiers::SHIFT);
    assert_eq!(
        app.app().focused(window),
        Some(buttons[2]),
        "Shift+Tab 必须反向回绕到 C"
    );

    frame(&mut app);
    let focused_c = read_pixels(&app);
    let ring_c = ring_pixels(&focused_c, rects[2]);
    let ring_a_off = ring_pixels(&focused_c, rects[0]);
    assert!(
        ring_c > 40,
        "焦点在 C 时必须画出 C 的焦点环（环像素 {ring_c}）"
    );
    assert_eq!(ring_a_off, 0, "未聚焦的 A 不得有焦点环");

    assert!(app.app_mut().set_focus(window, Some(buttons[0])));
    frame(&mut app);
    let focused_a = read_pixels(&app);
    assert!(
        ring_pixels(&focused_a, rects[0]) > 40,
        "焦点移到 A 后 A 的焦点环必须画出来"
    );
    assert_eq!(
        ring_pixels(&focused_a, rects[2]),
        0,
        "焦点离开 C 后 C 的焦点环必须消失"
    );

    let modal = app
        .app_mut()
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(180.0, 150.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("打开模态浮层");
    let panel = mount(
        &mut app,
        modal.node(),
        NodeBuilder::boxed()
            .style(absolute_style(Rect::new(0.0, 0.0, 300.0, 180.0)))
            .paint(PaintSpec {
                background: color(PANEL),
                ..PaintSpec::default()
            })
            .id("modal.panel"),
    );
    let mut modal_buttons = Vec::new();
    for (slot, action, x) in [
        (2usize, Action::custom("smoke.modal.m1"), 20.0f32),
        (3, Action::custom("smoke.modal.m2"), 160.0),
    ] {
        let node = mount(
            &mut app,
            panel,
            NodeBuilder::boxed()
                .layout_override(Rect::new(x, 40.0, 120.0, 44.0))
                .paint(PaintSpec {
                    background: color(BUTTON_IDLE),
                    ..PaintSpec::default()
                })
                .focus_ring(focus_ring())
                .on_click(action.clone())
                .id(match slot {
                    2 => "modal.m1",
                    _ => "modal.m2",
                }),
        );
        let counter = counts;
        app.app_mut().on_action(action, move |ctx| {
            ctx.update(&counter, |values| values[slot] += 1);
        });
        modal_buttons.push(node);
    }
    frame(&mut app);
    assert_eq!(app.app().overlays(window).len(), 1, "浮层必须挂上");

    let modal_ids = [modal_buttons[0].index(), modal_buttons[1].index()];
    let mut seen = [false; 2];
    for _ in 0..4 {
        key(&mut app, KeyCode::Tab, Modifiers::empty());
        let focused = app
            .app()
            .focused(window)
            .expect("模态作用域内必须始终有焦点");
        assert!(
            modal_buttons.contains(&focused),
            "Tab 把焦点带出了模态（焦点 #{:?}）",
            focused.index()
        );
        let slot = usize::from(focused != modal_buttons[0]);
        seen[slot] = true;
    }
    assert!(
        seen == [true, true],
        "Tab 必须遍历模态内的两个按钮（实际 {seen:?}，id={modal_ids:?}）"
    );
    app.app_mut().set_focus(window, Some(modal_buttons[0]));
    assert_eq!(
        app.app().focused(window),
        Some(modal_buttons[0]),
        "模态内可以直接指定焦点"
    );
    assert!(
        !app.app_mut().set_focus(window, Some(buttons[0])),
        "模态作用域必须拒绝背景节点获得焦点"
    );
    assert_eq!(
        app.app().focused(window),
        Some(modal_buttons[0]),
        "被拒绝的 set_focus 不得改变焦点"
    );

    let background_center = Point::new(150.0, 108.0);
    let before_counts = read_counts(&app, &counts);
    app.inject(InputEvent::pointer_down(
        background_center,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let up = app.inject(InputEvent::pointer_up(
        background_center,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(up.action, None, "模态外的点击必须被消费");
    assert_eq!(
        read_counts(&app, &counts),
        before_counts,
        "模态外的点击不得触发背景动作"
    );

    frame(&mut app);
    let modal_pixels = read_pixels(&app);
    assert_pixel(&modal_pixels, 330, 240, PANEL, "模态面板必须真的画出来");

    key(&mut app, KeyCode::Escape, Modifiers::empty());
    assert!(
        app.app().overlays(window).is_empty(),
        "Esc（Action::Dismiss）必须关闭最上层浮层"
    );
    assert_eq!(
        app.app().focused(window),
        Some(buttons[0]),
        "关闭后焦点必须还给 opener（A）"
    );
    frame(&mut app);
    let closed = read_pixels(&app);
    assert_pixel(&closed, 330, 240, BACKDROP, "关闭后浮层区域必须回到背景");
    assert!(
        ring_pixels(&closed, rects[0]) > 40,
        "焦点还原后 A 的焦点环必须画出来"
    );
    assert_eq!(
        ring_pixels(&closed, Rect::new(200.0, 190.0, 120.0, 44.0)),
        0,
        "浮层关闭后模态按钮的焦点环必须消失"
    );

    ScenarioReport {
        name: "⑤ 焦点 / Tab + 模态焦点陷阱",
        evidence: vec![
            format!(
                "Tab 序 A→B→C→A、Shift+Tab→C；C 聚焦时环像素 {ring_c}（A 未聚焦 {ring_a_off}），移到 A 后 A 环像素 {}",
                ring_pixels(&focused_a, rects[0]),
            ),
            format!(
                "模态：焦点校正进 m1 → Tab 回绕回 m1；背景 set_focus 被拒绝（仍停在模态内），背景点击动作={:?}",
                up.action,
            ),
            format!(
                "Esc 后浮层数={}、焦点还原为 #{:?}、面板像素 {:?} → {:?}",
                app.app().overlays(window).len(),
                app.app().focused(window).map(|node| node.index()),
                pixels::pixel(&modal_pixels, 330, 240),
                pixels::pixel(&closed, 330, 240),
            ),
        ],
    }
}

fn scenario_a11y_tree_with_focus() -> ScenarioReport {
    let mut app = app();
    app.enable_a11y_capture();
    let root = scene(&mut app);
    let button_rect = Rect::new(80.0, 80.0, 200.0, 56.0);
    let input_rect = Rect::new(80.0, 170.0, 280.0, 56.0);
    let button = mount(
        &mut app,
        root,
        NodeBuilder::boxed()
            .layout_override(button_rect)
            .paint(PaintSpec {
                background: color(BUTTON_IDLE),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("smoke.a11y.button"))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(SmolStr::new_static("提交")),
                ..A11yOverride::default()
            })
            .id("a11y.submit"),
    );
    let input = mount(
        &mut app,
        root,
        NodeBuilder::boxed()
            .style(absolute_style(input_rect))
            .paint(PaintSpec {
                background: color(INPUT_FIELD),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("smoke.a11y.input"))
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .id("a11y.input"),
    );
    let _text = mount(
        &mut app,
        input,
        NodeBuilder::text(SmolStr::new_static("牛"), text_style(24.0))
            .layout_override(Rect::new(8.0, 10.0, 260.0, 36.0))
            .id("a11y.input.text"),
    );
    let _title = mount(
        &mut app,
        root,
        NodeBuilder::text(SmolStr::new_static("行情"), text_style(20.0))
            .layout_override(Rect::new(400.0, 80.0, 180.0, 40.0))
            .id("a11y.title"),
    );
    let window = app.window_id();
    frame(&mut app);
    app.assert_a11y_contract();
    let full = a11y_snapshot(&app);
    assert!(full.full, "首帧必须是全树 a11y 交付");
    let first_update = app.a11y_update().expect("首帧必须有 TreeUpdate").clone();
    assert!(
        first_update.tree.is_some(),
        "全树 TreeUpdate 必须带 TreeInfo"
    );
    assert!(
        first_update.nodes.len() >= 6,
        "全树至少要含根 + 两个控件 + 文本节点（实际 {}）",
        first_update.nodes.len()
    );
    let roles: Vec<A11yRole> = full.nodes().iter().map(|node| node.role).collect();
    for expected in [A11yRole::Button, A11yRole::TextInput, A11yRole::TextRun] {
        assert!(
            roles.contains(&expected),
            "a11y 角色集合必须包含 {expected:?}：{roles:?}"
        );
    }

    key(&mut app, KeyCode::Tab, Modifiers::empty());
    assert_eq!(app.app().focused(window), Some(button));
    frame(&mut app);
    app.assert_a11y_contract();
    let focused = a11y_snapshot(&app);
    assert!(!focused.full, "只移焦点的下一笔必须是增量（不是变相全量）");
    let focus_update = app
        .a11y_update()
        .expect("焦点变化必须交付 TreeUpdate")
        .clone();
    assert_eq!(
        focus_update.focus.0,
        focused.focus().get(),
        "TreeUpdate.focus 必须与快照焦点是同一个 id"
    );
    assert!(
        focus_update
            .nodes
            .iter()
            .any(|(id, _)| *id == focus_update.focus),
        "焦点节点必须与本笔一起送达（否则 consumer 指向不存在的节点）"
    );
    assert_eq!(
        focused.focus_node().engine,
        button,
        "a11y 焦点必须落在按钮的引擎句柄上"
    );
    assert_eq!(focused.focus_node().role, A11yRole::Button);
    assert_eq!(focused.focus_node().label_text(), Some("提交"));
    let expected_bounds = button_rect.to_physical_covering(Scale::ONE);
    assert_eq!(
        focused.focus_node().bounds,
        expected_bounds,
        "a11y 焦点节点的边界必须是物理像素布局盒"
    );

    key(&mut app, KeyCode::Tab, Modifiers::empty());
    assert_eq!(app.app().focused(window), Some(input));
    frame(&mut app);
    app.assert_a11y_contract();
    let input_focus = a11y_snapshot(&app);
    assert_eq!(input_focus.focus_node().engine, input);
    assert_eq!(input_focus.focus_node().role, A11yRole::TextInput);
    assert_eq!(input_focus.focus_node().label_text(), Some("搜索"));

    let runs: Vec<&crate::a11y::A11yNode> = full
        .nodes()
        .iter()
        .filter(|node| node.role == A11yRole::TextRun)
        .collect();
    assert_eq!(runs.len(), 1, "只有输入框内的文本是 TextRun：{runs:?}");
    let niu = runs
        .iter()
        .find(|node| node.value.as_deref() == Some("牛"))
        .expect("输入框文本「牛」必须作为 TextRun 出现在全树里");
    assert_eq!(
        niu.character_lengths,
        vec![3u8],
        "「牛」必须是 1 个字符 / 3 个 UTF-8 字节"
    );
    let title_run = full
        .nodes()
        .iter()
        .find(|node| node.role == A11yRole::Label && node.value.as_deref() == Some("行情"))
        .expect("标题文本「行情」必须作为 Label 出现在全树里");
    assert_eq!(
        title_run.character_lengths,
        vec![3u8, 3u8],
        "「行情」必须是 2 个字符 / 各 3 个 UTF-8 字节"
    );

    app.assert_a11y_oracle();

    ScenarioReport {
        name: "⑥ 有焦点时的 a11y 树",
        evidence: vec![
            format!("首帧全树 {} 节点，角色 {roles:?}", first_update.nodes.len()),
            format!(
                "焦点笔：增量 {} 节点，focus={:?}，按钮「提交」边界 {:?}",
                focus_update.nodes.len(),
                focused.focus_node().label_text(),
                focused.focus_node().bounds,
            ),
            format!(
                "输入框焦点：{:?} /「搜索」；TextRun 度量 牛={:?} 行情={:?}；oracle 一致",
                input_focus.focus_node().role,
                niu.character_lengths,
                title_run.character_lengths
            ),
        ],
    }
}

fn app() -> TestApp {
    TestApp::with_options(WindowOptions {
        title: SmolStr::new_static("kytrix gui smoke"),
        size: WINDOW,
        scale: Scale::ONE,
        ..WindowOptions::default()
    })
}

fn scene(app: &mut TestApp) -> NodeId {
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();
    mount(
        app,
        root,
        boxed(Rect::new(0.0, 0.0, WINDOW.width, WINDOW.height), BACKDROP),
    );
    root
}

fn mount(app: &mut TestApp, parent: NodeId, builder: NodeBuilder) -> NodeId {
    let window = app.window_id();
    builder
        .build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            parent,
        )
        .expect("挂载节点")
}

fn boxed(rect: Rect, background: [u8; 4]) -> NodeBuilder {
    NodeBuilder::boxed().layout_override(rect).paint(PaintSpec {
        background: color(background),
        ..PaintSpec::default()
    })
}

fn container(app: &mut TestApp, parent: NodeId, rect: Rect, clip: Clip) -> NodeId {
    mount(
        app,
        parent,
        NodeBuilder::boxed().style({
            let mut style = absolute_style(rect);
            style.clip = clip;
            style
        }),
    )
}

fn absolute_style(rect: Rect) -> Style {
    StyleBuilder::new()
        .absolute()
        .left(rect.origin.x)
        .top(rect.origin.y)
        .size(rect.size.width, rect.size.height)
        .build()
}

fn color(rgba: [u8; 4]) -> Color {
    Color::from_rgba8(rgba)
}

fn text_style(size: f32) -> TextStyle {
    TextStyle {
        size,
        color: Color::WHITE,
        ..TextStyle::default()
    }
}

fn frame(app: &mut TestApp) -> FrameRecord {
    app.drain().expect("帧推进失败").clone()
}

fn read_u8(app: &TestApp, signal: &Signal<u8>) -> u8 {
    *app.app().read(signal)
}

fn read_u32(app: &TestApp, signal: &Signal<u32>) -> u32 {
    *app.app().read(signal)
}

fn read_bool(app: &TestApp, signal: &Signal<bool>) -> bool {
    *app.app().read(signal)
}

fn read_counts(app: &TestApp, signal: &Signal<[u32; 4]>) -> [u32; 4] {
    *app.app().read(signal)
}

fn read_phases(app: &TestApp, signal: &Signal<Vec<DragPhase>>) -> Vec<DragPhase> {
    app.app().read(signal).clone()
}

fn read_pixels(app: &TestApp) -> SnapshotImage {
    app.read_pixels().unwrap_or_else(|error| {
        panic!(
            "像素读回失败（本平台必须能离屏读回）：{}",
            pixels::describe_error(&error)
        )
    })
}

fn read_pixels_with_scroll(app: &TestApp, slots: &[ScrollSlot]) -> SnapshotImage {
    app.read_pixels_with_scroll(slots).unwrap_or_else(|error| {
        panic!(
            "带偏移的像素读回失败（本平台必须能离屏读回）：{}",
            pixels::describe_error(&error)
        )
    })
}

fn assert_pixel(image: &SnapshotImage, x: u32, y: u32, expected: [u8; 4], what: &str) {
    let actual = pixels::pixel(image, x, y);
    let span = pixels::channel_span(actual, expected);
    assert!(
        span <= COLOR_TOLERANCE,
        "{what}：像素 ({x},{y}) = {actual:?}，期望 {expected:?}（最大通道差 {span} > {COLOR_TOLERANCE}）"
    );
}

fn bright_pixels(image: &SnapshotImage, x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    pixels::count_pixels(image, x0, y0, x1, y1, |pixel| {
        pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200
    })
}

fn ring_pixels(image: &SnapshotImage, rect: Rect) -> u32 {
    let outer = rect.outset(6.0).to_physical_covering(Scale::ONE);
    pixels::count_pixels(
        image,
        outer.x0.max(0) as u32,
        outer.y0.max(0) as u32,
        outer.x1.max(0) as u32,
        outer.y1.max(0) as u32,
        |pixel| pixels::channel_span(pixel, RING) <= 4,
    )
}

fn focus_ring() -> FocusRing {
    FocusRing::new(2.0, 2.0, color(RING))
}

fn key(app: &mut TestApp, code: KeyCode, modifiers: Modifiers) {
    app.inject(InputEvent::key_down(code, modifiers));
}

fn a11y_snapshot(app: &TestApp) -> A11yTreeSnapshot {
    app.a11y().expect("本帧必须有一笔 a11y 交付").clone()
}
