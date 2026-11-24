use ui_core::math::px;
mod a11y;
mod card;
#[cfg(target_os = "linux")]
mod chrome;
mod cjk;
mod focus;
mod ime;
mod resources;
mod swatch;
#[cfg(all(test, feature = "test-support"))]
mod test_support;

use a11y::A11yState;
use focus::FocusView;
use ime::ImeField;
#[cfg(all(test, feature = "test-support"))]
use smol_str::SmolStr;
use ui_core::{
    app::App,
    event::{Event, InputEvent, KeyCode, KeyState},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::Color,
        style::{FocusRing, PaintSpec, Style},
        style_builder::StyleBuilder,
    },
    text::TextStyle,
    window::{WindowId, WindowOptions},
};

pub const LOGICAL_WIDTH: LogicalPx = px(960.0);
pub const LOGICAL_HEIGHT: LogicalPx = px(740.0);

#[cfg(target_os = "linux")]
const WINDOW_TOP_INSET: LogicalPx = chrome::TITLEBAR_HEIGHT;
#[cfg(target_os = "macos")]
const WINDOW_TOP_INSET: LogicalPx = px(38.0);
#[cfg(windows)]
const WINDOW_TOP_INSET: LogicalPx = px(0.0);

#[cfg(target_os = "macos")]
const HEADER_LEFT_INSET: LogicalPx = px(76.0);
#[cfg(not(target_os = "macos"))]
const HEADER_LEFT_INSET: LogicalPx = px(0.0);

const ROOT_TOP_PADDING: LogicalPx = px(14.0);
const ROOT_SIDE_PADDING: LogicalPx = px(24.0);
const ROOT_BOTTOM_PADDING: LogicalPx = px(14.0);
const HEADER_HEIGHT: LogicalPx = px(56.0);
const CONTENT_MARGIN_TOP: LogicalPx = px(10.0);
const CONTENT_WIDTH: LogicalPx = px(912.0);
const COLUMN_GAP: LogicalPx = px(20.0);
const ROW_GAP: LogicalPx = px(14.0);

const CONTENT_TOP: LogicalPx = px(WINDOW_TOP_INSET.value()
    + ROOT_TOP_PADDING.value()
    + HEADER_HEIGHT.value()
    + CONTENT_MARGIN_TOP.value());

#[must_use]
pub fn window_size() -> LogicalSize {
    LogicalSize::new(LOGICAL_WIDTH, LOGICAL_HEIGHT + WINDOW_TOP_INSET)
}

pub const TITLE: &str = "Kytrix GUI · 焦点 / 中文 / 输入法 / 无障碍";

const BG_RGBA: [u8; 4] = [0x0B, 0x0E, 0x13, 0xFF];
const CARD_RGBA: [u8; 4] = [0x14, 0x1A, 0x22, 0xFF];
const CARD_BORDER_RGBA: [u8; 4] = [0x22, 0x2C, 0x38, 0xFF];
const FIELD_RGBA: [u8; 4] = [0x0D, 0x11, 0x17, 0xFF];
const FIELD_BORDER_RGBA: [u8; 4] = [0x2A, 0x36, 0x44, 0xFF];
const TEXT_RGBA: [u8; 4] = [0xE6, 0xE8, 0xEB, 0xFF];
const MUTED_RGBA: [u8; 4] = [0x93, 0xA4, 0xB5, 0xFF];
const DIM_RGBA: [u8; 4] = [0x6B, 0x7A, 0x8A, 0xFF];
const ACCENT_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
const RING_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
const RAIL_RGBA: [u8; 4] = [0x24, 0x2E, 0x3A, 0xFF];
const TRACK_RGBA: [u8; 4] = [0x1B, 0x22, 0x2B, 0xFF];

const TITLE_HEIGHT: LogicalPx = px(34.0);
const SUBTITLE_HEIGHT: LogicalPx = px(20.0);
const HEADER_GAP: LogicalPx = px(2.0);
#[cfg(feature = "hud")]
const SUBTITLE: &str = "Tab / Shift+Tab 切换焦点 · Enter / Space 激活 · 点击色块 · 在输入框中输入中文 · F12 调试 HUD · Esc 退出";
#[cfg(not(feature = "hud"))]
const SUBTITLE: &str =
    "Tab / Shift+Tab 切换焦点 · Enter / Space 激活 · 点击色块 · 在输入框中输入中文 · Esc 退出";

