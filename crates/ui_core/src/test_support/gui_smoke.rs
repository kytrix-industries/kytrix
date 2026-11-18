//! GUI 端到端冒烟（AR5 交付缺口：`examples/acceptance` 只测帧率/CPU，不测交互）。
//!
//! 用**现有 API**（`TestApp` + `inject`/`dispatch_event` + `Probe` + a11y 捕获 +
//! `read_pixels` 真实 GPU 读回）把一条完整回路走通：
//!
//! ```text
//! 输入事件 → 命中/焦点 → 状态写入 → 样式/文本 pass → 布局 → 绘制 → 实例编码
//!          → 合成（含滚动槽偏移）→ GPU 渲染 → 像素读回 → 断言
//!          └──────────────→ a11y 树 / TreeUpdate → 断言
//! ```
//!
//! 六个场景（每个都在**真实像素**与引擎事实上双断言，见各函数文档里的「变异验证」）：
//!
//! 1. 按钮点击（指针按下/抬起 + `PointerCancel` 路径）：点击后按钮背景色必须变；
//! 2. 文本输入（键盘字符 + 中文 IME 提交；无 IME 时走直接提交）：提交的字符必须出现在文本区
//!    并真的画出字形，IME caret hint 必须落在输入框内；
//! 3. 滚动容器（滚轮 → 合成端偏移 → 不重编码）：滚动帧不得重编码/上传实例字节，读回像素
//!    必须恰好位移「3 行」；
//! 4. 拖拽（跨节点拖放 + 指针捕获）：拖拽中捕获在源卡片上、悬停高亮跟随、落下后卡片出现在
//!    目标区、取消后回到原位；
//! 5. 焦点 / Tab 遍历 + 模态对话框焦点陷阱：Tab 序回绕、焦点环像素随焦点移动、模态内循环、
//!    背景点击被消费、Esc 关闭并还原焦点；
//! 6. 有焦点时的 a11y 树形状：全树/增量交付、焦点 id、角色/名字、焦点与布局盒一致、
//!    增量 vs 全量 oracle。
//!
//! # 运行
//!
//! ```text
//! cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture
//! cargo run  -p ui_core --features test-support --example acceptance -- interaction
//! ```
//!
//! **默认 `cargo test` 不跑**（集成测试标了 `#[ignore]`）：离屏读回要建 GPU 设备，默认测试面
//! 保持零 GPU 依赖。**CI 友好**：headless 全程不建平台窗口、不需要显示服务器 / 合成器 ——
//! Linux 上没有 `WAYLAND_DISPLAY` 时走的正是这条离屏路径（Vulkan 离屏 + 读回，如 lavapipe）。
//!
//! # 为什么由测试扮演「应用」
//!
//! 引擎不内建文本编辑器 / 拖放模型（I9：交互语义归应用）。因此本套件里的 `on_action` 处理器
//! 就是**最简应用**：它们写信号（文本内容、落点、高亮），引擎负责其余整条链路。断言因此仍然
//! 覆盖「输入 → 应用状态 → 引擎 pass → 像素 / a11y」，而不是绕过引擎直接改树。

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

/// 冒烟窗口的逻辑尺寸（DPI 恒 1：像素坐标 = 逻辑坐标，断言可读）。
const WINDOW: Size = Size {
    width: 640.0,
    height: 480.0,
};
/// 颜色断言容差（三平台 sRGB 量化；只取图元内部点，2 足够）。
const COLOR_TOLERANCE: u8 = 2;

// ── 场景配色（不透明；预乘空间里 alpha = 255 时与直通色一致）─────────────────

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

// ── 报告 ────────────────────────────────────────────────────────────────────

/// 一个场景的机器可读结果（证据行是**实测值**，不是「通过」两个字）。
#[derive(Clone, Debug)]
pub struct ScenarioReport {
    /// 场景名。
    pub name: &'static str,
    /// 实测证据（像素值 / 偏移 / 焦点 / 计数…）。
    pub evidence: Vec<String>,
}

/// 整套冒烟的报告（断言失败会 panic，能走到这里 = 全绿）。
#[derive(Clone, Debug)]
pub struct SmokeReport {
    /// 像素读回走的后端（WARP / Metal / Vulkan）。
    pub backend: &'static str,
    /// 六个场景的实测证据。
    pub scenarios: Vec<ScenarioReport>,
}

