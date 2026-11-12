//! demo 的场景：一块背板 + 三个彩色 quad + 一行文本；点击改色、Esc 退出
//! （`design/phase-09-win32-demo.md` §7.3）。
//!
//! 同一份建树代码被两个入口复用：真实窗口（`Application::run`）与 `--snapshot`（WARP 离屏）。
//! 只用引擎公开 API（`App` / `NodeBuilder` / `StylePatch` / `Signal`）。

use kytrix_gui::{
    app::{App, AppHandle},
    event::{Action, Event, InputEvent, Key, KeyState},
    interaction::ScrollConfig,
    math::{Rect, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::Color,
        style::{FocusRing, PaintSpec, Style, StylePatch},
    },
    window::WindowOptions,
};

/// 场景逻辑宽度。
pub(crate) const LOGICAL_WIDTH: f32 = 800.0;
/// 场景逻辑高度。
pub(crate) const LOGICAL_HEIGHT: f32 = 600.0;

/// 背景色（`#101418`）。
const BACKGROUND_RGBA: [u8; 4] = [0x10, 0x14, 0x18, 0xFF];
/// 焦点环颜色（`#4C9AFF`）。
const FOCUS_RGBA: [u8; 4] = [0x4C, 0x9A, 0xFF, 0xFF];
/// 文本颜色（`#E6E8EB`）。
const FOREGROUND_RGBA: [u8; 4] = [0xE6, 0xE8, 0xEB, 0xFF];
/// 三个色块的常态颜色（sRGB）。
const SWATCHES: [([u8; 4], &str); 3] = [
    ([0xE5, 0x48, 0x4D, 0xFF], "#E5484D"),
    ([0x30, 0xA4, 0x6C, 0xFF], "#30A46C"),
    ([0x00, 0x91, 0xFF, 0xFF], "#0091FF"),
];
/// 选中后的高亮颜色。
const HIGHLIGHTS: [[u8; 4]; 3] = [
    [0xF2, 0x55, 0x5A, 0xFF],
    [0x3C, 0xB1, 0x79, 0xFF],
    [0x2B, 0x9F, 0xFF, 0xFF],
];
/// 色块的逻辑矩形（x, y, w, h）。
const SLOTS: [(f32, f32, f32, f32); 3] = [
    (40.0, 40.0, 200.0, 120.0),
    (280.0, 40.0, 200.0, 120.0),
    (520.0, 40.0, 200.0, 120.0),
];
/// 文本的逻辑矩形。
const TEXT_RECT: (f32, f32, f32, f32) = (40.0, 200.0, 720.0, 40.0);

/// 场景句柄（建树之后交给事件钩子）。
#[derive(Clone)]
pub(crate) struct Palette {
    window: kytrix_gui::window::WindowId,
    /// 当前选中的色块下标（`None` = 都没选中）。
    active: Signal<u8>,
    /// 三个色块的节点（点击 → 动作 → `NodeId` 反查下标）。
    swatches: Vec<NodeId>,
}

/// 建窗 + 建树（真实窗口路径）。
pub(crate) fn open(app: &mut App) -> Palette {
    let window = app.create_window(WindowOptions {
        title: "Kytrix GUI · Phase 09".into(),
        size: Size::new(LOGICAL_WIDTH, LOGICAL_HEIGHT),
        scale: kytrix_gui::math::Scale::ONE,
    });
    build_into(app, window)
}