pub(crate) const FOCUS_CARD: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(px(24.0), CONTENT_TOP),
    LogicalSize::new(px(580.0), px(176.0)),
);
pub(crate) const FOCUS_PANEL: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(px(624.0), CONTENT_TOP),
    LogicalSize::new(px(312.0), px(176.0)),
);
pub(crate) const CJK_CARD: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(
        px(24.0),
        px(CONTENT_TOP.value() + FOCUS_CARD.size.height.value() + ROW_GAP.value()),
    ),
    LogicalSize::new(px(580.0), px(200.0)),
);
pub(crate) const A11Y_CARD: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(
        px(624.0),
        px(CONTENT_TOP.value() + FOCUS_CARD.size.height.value() + ROW_GAP.value()),
    ),
    LogicalSize::new(px(312.0), px(200.0)),
);
pub(crate) const IME_CARD: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(
        px(24.0),
        px(CONTENT_TOP.value()
            + FOCUS_CARD.size.height.value()
            + ROW_GAP.value()
            + CJK_CARD.size.height.value()
            + ROW_GAP.value()),
    ),
    LogicalSize::new(px(580.0), px(242.0)),
);
pub(crate) const RES_CARD: LogicalRect = LogicalRect::from_origin_size(
    LogicalPoint::new(
        px(624.0),
        px(CONTENT_TOP.value()
            + FOCUS_CARD.size.height.value()
            + ROW_GAP.value()
            + CJK_CARD.size.height.value()
            + ROW_GAP.value()),
    ),
    LogicalSize::new(px(312.0), px(242.0)),
);

pub(crate) fn color(rgba: [u8; 4]) -> Color {
    Color::from_rgba8(rgba)
}

pub(crate) fn text_style(size: LogicalPx, weight: u16, rgba: [u8; 4]) -> TextStyle {
    TextStyle::new(size)
        .with_weight(weight)
        .with_color(color(rgba))
}

pub(crate) fn ring() -> FocusRing {
    FocusRing::new(px(2.0), px(2.0), color(RING_RGBA))
}

fn root_style() -> Style {
    let mut style = StyleBuilder::new()
        .size_full()
        .flex_col()
        .padding_sides(
            WINDOW_TOP_INSET + ROOT_TOP_PADDING,
            ROOT_SIDE_PADDING,
            ROOT_BOTTOM_PADDING,
            ROOT_SIDE_PADDING,
        )
        .build();
    style.align_items(taffy::style::AlignItems::START);
    style
}

fn background_style() -> Style {
    StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size_full()
        .build()
}

fn header_style() -> Style {
    let mut style = StyleBuilder::new()
        .width_full()
        .height(HEADER_HEIGHT)
        .flex_col()
        .build();
    style
        .align_items(taffy::style::AlignItems::START)
        .gap_height(HEADER_GAP)
        .padding_sides(px(0.0), px(0.0), px(0.0), HEADER_LEFT_INSET);
    style
}

fn text_line_style(height: LogicalPx) -> Style {
    StyleBuilder::new().width_full().height(height).build()
}

fn content_style() -> Style {
    let mut style = StyleBuilder::new().width(CONTENT_WIDTH).flex_col().build();
    style
        .align_items(taffy::style::AlignItems::START)
        .gap_height(ROW_GAP)
        .margin_sides(CONTENT_MARGIN_TOP, px(0.0), px(0.0), px(0.0));
    style
}

fn row_style(height: LogicalPx) -> Style {
    let mut style = StyleBuilder::new()
        .width_full()
        .height(height)
        .flex_row()
        .build();
    style
        .align_items(taffy::style::AlignItems::START)
        .gap_width(COLUMN_GAP);
    style
}

#[derive(Clone)]
pub struct Scene {
    window: WindowId,
    active: Signal<u8>,
    focus: Signal<FocusView>,
    input_focused: Signal<bool>,
    ime: Signal<ImeField>,
    a11y: Signal<A11yState>,
    swatches: Vec<NodeId>,
    focus_targets: Vec<(NodeId, &'static str)>,
    ime_field: NodeId,
    ime_text: NodeId,
    slider: NodeId,
    #[cfg(target_os = "linux")]
    chrome: chrome::Chrome,
}

impl Scene {
    pub fn open(app: &mut App) -> Self {
        let window = app.create_window(WindowOptions {
            title: TITLE.into(),
            size: window_size(),
            background: color(BG_RGBA),
            ..WindowOptions::default()
        });
        Self::build_into(app, window)
    }