impl SmokeReport {
    /// 证据行总数（集成测试用它确认每个场景都真的跑了断言）。
    #[must_use]
    pub fn evidence_count(&self) -> usize {
        self.scenarios
            .iter()
            .map(|scenario| scenario.evidence.len())
            .sum()
    }
}

/// 当前平台的离屏读回后端名（报告用）。
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

/// 跑全部六个场景；任何断言失败即 panic（消息带实测值与期望值）。
///
/// GPU 读回是进程级串行资源（与 `tests/snapshot.rs` 共用 `gpu_guard`）：整个套件持锁执行，
/// 避免多个 `#[test]` 并行时各自建设备互相拖垮。
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

// ── 场景 ①：按钮点击 + PointerCancel ────────────────────────────────────────

/// 按钮点击（指针按下/抬起 + `PointerCancel` 路径）。
///
/// 断言链：`pointer_down` 不触发动作 → `pointer_up` 触发 `ActionSource::Click` 且只计一次 →
/// 应用态（点击计数 + 背景色信号）真的变了 → **读回像素**里按钮背景从 `BUTTON_IDLE` 变成
/// `BUTTON_ON`；按下后 `PointerCancel` 再抬起**不得**触发点击（Windows `WM_CAPTURECHANGED` /
/// macOS `resignKey` / Wayland 指针 leave 的真实路径），像素保持原状。
///
/// 变异验证：把 `on_action` 里的背景色写入删掉 ⇒ 变色断言红；让 `cancel_pointer_session`
/// 不清按压记录 ⇒ 取消后的抬起会计数，计数断言红；把 `dispatch_input` 的指针抬起分支去掉 ⇒
/// 动作断言红。
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
            // 最简应用：记一次点击 + 把按钮切成「已点击」配色。
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

    // ── PointerCancel 路径：按下后系统取消 ⇒ 不得提交 ──
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

    // 绘制 oracle：增量路径必须与全量重绘逐图元一致（点击帧走过增量编码）。
    app.assert_oracle();

    // ── P-B：Draw/Inst 必须是后端录制出的真实计数，而不是默认值 ──────────────────
    //
    // 前面的步骤都是 headless 窗口（没有挂渲染器）：`commit_frame` 不会 publish，`render_stats`
    // 就没有来源。这里把真实离屏渲染器挂到窗口上，让本帧走 inline 提交（同步返回统计），断言
    // HUD 指标拿到的是后端录制的 draw_calls / instances。
    //
    // threaded 路径（真实窗口）的同一指标经 `FrameFeedback` 回来，由
    // `window/record.rs::note_completed_frame` 的单测覆盖 —— 离屏套件不建平台窗口，测不到那条链。
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

// ── 场景 ②：文本输入（键盘 + 中文 IME + 无 IME 退化）────────────────────────

/// 文本输入：键盘字符、中文 IME 提交、无 IME 时的直接提交，以及 IME caret hint。
///
/// 引擎不内建文本编辑器：应用从 `App::drain_events` 读 `InputEvent::Text` / `ImeEvent::Commit`
/// （这里由测试扮演应用），把字符写进 `Signal<SmolStr>`，文本节点 `bind_text` 重排 → 重绘。
///
/// 断言链：事件真的到达应用侧（观测队列）→ 文本内容进了树（`Selector::Text`）→ **读回像素**里
/// 字形数变多（`ABC` 的亮像素 > `AB`，`ABC中` 又多出 IME 提交的字形）→ `IME caret hint`
/// 落在输入框内（平台据此摆候选窗）。
///
/// 变异验证：把 `dispatch` 的 `Event::Input` 入队删掉 ⇒ 事件断言红；让文本节点不接
/// `bind_text` ⇒ 树/像素断言红；把 caret hint 的字形换算写死成 (0,0) ⇒ caret 断言红。
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

    // ②-a 键盘字符（无 IME 的直接提交路径）。
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

    // ②-b 中文 IME 提交（无 IME 的机器上由平台层直接发 Text；提交路径本身与平台无关）。
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

    // ②-c IME caret hint：平台每轮读它去摆候选窗 / 组合窗。
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

    // ②-d 无 IME 退化：显式关闭 IME 后，直接 Text 通路仍必须完成插入。
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

// ── 场景 ③：滚动（滚轮 → 合成端偏移 → 不重编码）────────────────────────────

