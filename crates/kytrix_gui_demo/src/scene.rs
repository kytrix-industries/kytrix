//! demo 的主场景：焦点 / CJK / 输入法 / 无障碍 / 图片与滚动
//! （`design/phase-15-demo-showcase.md`）。
//!
//! 两个入口共用同一份建树代码：真实窗口（`Application::run`）与 `--snapshot`（WARP 离屏）。
//! 本模块是场景的**模块根**：常量、[`Scene`] 句柄与编排（建树 / 安装 / 帧钩子 / 截图种子状态）；
//! 每张卡片的内容在子模块里。
//!
//! 帧钩子（`Scene::tick`）做三件事：
//! 1. 把平台事件落到场景状态（IME / 输入 / 滑块方向键 / Esc 关窗）；
//! 2. 轮询 `App::focused` 写 `FocusView` 与「输入框是否聚焦」（引擎不派发焦点变化事件）；
//! 3. 输入框聚焦时把真实文本插入点交给 `App::set_ime_cursor_area`（P16 精确 IME 定位）。

mod a11y;
mod card;
mod cjk;
mod focus;
mod ime;
mod resources;
mod swatch;

use a11y::A11yState;
use focus::FocusView;
use ime::ImeField;
use kytrix_gui::{
    app::App,
    event::{Event, InputEvent, Key, KeyState},
    math::{Rect, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::Color,
        style::{FocusRing, PaintSpec},
    },
    text::TextStyle,
    window::{WindowId, WindowOptions},
};
use smol_str::SmolStr;

/// 场景逻辑宽度。
pub const LOGICAL_WIDTH: f32 = 960.0;
/// 场景逻辑高度。
pub const LOGICAL_HEIGHT: f32 = 740.0;

/// 窗口标题。
pub const TITLE: &str = "Kytrix GUI · 焦点 / 中文 / 输入法 / 无障碍";

/// 背景色（`#0B0E13`）。
const BG_RGBA: [u8; 4] = [0x0B, 0x0E, 0x13, 0xFF];
/// 卡片背景（`#141A22`）。
const CARD_RGBA: [u8; 4] = [0x14, 0x1A, 0x22, 0xFF];
/// 卡片边框（`#222C38`）。
const CARD_BORDER_RGBA: [u8; 4] = [0x22, 0x2C, 0x38, 0xFF];
/// 输入框背景（`#0D1117`）。
const FIELD_RGBA: [u8; 4] = [0x0D, 0x11, 0x17, 0xFF];
/// 输入框边框（`#2A3644`）。
const FIELD_BORDER_RGBA: [u8; 4] = [0x2A, 0x36, 0x44, 0xFF];
/// 正文色（`#E6E8EB`）。
const TEXT_RGBA: [u8; 4] = [0xE6, 0xE8, 0xEB, 0xFF];
/// 次要文字（`#93A4B5`）。
const MUTED_RGBA: [u8; 4] = [0x93, 0xA4, 0xB5, 0xFF];
/// 弱化文字 / 提示（`#6B7A8A`）。
const DIM_RGBA: [u8; 4] = [0x6B, 0x7A, 0x8A, 0xFF];
/// 强调色（`#4C9AFF`）。
const ACCENT_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
/// 焦点环（`#4C9AFF`）。
const RING_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
/// 未点亮的轨道（`#242E3A`）。
const RAIL_RGBA: [u8; 4] = [0x24, 0x2E, 0x3A, 0xFF];
/// 滚动容器背景（`#1B222B`）。
const TRACK_RGBA: [u8; 4] = [0x1B, 0x22, 0x2B, 0xFF];

/// 顶部标题 / 副标题。
const TITLE_RECT: Rect = Rect::new(24.0, 14.0, 800.0, 34.0);
/// 副标题矩形。
const SUBTITLE_RECT: Rect = Rect::new(24.0, 50.0, 912.0, 20.0);
/// 副标题文案。
const SUBTITLE: &str =
    "Tab / Shift+Tab 切换焦点 · Enter / Space 激活 · 点击色块 · 在输入框中输入中文 · Esc 退出";

/// 六张卡片（`design/phase-15-demo-showcase.md` §1）。
const FOCUS_CARD: Rect = Rect::new(24.0, 80.0, 580.0, 176.0);
/// 焦点状态面板。
const FOCUS_PANEL: Rect = Rect::new(624.0, 80.0, 312.0, 176.0);
/// CJK 卡片。
const CJK_CARD: Rect = Rect::new(24.0, 270.0, 580.0, 200.0);
/// 无障碍卡片。
const A11Y_CARD: Rect = Rect::new(624.0, 270.0, 312.0, 200.0);
/// 输入法卡片。
const IME_CARD: Rect = Rect::new(24.0, 484.0, 580.0, 242.0);
/// 图片与滚动卡片。
const RES_CARD: Rect = Rect::new(624.0, 484.0, 312.0, 242.0);