    pub fn build_into(app: &mut App, window: WindowId) -> Self {
        let root = app
            .window(window)
            .map(|target| target.tree().root())
            .expect("场景窗口必须存在");

        let active = app.signal(u8::MAX);
        let focus = app.signal(FocusView::default());
        let input_focused = app.signal(false);
        let ime = app.signal(ImeField::default());
        let a11y = app.signal(A11yState::default());

        card::tree(app, window)
            .set_style(root, root_style())
            .expect("根样式写入");
        let _background = NodeBuilder::boxed()
            .style(background_style())
            .paint(PaintSpec {
                background: color(BG_RGBA),
                ..PaintSpec::default()
            })
            .id("demo.background")
            .build(card::tree(app, window), root)
            .expect("背景挂载");

        let header = card::container(app, window, root, header_style());
        let _ = NodeBuilder::text(TITLE, text_style(px(20.0), 700, TEXT_RGBA))
            .style(text_line_style(TITLE_HEIGHT))
            .build(card::tree(app, window), header)
            .expect("标题挂载");
        let _ = NodeBuilder::text(SUBTITLE, text_style(px(12.5), 400, MUTED_RGBA))
            .style(text_line_style(SUBTITLE_HEIGHT))
            .build(card::tree(app, window), header)
            .expect("副标题挂载");

        let content = card::container(app, window, root, content_style());
        let row_one = card::container(app, window, content, row_style(FOCUS_CARD.size.height));
        let row_two = card::container(app, window, content, row_style(CJK_CARD.size.height));
        let row_three = card::container(app, window, content, row_style(IME_CARD.size.height));

        let focus_card = card::card(
            app,
            window,
            row_one,
            FOCUS_CARD.size,
            "① 焦点与动作",
            "demo.card.focus",
        );
        let swatches = swatch::build(app, window, focus_card, active);

        let focus_panel = card::card(
            app,
            window,
            row_one,
            FOCUS_PANEL.size,
            "② 焦点状态",
            "demo.card.focus-panel",
        );
        focus::build(app, window, focus_panel, focus);

        let cjk_card = card::card(
            app,
            window,
            row_two,
            CJK_CARD.size,
            "③ 中文排版（CJK）",
            "demo.card.cjk",
        );
        cjk::build(app, window, cjk_card);

        let a11y_card = card::card(
            app,
            window,
            row_two,
            A11Y_CARD.size,
            "④ 无障碍（AccessKit）",
            "demo.card.a11y",
        );
        let controls = a11y::build(app, window, a11y_card, a11y);

        let ime_card = card::card(
            app,
            window,
            row_three,
            IME_CARD.size,
            "⑤ 输入法（IME）",
            "demo.card.ime",
        );
        let (ime_field, ime_text) = ime::build(app, window, ime_card, ime, input_focused);

        let resources_card = card::card(
            app,
            window,
            row_three,
            RES_CARD.size,
            "⑥ 图片与滚动",
            "demo.card.resources",
        );
        resources::build(app, window, resources_card, RES_CARD.origin);

        #[cfg(target_os = "linux")]
        let chrome = chrome::build(app, window, root);

        let mut focus_targets: Vec<(NodeId, &'static str)> = Vec::with_capacity(7);
        focus_targets.extend(
            swatches
                .iter()
                .zip(swatch::LABELS)
                .map(|(node, label)| (*node, label)),
        );
        focus_targets.push((controls.radios[0], "选项 A"));
        focus_targets.push((controls.radios[1], "选项 B"));
        focus_targets.push((controls.slider, "缩放滑块"));
        focus_targets.push((ime_field, "输入框"));

        Self {
            window,
            active,
            focus,
            input_focused,
            ime,
            a11y,
            swatches,
            focus_targets,
            ime_field,
            ime_text,
            slider: controls.slider,
            #[cfg(target_os = "linux")]
            chrome,
        }
    }

    pub fn install(app: &mut App, scene: Self) {
        swatch::install(app, &scene);
        a11y::install(app, &scene);
        #[cfg(target_os = "linux")]
        chrome::install(app, &scene);
        if let Some((first, _)) = scene.focus_targets.first().copied() {
            let _ = app.set_focus(scene.window, Some(first));
            let _ = app.write(
                &scene.focus,
                FocusView::resolve(Some(first), &scene.focus_targets),
            );
            let _ = app.write(&scene.input_focused, first == scene.ime_field);
        }
        #[cfg(feature = "hud")]
        {
            let _ = app.set_debug_hud(scene.window, true);
        }
        let mut hook = scene;
        app.on_frame(move |app| hook.tick(app));
    }

    pub fn preview(&self, app: &mut App) {
        let _ = app.write(&self.active, 1);
        app.write(&self.ime, ImeField::sample());
        app.write(&self.a11y, A11yState::sample());
        let _ = app.set_focus(self.window, Some(self.ime_field));
        app.write(&self.input_focused, true);
        app.write(
            &self.focus,
            FocusView::resolve(Some(self.ime_field), &self.focus_targets),
        );
        let caret = app.read(&self.ime).display(false).len();
        let _ = app.set_ime_cursor_area(self.window, self.ime_text, caret);
    }

    fn tick(&mut self, app: &mut App) {
        if app.window(self.window).is_none() {
            return;
        }
        for (window, event) in app.drain_events() {
            if window == self.window {
                self.handle_event(app, &event);
            }
        }
        self.sync_focus(app);
    }

    fn sync_focus(&mut self, app: &mut App) {
        if app.window(self.window).is_none() {
            return;
        }
        let focused = app.focused(self.window);
        let on_input = focused == Some(self.ime_field);
        if !on_input && app.read(&self.ime).composing() {
            self.update_ime(app, ImeField::cancel_composition);
        }
        if on_input {
            let caret = app.read(&self.ime).display(false).len();
            let _ = app.set_ime_cursor_area(self.window, self.ime_text, caret);
        } else {
            app.clear_ime_cursor_area(self.window);
        }
        app.write(
            &self.focus,
            FocusView::resolve(focused, &self.focus_targets),
        );
        app.write(&self.input_focused, on_input);
    }

    fn handle_event(&mut self, app: &mut App, event: &Event) {
        let focused = app.focused(self.window);
        match event {
            #[cfg(feature = "hud")]
            Event::Input(InputEvent::Key {
                code: KeyCode::F12,
                state: KeyState::Down,
                modifiers,
                ..
            }) if modifiers.is_empty() => {
                let enabled = !app.debug_hud_enabled(self.window);
                let _ = app.set_debug_hud(self.window, enabled);
            }
            Event::Input(InputEvent::Key {
                code: KeyCode::Escape,
                state: KeyState::Down,
                ..
            }) => {
                if !app.read(&self.ime).composing() {
                    app.close_window(self.window);
                }
            }
            Event::CloseRequested => {
                app.close_window(self.window);
            }
            Event::FocusChanged { focused: false } => {
                self.update_ime(app, ImeField::cancel_composition);
            }
            Event::Ime(ime_event) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, |field| field.apply(ime_event));
                }
            }
            Event::Input(InputEvent::Text { text, .. }) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, |field| field.insert(text));
                }
            }
            Event::Input(InputEvent::Key {
                code: KeyCode::Backspace,
                state: KeyState::Down,
                ..
            }) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, ImeField::backspace);
                }
            }
            Event::Input(InputEvent::Key {
                code: KeyCode::Enter,
                state: KeyState::Down,
                ..
            }) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, ImeField::submit);
                }
            }
            Event::Input(InputEvent::Key {
                code: key @ (KeyCode::ArrowLeft | KeyCode::ArrowRight),
                state: KeyState::Down,
                modifiers,
                ..
            }) if modifiers.is_empty() && focused == Some(self.slider) => {
                self.step_scale(app, if *key == KeyCode::ArrowLeft { -1 } else { 1 });
            }
            _ => {}
        }
    }

    fn update_ime(&self, app: &mut App, apply: impl FnOnce(&mut ImeField)) {
        let mut field = (*app.read(&self.ime)).clone();
        apply(&mut field);
        app.write(&self.ime, field);
    }

    fn step_scale(&self, app: &mut App, step: i8) {
        let mut state = (*app.read(&self.a11y)).clone();
        state.step_scale(step);
        app.write(&self.a11y, state);
    }
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn dispatch_event(scene: &mut Scene, app: &mut App, event: &Event) {
    scene.handle_event(app, event);
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn tick(scene: &mut Scene, app: &mut App) {
    scene.tick(app);
}

#[cfg(all(test, feature = "test-support"))]
#[must_use]
pub(crate) fn ime_text(scene: &Scene, app: &App) -> SmolStr {
    SmolStr::new(app.read(&scene.ime).committed())
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use ui_core::{
        a11y::A11yRole,
        math::{LogicalRect, LogicalSize, px},
        test_support::{probe::Selector, record::PrimitiveKind},
        text::TextDiagnostic,
    };

    use super::{
        FOCUS_CARD, RES_CARD, ime_text,
        test_support::{frame, setup},
        window_size,
    };

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

        let canvases: Vec<_> = frame
            .primitives
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Canvas)
            .map(|primitive| primitive.bounds)
            .collect();
        assert_eq!(canvases.len(), 2, "两个图标都是 Canvas 节点");
        for bounds in canvases {
            assert!(
                bounds.origin.x >= RES_CARD.origin.x && bounds.origin.y >= RES_CARD.origin.y,
                "图标必须画在 ⑥ 卡片内：{bounds:?}"
            );
            assert!(
                bounds.max().y <= RES_CARD.max().y,
                "图标不得溢出卡片底边：{bounds:?}"
            );
        }

        let probe = test.probe();
        let background = probe
            .by_selector(Selector::Id("demo.background"))
            .expect("背景节点");
        assert_eq!(
            probe.bounds(background),
            Some(LogicalRect::new(
                px(0.0),
                px(0.0),
                window_size().width,
                window_size().height
            )),
            "背景必须铺满整个窗口"
        );
        for (id, expected) in [
            ("demo.card.focus", FOCUS_CARD),
            ("demo.card.cjk", super::CJK_CARD),
            ("demo.card.ime", super::IME_CARD),
        ] {
            let node = probe.by_selector(Selector::Id(id)).expect(id);
            assert_eq!(probe.bounds(node), Some(expected), "{id} 的 flex 落点");
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
    fn resizing_window_fills_background_and_keeps_content_top_left() {
        let (mut test, window, _scene) = setup();
        frame(&mut test);

        let resized = LogicalSize::new(window_size().width + 240.0, window_size().height + 160.0);
        test.app_mut()
            .window_mut(window)
            .expect("窗口存在")
            .set_size(resized);
        frame(&mut test);

        assert!(
            test.frame_record().rendered,
            "resize 后必须重新提交一帧（不能只保留旧画面）"
        );
        let probe = test.probe();
        let background = probe
            .by_selector(Selector::Id("demo.background"))
            .expect("背景节点");
        assert_eq!(
            probe.bounds(background),
            Some(LogicalRect::new(
                px(0.0),
                px(0.0),
                resized.width,
                resized.height
            )),
            "窗口变大后背景必须继续铺满"
        );
        for (id, expected) in [
            ("demo.card.focus", FOCUS_CARD),
            ("demo.card.ime", super::IME_CARD),
            ("demo.card.resources", RES_CARD),
        ] {
            let node = probe.by_selector(Selector::Id(id)).expect(id);
            assert_eq!(
                probe.bounds(node),
                Some(expected),
                "flex 内容必须保持左上锚定（{id}）"
            );
        }
    }

    #[test]
    fn tab_order_visits_the_documented_seven_labels() {
        let (mut test, window, _scene) = setup();
        test.enable_a11y_capture();
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
                "缩放比例",
                "输入法测试输入框",
            ],
            "Tab 序必须与设计文档 §1.1 一致"
        );

        let wrapped = test.app_mut().focus_next(window).expect("回绕");
        assert_eq!(wrapped, visited[0], "第 8 次 Tab 必须回绕到第一个");
    }

    #[cfg(feature = "hud")]
    #[test]
    fn f12_toggles_the_hud_and_snapshot_build_has_none() {
        use ui_core::event::{Event, KeyCode};

        use super::{
            dispatch_event,
            test_support::{press, setup_installed},
        };

        let (mut test, window, _scene) = setup();
        assert!(
            !test.app().debug_hud_enabled(window),
            "snapshot 路径（build_into）不得安装 HUD"
        );
        frame(&mut test);

        let (mut test, window, mut scene) = setup_installed();
        assert!(test.app().debug_hud_enabled(window), "真实路径默认开启 HUD");
        frame(&mut test);

        dispatch_event(
            &mut scene,
            test.app_mut(),
            &Event::Input(press(KeyCode::F12)),
        );
        frame(&mut test);
        assert!(!test.app().debug_hud_enabled(window), "F12 必须隐藏 HUD");

        dispatch_event(
            &mut scene,
            test.app_mut(),
            &Event::Input(press(KeyCode::F12)),
        );
        frame(&mut test);
        assert!(
            test.app().debug_hud_enabled(window),
            "第二次 F12 必须重新显示 HUD"
        );
    }

    #[test]
    fn preview_seeds_the_snapshot_state() {
        let (mut test, window, scene) = setup();
        test.enable_a11y_capture();
        frame(&mut test);
        scene.preview(test.app_mut());
        frame(&mut test);

        assert_eq!(ime_text(&scene, test.app()).as_str(), "你好，Kytrix");

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
}