/// 滚动容器：滚轮 → 合成端偏移 → **不重编码**。
///
/// 引擎的滚动是「合成端偏移」：内容烘焙在**不含本槽位移**的坐标空间里，滚动只改变
/// `FramePayload.scroll` 的槽位偏移表。因此这条用例有四个互相独立的断言面：
///
/// 1. 滚轮（`ScrollUnit::Line`，3 格 × `line_height 40`）最终把偏移推进到 120；
/// 2. 每一个「偏移变化帧」都满足 `encoded_lanes == 0` 且 `uploaded_bytes == 0`（不重编码、
///    不重传实例），damage 落在视口内；
/// 3. **不喂合成偏移**读回：视口顶部仍是烘焙的 row0（像素侧证明「内容没被重编码」）；
/// 4. **喂合成偏移**读回：视口顶部恰好是 row3（位移 3 行 = 120 px），固定头部不动，
///    命中测试也落到 row3（偏移参与命中）。
///
/// 变异验证：让 `on_scroll` 不写偏移 ⇒ 偏移/命中/位移断言红；让滚动走「重建 lane + 重编码」
/// （旧实现）⇒ 第 2 条红；把 `ScrollTable::physical_offset` 的符号反过来 ⇒ 第 4 条读到 row0/row5
/// 以外的东西（红）；让 paint pass 把滚动偏移烘焙进图元 ⇒ 第 3 条红（烘焙态顶部不再是 row0）。
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
    // 让「容器变成滚动容器」（lane 的滚动槽 0 → 1）在独立一帧里安定下来；滚动帧才只测偏移。
    app.invalidate_all_lanes();
    frame(&mut app);
    let before = read_pixels(&app);
    assert_pixel(&before, 240, 130, ROW_COLORS[0], "滚动前视口顶部是 row0");
    assert_pixel(&before, 240, 70, HEADER, "固定头部在视口外");

    // 滚轮向下 3 格：delta.y 为负 = 内容向上移（与 Win32 WM_MOUSEWHEEL 正方向一致）。
    let wheel = app.inject(InputEvent::scroll_lines(
        Point::new(240.0, 130.0),
        Point::new(0.0, -3.0),
        Modifiers::empty(),
    ));
    assert!(wheel.hit.is_some(), "滚轮必须命中滚动容器");

    // 离散滚轮走平滑动画：按可控时钟推进，直到偏移收敛；逐帧检查「无重编码」。
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

    // ③-a 不喂合成偏移：烘焙内容未变（= 不重编码的像素侧证据）。
    let baked = read_pixels(&app);
    assert_pixel(
        &baked,
        240,
        130,
        ROW_COLORS[0],
        "不喂偏移时视口顶部仍是烘焙的 row0（内容没有被重编码）",
    );

    // ③-b 喂合成偏移：内容必须恰好位移 3 行。
    //
    // **严格断言，三端同一份口径**：滚动偏移必须真的进 GPU。这里曾经有一条「自适应」分支
    // （`scroll_applied` 为假时按平台退回「烘焙态 row0」并输出 `DEFECT(Windows)` 证据行）——
    // 它让 Windows 的上传缺陷（`draw_ops` 每 op 只推 draw 块，`with_scroll` 写的 frame 块
    // 从未上传 ⇒ 偏移永远停在 `bind_frame` 的 0 上）在**修好与没修好时都绿**，等于没有守卫。
    // 现在任何后端把上传范围收窄回 draw 块，这条断言立刻红。
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

    // ③-c 偏移参与命中：同一屏幕点现在落到 row3。
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

// ── 场景 ④：拖拽（跨节点拖放 + 捕获）────────────────────────────────────────