/// sRGB 字节 → 引擎颜色。
pub(crate) fn color(rgba: [u8; 4]) -> Color {
    Color::from_rgba8(rgba)
}

/// 常用文本样式（字号 / 字重 / 颜色）。
pub(crate) fn text_style(size: f32, weight: u16, rgba: [u8; 4]) -> TextStyle {
    TextStyle::new(size)
        .with_weight(weight)
        .with_color(color(rgba))
}

/// 焦点环：2px 宽、2px 外扩、强调色。
pub(crate) fn ring() -> FocusRing {
    FocusRing::new(2.0, 2.0, color(RING_RGBA))
}

/// 场景句柄（建树之后交给事件钩子；`Clone` 让帧钩子持有一份）。
#[derive(Clone)]
pub struct Scene {
    window: WindowId,
    /// 选中的色块下标（`u8::MAX` = 未选）。
    active: Signal<u8>,
    /// 焦点面板的值。
    focus: Signal<FocusView>,
    /// 输入框是否持有焦点（决定是否显示光标）。
    input_focused: Signal<bool>,
    /// 输入法输入框的状态。
    ime: Signal<ImeField>,
    /// 无障碍卡片的演示状态。
    a11y: Signal<A11yState>,
    /// 三个色块节点（顺序 = Tab 序）。
    swatches: Vec<NodeId>,
    /// Tab 序（节点 → 可访问名），与引擎 `focus::next_focusable` 的 DFS 先序一致。
    focus_targets: Vec<(NodeId, &'static str)>,
    /// 输入框节点。
    ime_field: NodeId,
    /// 输入框正文文本节点（IME caret 的几何来源）。
    ime_text: NodeId,
    /// 缩放滑块节点。
    slider: NodeId,
}

impl Scene {
    /// 建窗 + 建树（真实窗口路径）。
    pub fn open(app: &mut App) -> Self {
        let window = app.create_window(WindowOptions {
            title: TITLE.into(),
            size: Size::new(LOGICAL_WIDTH, LOGICAL_HEIGHT),
            scale: kytrix_gui::math::Scale::ONE,
        });
        Self::build_into(app, window)
    }

    /// 在**已存在**的窗口上建树（`--snapshot` 用 `TestApp` 自己建的窗口，不能重复建窗）。
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

