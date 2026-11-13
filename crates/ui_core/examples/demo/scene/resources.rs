//! ⑥ 图片与滚动：内嵌 PNG（1:1 / 2×）+ 20 行滚动列表
//! （`design/phase-15-demo-showcase.md` §1）。
//!
//! 两个能力都是 P13 / P10 已有的公开面：`ResourceCache::insert`（PNG → 解码 → 线性预乘 → 图集 tile）
//! 与 `App::{make_scroll, set_scroll_content}`（自带 clip 的滚动容器）。

use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    interaction::ScrollConfig,
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::Image,
        style::{Clip, PaintSpec},
    },
    window::WindowId,
};

use super::{DIM_RGBA, TRACK_RGBA, card, color, text_style};

/// 内嵌的 16×16 PNG（四个 8×8 象限，alpha = 255 / 170 / 85 / 0）。
///
/// P13 的 `ResourceCache` 生产消费者：解码 → 线性预乘 → sRGB 编码 → 图片图集 tile → 页上传 → 采样，
/// 一条路径全部覆盖（`--features snapshot` 时像素可看）。生成方式：用 `image` 把 16×16 的
/// 四象限 RGBA 编码成 PNG 后粘贴字节（无外部资产依赖）。
const ICON_PNG: [u8; 189] = [
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

/// 两个图片节点的逻辑矩形（1:1 与 2× 放大；相对卡片）。
const ICON_SLOTS: [(f32, f32, f32); 2] = [(18.0, 40.0, 16.0), (44.0, 40.0, 32.0)];

/// 两个图片的替代文本（自绘节点缺替代文本会被引擎标 `hidden` 并记诊断）。
const ICON_LABELS: [&str; 2] = ["Kytrix 图标（16×16，1:1）", "Kytrix 图标（2× 放大）"];

/// 滚动容器（相对卡片）与行高。
const SCROLL_RECT: (f32, f32, f32, f32) = (18.0, 82.0, 276.0, 126.0);

/// 行数。
const ROW_COUNT: usize = 20;

/// 行高。
const ROW_HEIGHT: f32 = 24.0;

/// 建图标 + 滚动列表；不产生可聚焦节点。
///
/// `origin` 是卡片在窗口里的绝对原点：`Canvas` 内部的图元坐标是**绝对窗口逻辑像素**
/// （`paint::primitive` 的契约），与 `LayoutOverride` 的父相对坐标不同，必须显式平移。
pub(crate) fn build(app: &mut App, window: WindowId, panel: NodeId, origin: Point) {
    let icon = app
        .resources_mut()
        .insert(&ICON_PNG)
        .expect("内嵌 16x16 PNG 必须可解码");
    for (index, slot) in ICON_SLOTS.iter().enumerate() {
        let local = Rect::new(slot.0, slot.1, slot.2, slot.2);
        let absolute = local.translate(origin);
        let mut canvas = Canvas::new(absolute);
        canvas.push_image(Image {
            bounds: absolute,
            tile: icon.tile,
            modulation: Color::WHITE,
        });
        let _ = NodeBuilder::canvas(canvas)
            .layout_override(local)
            .a11y(A11yOverride {
                role: Some(A11yRole::Image),
                label: Some(SmolStr::new_static(ICON_LABELS[index])),
                ..A11yOverride::default()
            })
            .build(card::tree(app, window), panel)
            .expect("图标挂载");
    }
    let _ = card::label(
        app,
        window,
        panel,
        Rect::new(86.0, 46.0, 208.0, 18.0),
        "内嵌 PNG → 图集 tile（1:1 / 2×）",
        text_style(11.5, 400, DIM_RGBA),
    );

    let (x, y, width, height) = SCROLL_RECT;
    let mut style = card::absolute_style(Rect::new(x, y, width, height));
    style.clip = Clip::Bounds;
    let scroll = NodeBuilder::boxed()
        .style(style)
        .paint(PaintSpec {
            background: color(TRACK_RGBA),
            corner_radius: [8.0; 4],
            ..PaintSpec::default()
        })
        .build(card::tree(app, window), panel)
        .expect("滚动容器挂载");
    for index in 0..ROW_COUNT {
        let shade = 0x1F + (index as u8 % 8) * 7;
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(
                0.0,
                index as f32 * ROW_HEIGHT,
                width,
                ROW_HEIGHT - 1.0,
            ))
            .paint(PaintSpec {
                background: Color::from_rgba8([shade, shade, shade + 6, 0xFF]),
                ..PaintSpec::default()
            })
            .build(card::tree(app, window), scroll)
            .expect("滚动行挂载");
    }
    if let Ok(handle) = app.make_scroll(window, scroll, ScrollConfig::default()) {
        let _ = app.set_scroll_content(&handle, Size::new(width, ROW_COUNT as f32 * ROW_HEIGHT));
    }
    let _ = card::label(
        app,
        window,
        panel,
        Rect::new(18.0, 212.0, 276.0, 16.0),
        "滚轮滚动（20 行，276×126 视口）",
        text_style(11.0, 400, DIM_RGBA),
    );
}