/// 在**已存在**的窗口上建树（`--snapshot` 用 `TestApp` 自己建的窗口，不能重复建窗）。
pub(crate) fn build_into(app: &mut App, window: kytrix_gui::window::WindowId) -> Palette {
    let root: NodeId = app
        .window(window)
        .map(|window| window.tree().root())
        .expect("the window was just created");
    let active = app.signal(u8::MAX);

    // 背板：整窗背景。
    let _ = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, LOGICAL_WIDTH, LOGICAL_HEIGHT))
        .paint(PaintSpec {
            background: Color::from_rgba8(BACKGROUND_RGBA),
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root);

    // 三个色块：`on_click` 声明动作（`active` 由 `install` 的 handler 写），`bind_style` 读 `active` 换高亮。
    let mut swatches: Vec<NodeId> = Vec::with_capacity(SWATCHES.len());
    for (index, (rgba, _hex)) in SWATCHES.iter().enumerate() {
        let (x, y, width, height) = SLOTS[index];
        let base = Color::from_rgba8(*rgba);
        let highlight = Color::from_rgba8(HIGHLIGHTS[index]);
        let selected = active;
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(x, y, width, height))
            .paint(PaintSpec {
                background: base,
                corner_radius: [12.0; 4],
                ..PaintSpec::default()
            })
            .on_click(Action::custom("swatch.select"))
            .focus_ring(FocusRing::new(2.0, 2.0, Color::from_rgba8(FOCUS_RGBA)))
            .bind_style(move |cx| {
                if cx.read(&selected) == index as u8 {
                    StylePatch {
                        background: Some(highlight),
                        ..StylePatch::default()
                    }
                } else {
                    StylePatch {
                        background: Some(base),
                        ..StylePatch::default()
                    }
                }
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("swatch");
        swatches.push(node);
    }

    // 滚动列表（P10）：自带 clip 的容器 + 20 行内容（引擎只对可视区出像素）。
    let mut scroll_style = Style::default();
    scroll_style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(200.0),
        height: taffy::style::Dimension::length(280.0),
    };
    scroll_style.inner.position = taffy::style::Position::Absolute;
    scroll_style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(560.0),
        top: taffy::style::LengthPercentageAuto::length(260.0),
        right: taffy::style::LengthPercentageAuto::auto(),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    scroll_style.clip = kytrix_gui::paint::style::Clip::Bounds;
    let scroll = NodeBuilder::boxed()
        .style(scroll_style)
        .paint(PaintSpec {
            background: Color::from_rgba8([0x1B, 0x20, 0x27, 0xFF]),
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("scroll container");
    for index in 0..20 {
        let shade = 0x20 + (index as u8 % 8) * 8;
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, index as f32 * 28.0, 200.0, 27.0))
            .paint(PaintSpec {
                background: Color::from_rgba8([shade, shade, shade + 8, 0xFF]),
                ..PaintSpec::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), scroll);
    }

    // 文本（P05 的文本 pass 由 `App::frame` 驱动；这里只给位置与样式）。
    let (x, y, width, height) = TEXT_RECT;
    let _ = NodeBuilder::text(
        "Kytrix GUI · Phase 09 — click a swatch, Esc to quit",
        kytrix_gui::text::TextStyle {
            size: 18.0,
            color: Color::from_rgba8(FOREGROUND_RGBA),
            ..kytrix_gui::text::TextStyle::default()
        },
    )
    .layout_override(Rect::new(x, y, width, height))
    .build(app.window_mut(window).expect("window").tree_mut(), root);

    if let Ok(handle) = app.make_scroll(window, scroll, ScrollConfig::default()) {
        let _ = app.set_scroll_content(&handle, Size::new(200.0, 20.0 * 28.0));
    }
    Palette {
        window,
        active,
        swatches,
    }
}

/// 安装交互：点击色块 → `Action` → handler（写状态 + 剪贴板），Esc 关窗，Tab 走焦点环。
pub(crate) fn install(app: &mut App, palette: Palette) {
    let _handle: AppHandle = app.handle();
    let swatches = palette.swatches.clone();
    let active = palette.active;
    app.on_action(Action::custom("swatch.select"), move |ctx| {
        let Some(index) = swatches.iter().position(|node| *node == ctx.node()) else {
            return;
        };
        ctx.write(&active, index as u8);
        // 剪贴板：把该色块的十六进制值写出去（失败不影响 UI）。
        let _ = ctx.app_mut().set_clipboard_text(SWATCHES[index].1);
    });
    app.on_frame(move |app| {
        for (window, event) in app.drain_events() {
            if window != palette.window {
                continue;
            }
            match event {
                Event::Input(InputEvent::Key {
                    key: Key::Escape,
                    state: KeyState::Down,
                    ..
                })
                | Event::CloseRequested => {
                    app.close_window(palette.window);
                }
                _ => {}
            }
        }
    });
    // 首帧前把焦点给第一个色块（焦点环可见）。
    if let Some(first) = palette.swatches.first() {
        let _ = app.set_focus(palette.window, Some(*first));
    }
}

#[cfg(test)]
mod tests {
    use kytrix_gui::{
        app::App,
        math::{Scale, Size},
        window::WindowOptions,
    };

    use super::{LOGICAL_HEIGHT, LOGICAL_WIDTH, SLOTS, build_into};

    #[test]
    fn scene_mounts_the_swatches_and_the_scroll_list() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let mut app = App::new(rt.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "scene".into(),
            size: Size::new(LOGICAL_WIDTH, LOGICAL_HEIGHT),
            scale: Scale::ONE,
        });

        let palette = build_into(&mut app, window);

        assert_eq!(palette.swatches.len(), SLOTS.len());
        let order = app.window(window).expect("window").tree().order().len();
        assert!(
            order > SLOTS.len() + 20,
            "背景 + 色块 + 滚动列表 20 行都在树里：{order}"
        );
    }
}