        // 背景（整窗）+ 标题：先画、垫在最底层。
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, LOGICAL_WIDTH, LOGICAL_HEIGHT))
            .paint(PaintSpec {
                background: color(BG_RGBA),
                ..PaintSpec::default()
            })
            .build(card::tree(app, window), root);
        let _ = card::label(
            app,
            window,
            root,
            TITLE_RECT,
            "Kytrix GUI · 焦点 / 中文 / 输入法 / 无障碍",
            text_style(20.0, 700, TEXT_RGBA),
        );
        let _ = card::label(
            app,
            window,
            root,
            SUBTITLE_RECT,
            SUBTITLE,
            text_style(12.5, 400, MUTED_RGBA),
        );

        // 挂载顺序 = Tab 序：① 色块 → ④ 单选/滑块 → ⑤ 输入框；其余卡片没有可聚焦节点。
        let focus_card = card::card(app, window, root, FOCUS_CARD, "① 焦点与动作");
        let swatches = swatch::build(app, window, focus_card, active);

        let a11y_card = card::card(app, window, root, A11Y_CARD, "④ 无障碍（AccessKit）");
        let controls = a11y::build(app, window, a11y_card, a11y);

        let ime_card = card::card(app, window, root, IME_CARD, "⑤ 输入法（IME）");
        let (ime_field, ime_text) = ime::build(app, window, ime_card, ime, input_focused);

        let resources_card = card::card(app, window, root, RES_CARD, "⑥ 图片与滚动");
        resources::build(app, window, resources_card, RES_CARD.origin);

        let cjk_card = card::card(app, window, root, CJK_CARD, "③ 中文排版（CJK）");
        cjk::build(app, window, cjk_card);

        let focus_panel = card::card(app, window, root, FOCUS_PANEL, "② 焦点状态");
        focus::build(app, window, focus_panel, focus);

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
        }
    }

    /// 安装交互：动作处理器 + 帧钩子 + 初始焦点（色块 1，焦点环可见）。
    pub fn install(app: &mut App, scene: Self) {
        swatch::install(app, &scene);
        a11y::install(app, &scene);
        if let Some((first, _)) = scene.focus_targets.first().copied() {
            let _ = app.set_focus(scene.window, Some(first));
            let _ = app.write(
                &scene.focus,
                FocusView::resolve(Some(first), &scene.focus_targets),
            );
            let _ = app.write(&scene.input_focused, first == scene.ime_field);
        }
        let mut hook = scene;
        app.on_frame(move |app| hook.tick(app));
    }

    /// 离屏截图用的种子状态：选中色块 2、单选 B、滑块 70%、输入框有已提交文本与组合串，
    /// 焦点落在输入框（光标可见）。只用于 `--snapshot`，真实窗口从空状态开始。
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

    /// 帧钩子：事件 → 场景状态；然后同步焦点视图。
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

    /// 同步焦点面板与「输入框是否聚焦」。
    fn sync_focus(&mut self, app: &mut App) {
        if app.window(self.window).is_none() {
            return;
        }
        let focused = app.focused(self.window);
        let on_input = focused == Some(self.ime_field);
        // 焦点离开输入框：组合串不能在字段里永久残留（Tab 走开再按 Esc 时会被误判成「组合中」）。
        if !on_input && app.read(&self.ime).composing() {
            self.update_ime(app, ImeField::cancel_composition);
        }
        // P16：把真实文本插入点（不含视觉 `|`）交给引擎；引擎负责 caret 几何与平台 IME 下发。
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

    /// 一条平台事件落到场景状态（只处理与 demo 相关的事件）。
    fn handle_event(&mut self, app: &mut App, event: &Event) {
        let focused = app.focused(self.window);
        match event {
            Event::Input(InputEvent::Key {
                key: Key::Escape,
                state: KeyState::Down,
                ..
            }) => {
                // 组合中 Esc 是「取消组合」（平台已消费），不关窗。
                if !app.read(&self.ime).composing() {
                    app.close_window(self.window);
                }
            }
            Event::CloseRequested => {
                app.close_window(self.window);
            }
            Event::FocusChanged { focused: false } => {
                // 窗口失焦：IME 组合随焦点结束，别让 preedit 留在字段里。
                self.update_ime(app, ImeField::cancel_composition);
            }
            Event::Ime(ime_event) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, |field| field.apply(ime_event));
                }
            }
            Event::Input(InputEvent::Text { text }) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, |field| field.insert(text));
                }
            }
            Event::Input(InputEvent::Key {
                key: Key::Backspace,
                state: KeyState::Down,
                ..
            }) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, ImeField::backspace);
                }
            }
            Event::Input(InputEvent::Key {
                key: Key::Enter,
                state: KeyState::Down,
                ..
            }) => {
                if focused == Some(self.ime_field) {
                    self.update_ime(app, ImeField::submit);
                }
            }
            Event::Input(InputEvent::Key {
                key: key @ (Key::ArrowLeft | Key::ArrowRight),
                state: KeyState::Down,
                modifiers,
                ..
            }) if modifiers.is_empty() && focused == Some(self.slider) => {
                self.step_scale(app, if *key == Key::ArrowLeft { -1 } else { 1 });
            }
            _ => {}
        }
    }

    /// 读—改—写输入框状态（`App::write` 对等值短路，不产生多余失效）。
    fn update_ime(&self, app: &mut App, apply: impl FnOnce(&mut ImeField)) {
        let mut field = app.read(&self.ime);
        apply(&mut field);
        app.write(&self.ime, field);
    }

    /// 方向键调整滑块（钳制在 10%–100%）。
    fn step_scale(&self, app: &mut App, step: i8) {
        let mut state = app.read(&self.a11y);
        state.step_scale(step);
        app.write(&self.a11y, state);
    }
}

/// 把一条合成事件交给场景（测试用：平台事件队列只能由引擎内部投递）。
pub fn dispatch_event(scene: &mut Scene, app: &mut App, event: &Event) {
    scene.handle_event(app, event);
}

/// 跑一次场景钩子（测试用：`TestApp::drain` 不经过平台泵的帧钩子）。
pub fn tick(scene: &mut Scene, app: &mut App) {
    scene.tick(app);
}

/// 输入框当前已提交的文本（测试 / 诊断用）。
#[must_use]
pub fn ime_text(scene: &Scene, app: &App) -> SmolStr {
    SmolStr::new(app.read(&scene.ime).committed())
}