/// 拖拽：跨节点拖放 + 指针捕获。
///
/// 断言链：按下不成立拖拽（R02-9 阈值）→ 越阈值后 `DragPhase::Start`、**指针捕获落在源卡片**
/// 上 → 悬停到目标区时高亮信号被写（像素读回目标区变 `ZONE_HOT`）→ 抬起派发 `End`，应用把
/// 模型从「源槽」移到「目标区」（两份视图切换可见性）⇒ 像素读回目标区出现卡片色、源槽空；
/// 再次拖拽后 `PointerCancel` ⇒ 派发 `Cancel`、捕获释放、高亮撤销、卡片留在原位。
///
/// 变异验证：不请求指针捕获 ⇒ 捕获断言红；`End` 不写落点 ⇒ 目标区像素断言红；`Cancel` 不撤
/// 高亮 ⇒ 取消后的目标区像素断言红（仍是 `ZONE_HOT`）；越过阈值的判定被去掉（按下即 Start）⇒
/// 「按下不成立拖拽」断言红。
fn scenario_drag_drop_with_pointer_capture() -> ScenarioReport {
    let mut app = app();
    let root = scene(&mut app);
    let source_rect = Rect::new(80.0, 140.0, 140.0, 100.0);
    let target_rect = Rect::new(380.0, 140.0, 160.0, 160.0);
    let location = app.app_mut().signal(0u8); // 0 = 源槽，1 = 目标区
    let hot = app.app_mut().signal(false);
    let phases = app.app_mut().signal(Vec::<DragPhase>::new());

    // 源槽（卡片搬走后露出的底色）。
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
    // 目标区（拖拽悬停时高亮）。
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
    // 同一模型项的**两份视图**：模型落点决定哪一份可见（跨节点拖放的常见实现）。
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
    // 最简应用：Start/Move 高亮落点，End 写落点，Cancel 只撤高亮。
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

    // ── PointerCancel 路径：取消不得改变落点、不得留下高亮 ──
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

// ── 场景 ⑤：焦点 / Tab + 模态焦点陷阱 ──────────────────────────────────────

/// 焦点 / Tab 遍历 + 模态对话框的焦点陷阱。
///
/// 断言链：Tab 序按 DFS 阅读顺序 A→B→C→A（Shift+Tab 反向）→ **焦点环像素**只在当前焦点
/// 节点上出现并随焦点移动 → 打开 `trap_focus` 模态后第一次输入把焦点校正进模态 → Tab 只在
/// 模态内回绕、`set_focus(背景节点)` 被拒绝 → 背景点击被**消费**（动作不触发）→ 模态面板
/// 像素存在 → Esc 关闭浮层、焦点还原给 opener、面板区域回到底色、opener 的焦点环重新出现。
///
/// 变异验证：去掉 `focus_scope_root` 的作用域过滤 ⇒ Tab 会跑出模态（断言红）；去掉
/// `outside_click_target` 的消费 ⇒ 背景动作计数 +1（断言红）；焦点环不随焦点重绘 ⇒ 环像素
/// 断言红；`restore_focus` 不生效 ⇒ 关闭后焦点断言红。
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
    // Tab 序（回绕）与 Shift+Tab。
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

    // 焦点环像素：只出现在当前焦点节点上。
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

    // ── 模态：trap_focus + restore_focus ──
    let modal = app
        .app_mut()
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(180.0, 150.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
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

    // 模态一开，焦点必须被拉进模态；Tab 只在这两个模态按钮之间循环（陷阱）。
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

    // 背景点击被消费（不得触发背景动作、不得抢焦点）。
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

    // Esc：关闭浮层 + 焦点还给 opener（A）+ 焦点环回来。
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

// ── 场景 ⑥：有焦点时的 a11y 树形状 ─────────────────────────────────────────

/// 有焦点时的 a11y 树形状。
///
/// 断言链：首帧是**全树** `TreeUpdate`（带 `TreeInfo`）且契约合法 → 角色集合含
/// `Button`/`TextInput`/`TextRun` → Tab 后焦点笔是**增量**、`TreeUpdate.focus` 与快照焦点一致、
/// 焦点节点与本笔一起送达 → 快照里 `focus_node()` 的 engine 句柄/角色/名字与树一致、物理边界
/// 等于布局盒 → 焦点移到输入框后角色/名字/文本度量（`character_lengths`）正确 →
/// 增量 vs 全量 oracle 一致。
///
/// 变异验证：`mark_a11y` 漏标焦点 ⇒ oracle 红；`to_tree_update` 不把焦点节点并入本笔 ⇒
/// AccessKit 契约失败；焦点边界渲成逻辑像素 ⇒ 边界断言红；角色推导写错 ⇒ 角色断言红。
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

    // ── 焦点：Tab 到按钮 ──
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

    // ── 焦点移到文本输入框：角色 / 名字 ──
    key(&mut app, KeyCode::Tab, Modifiers::empty());
    assert_eq!(app.app().focused(window), Some(input));
    frame(&mut app);
    app.assert_a11y_contract();
    let input_focus = a11y_snapshot(&app);
    assert_eq!(input_focus.focus_node().engine, input);
    assert_eq!(input_focus.focus_node().role, A11yRole::TextInput);
    assert_eq!(input_focus.focus_node().label_text(), Some("搜索"));

    // ── 文本度量：输入框内文本是 TextRun，其余文本是 Label；两者都带 grapheme/字节长度 ──
    // （增量笔只带变更节点，文本节点不在增量批里，因此用首帧全树快照断言。）
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

    // 增量 vs 全量 oracle（漏标 `mark_a11y` 的唯一防线）。
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

// ── 场景搭建原语（全部走公开 API）──────────────────────────────────────────

/// 冒烟窗口：640×480、DPI 1（像素坐标 = 逻辑坐标）。
fn app() -> TestApp {
    TestApp::with_options(WindowOptions {
        title: SmolStr::new_static("kytrix gui smoke"),
        size: WINDOW,
        scale: Scale::ONE,
        ..WindowOptions::default()
    })
}

/// 铺满窗口的底色（读回时的「空白」参照），返回根节点。
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

/// 挂一个节点（`NodeBuilder::build` 的薄封装）。
fn mount(app: &mut TestApp, parent: NodeId, builder: NodeBuilder) -> NodeId {
    let window = app.window_id();
    builder
        .build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            parent,
        )
        .expect("挂载节点")
}

/// 绝对定位的盒子（`Rect` = border box；出流、位置尺寸都由场景定）。
fn boxed(rect: Rect, background: [u8; 4]) -> NodeBuilder {
    NodeBuilder::boxed().layout_override(rect).paint(PaintSpec {
        background: color(background),
        ..PaintSpec::default()
    })
}

/// 可容纳子节点的绝对定位容器（`layout_override` 的节点是布局叶子，不能挂子树）。
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

/// 绝对定位 + 固定尺寸的 taffy 样式。
fn absolute_style(rect: Rect) -> Style {
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Absolute;
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(rect.size.width),
        height: taffy::style::Dimension::length(rect.size.height),
    };
    style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(rect.origin.x),
        top: taffy::style::LengthPercentageAuto::length(rect.origin.y),
        right: taffy::style::LengthPercentageAuto::auto(),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    style
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

/// 推进一帧并克隆记录（`TestApp::drain` 返回借用，断言里要跨帧比较）。
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

/// 读回当前窗口像素（真实 GPU 离屏；失败即 panic，不静默跳过）。
fn read_pixels(app: &TestApp) -> SnapshotImage {
    app.read_pixels().unwrap_or_else(|error| {
        panic!(
            "像素读回失败（本平台必须能离屏读回）：{}",
            pixels::describe_error(&error)
        )
    })
}

/// 读回当前窗口像素，并应用合成端滚动槽偏移表。
fn read_pixels_with_scroll(app: &TestApp, slots: &[ScrollSlot]) -> SnapshotImage {
    app.read_pixels_with_scroll(slots).unwrap_or_else(|error| {
        panic!(
            "带偏移的像素读回失败（本平台必须能离屏读回）：{}",
            pixels::describe_error(&error)
        )
    })
}

/// 断言某个像素等于期望色（容差内）。
fn assert_pixel(image: &SnapshotImage, x: u32, y: u32, expected: [u8; 4], what: &str) {
    let actual = pixels::pixel(image, x, y);
    let span = pixels::channel_span(actual, expected);
    assert!(
        span <= COLOR_TOLERANCE,
        "{what}：像素 ({x},{y}) = {actual:?}，期望 {expected:?}（最大通道差 {span} > {COLOR_TOLERANCE}）"
    );
}

/// 区域内「接近白色」的像素数（文本覆盖率；三通道 >200 = 预乘不透明亮像素）。
fn bright_pixels(image: &SnapshotImage, x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    pixels::count_pixels(image, x0, y0, x1, y1, |pixel| {
        pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200
    })
}

/// 区域内「接近焦点环颜色」的像素数（焦点环可见性断言）。
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

/// 场景统一用的焦点环（2 px 宽、外扩 2 px）。
fn focus_ring() -> FocusRing {
    FocusRing::new(2.0, 2.0, color(RING))
}

/// 派发一个按键按下事件（与平台泵同一条 `dispatch_input` 路径）。
fn key(app: &mut TestApp, code: KeyCode, modifiers: Modifiers) {
    app.inject(InputEvent::key_down(code, modifiers));
}

/// 当前 a11y 交付快照的克隆（避免借用 `TestApp` 造成后续写操作的借用冲突）。
fn a11y_snapshot(app: &TestApp) -> A11yTreeSnapshot {
    app.a11y().expect("本帧必须有一笔 a11y 交付").clone()
}
