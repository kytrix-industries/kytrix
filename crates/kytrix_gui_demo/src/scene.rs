//! demo 的场景：一块背板 + 三个彩色 quad + 一行文本；点击改色、Esc 退出
//! （`design/phase-09-win32-demo.md` §7.3）。
//!
//! 同一份建树代码被两个入口复用：真实窗口（`Application::run`）与 `--snapshot`（WARP 离屏）。
//! 只用引擎公开 API（`App` / `NodeBuilder` / `StylePatch` / `Signal`）。

use kytrix_gui::{
    app::{App, AppHandle},
    event::{Event, InputEvent, Key, KeyState, PointerButton},
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        color::Color,
        style::{PaintSpec, StylePatch},
    },
    window::WindowOptions,
};

/// 场景逻辑宽度。
pub(crate) const LOGICAL_WIDTH: f32 = 800.0;
/// 场景逻辑高度。
pub(crate) const LOGICAL_HEIGHT: f32 = 600.0;

/// 背景色（`#101418`）。
const BACKGROUND_RGBA: [u8; 4] = [0x10, 0x14, 0x18, 0xFF];
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
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    window: kytrix_gui::window::WindowId,
    /// 当前选中的色块下标（`None` = 都没选中）。
    active: Signal<u8>,
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

    // 三个色块：`bind_style` 读 `active`，选中时换高亮色。
    for (index, (rgba, _hex)) in SWATCHES.iter().enumerate() {
        let (x, y, width, height) = SLOTS[index];
        let base = Color::from_rgba8(*rgba);
        let highlight = Color::from_rgba8(HIGHLIGHTS[index]);
        let selected = active;
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(x, y, width, height))
            .paint(PaintSpec {
                background: base,
                corner_radius: [12.0; 4],
                ..PaintSpec::default()
            })
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
            .build(app.window_mut(window).expect("window").tree_mut(), root);
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

    Palette { window, active }
}

/// 安装每轮事件循环的钩子：点击改色、Esc 关窗、点击同时把色值写进剪贴板。
pub(crate) fn install(app: &mut App, palette: Palette) {
    let _handle: AppHandle = app.handle();
    app.on_frame(move |app| {
        for (window, event) in app.drain_events() {
            if window != palette.window {
                continue;
            }
            match event {
                Event::Input(InputEvent::PointerDown {
                    position,
                    button: PointerButton::Primary,
                }) => {
                    if let Some(index) = slot_at(position) {
                        app.write(&palette.active, index as u8);
                        // 剪贴板：把该色块的十六进制值写出去（失败不影响 UI）。
                        let _ = app.set_clipboard_text(SWATCHES[index].1);
                    }
                }
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
}

/// 命中的色块下标（逻辑坐标；色块矩形是硬编码的布局常量）。
fn slot_at(position: Point) -> Option<usize> {
    SLOTS.iter().position(|(x, y, width, height)| {
        position.x >= *x
            && position.x <= *x + *width
            && position.y >= *y
            && position.y <= *y + *height
    })
}

#[cfg(test)]
mod tests {
    use kytrix_gui::math::Point;

    use super::slot_at;

    #[test]
    fn hit_testing_uses_the_swatch_rectangles() {
        assert_eq!(slot_at(Point::new(140.0, 100.0)), Some(0));
        assert_eq!(slot_at(Point::new(380.0, 100.0)), Some(1));
        assert_eq!(slot_at(Point::new(620.0, 100.0)), Some(2));
        assert_eq!(slot_at(Point::new(10.0, 10.0)), None);
        assert_eq!(slot_at(Point::new(140.0, 400.0)), None);
    }
}
