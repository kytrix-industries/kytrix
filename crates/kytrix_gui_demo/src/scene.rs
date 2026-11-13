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
        canvas::Canvas,
        color::Color,
        primitive::Image,
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

/// 内嵌的 16×16 PNG（四个 8×8 象限，alpha = 255 / 170 / 85 / 0）。
///
/// P13 的 `ResourceCache` 生产消费者：解码 → 线性预乘 → sRGB 编码 → 图片图集 tile → 页上传 → 采样，
/// 一条路径全部覆盖（`--features snapshot` 时像素可看）。生成方式：用 `image` 把 16×16 的
/// 四象限 RGBA 编码成 PNG 后粘贴字节（无外部资产依赖）。
pub(crate) const ICON_PNG: [u8; 189] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0xF3, 0xFF,
    0x61, 0x00, 0x00, 0x00, 0x84, 0x49, 0x44, 0x41, 0x54, 0x78, 0x01, 0xED, 0xC0, 0x03, 0xA0, 0x24,
    0x59, 0x96, 0xC6, 0xF1, 0xFF, 0x77, 0xEE, 0x8D, 0xC8, 0xCC, 0xA7, 0x72, 0x4B, 0x63, 0xAE, 0x6D,
    0xDB, 0xB6, 0x6D, 0xDB, 0xB6, 0x6D, 0xDB, 0xB6, 0x6D, 0x69, 0x8C, 0x9E, 0x96, 0x4A, 0xAF, 0x9E,
    0x32, 0x33, 0x22, 0xEE, 0xF9, 0x76, 0xB7, 0x6A, 0x7A, 0xA6, 0x87, 0x3B, 0x6B, 0xD5, 0xAF, 0xDE,
    0xF5, 0xD6, 0xAF, 0x6E, 0x5E, 0x88, 0x8D, 0x8F, 0xDC, 0xF8, 0x31, 0x5E, 0x30, 0x82, 0x7F, 0x1F,
    0x82, 0x7F, 0x1F, 0x82, 0x7F, 0x1F, 0x82, 0x7F, 0x1F, 0x82, 0x7F, 0x1F, 0x82, 0x7F, 0x1F, 0x82,
    0x7F, 0x1F, 0xF4, 0x86, 0xDF, 0x7D, 0xD7, 0x3B, 0xF0, 0x42, 0xDC, 0xF6, 0xC5, 0xAF, 0xFB, 0x63,
    0xBC, 0x60, 0x04, 0xFF, 0x3E, 0x04, 0xFF, 0x3E, 0x04, 0xFF, 0x3E, 0x04, 0xFF, 0x3E, 0x04, 0xFF,
    0x3E, 0x04, 0xFF, 0x3E, 0x04, 0xFF, 0x3E, 0xFC, 0x23, 0x90, 0xDB, 0x08, 0x38, 0x64, 0x32, 0x41,
    0xFE, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// 两个图片节点的逻辑矩形（1:1 与 2× 放大）。
const ICON_SLOTS: [(f32, f32, f32); 2] = [(40.0, 300.0, 16.0), (88.0, 300.0, 32.0)];

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

    // 图片（P13）：内嵌 16×16 PNG → `ResourceCache` → 图片图集 tile → `Image` 图元。
    // 一个 1:1、一个 2× 放大（验证 tile 的 UV 与放大采样）。
    let icon = app
        .resources_mut()
        .insert(&ICON_PNG)
        .expect("the embedded 16x16 PNG must decode");
    for (x, y, size) in ICON_SLOTS {
        let bounds = Rect::new(x, y, size, size);
        let mut canvas = Canvas::new(bounds);
        canvas.push_image(Image {
            bounds,
            tile: icon.tile,
            modulation: Color::WHITE,
        });
        let _ = NodeBuilder::canvas(canvas)
            .layout_override(bounds)
            .build(app.window_mut(window).expect("window").tree_mut(), root);
    }

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
