use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    interaction::ScrollConfig,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::Image,
        style::{Clip, PaintSpec},
    },
    window::WindowId,
};

use super::{DIM_RGBA, TRACK_RGBA, card, color, text_style};

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

const ICON_SLOTS: [(LogicalPx, LogicalPx, LogicalPx); 2] = [
    (px(18.0), px(40.0), px(16.0)),
    (px(44.0), px(40.0), px(32.0)),
];

const ICON_LABELS: [&str; 2] = ["Kytrix 图标（16×16，1:1）", "Kytrix 图标（2× 放大）"];

const SCROLL_RECT: (LogicalPx, LogicalPx, LogicalPx, LogicalPx) =
    (px(18.0), px(82.0), px(276.0), px(126.0));

const ROW_COUNT: usize = 20;

const ROW_HEIGHT: LogicalPx = px(24.0);

pub(crate) fn build(app: &mut App, window: WindowId, panel: NodeId) {
    let icon = app
        .insert_bitmap_for_window(window, &ICON_PNG)
        .expect("内嵌 16x16 PNG 必须可解码");
    for (index, slot) in ICON_SLOTS.iter().enumerate() {
        let local = LogicalRect::new(slot.0, slot.1, slot.2, slot.2);
        let canvas_bounds = LogicalRect::from_origin_size(LogicalPoint::ZERO, local.size);
        let tile = app
            .prepare_image_variant(window, icon, local.size, ImageFit::Fill)
            .expect("位图的原生变体恒可用");
        let mut canvas = Canvas::new(canvas_bounds);
        canvas.push_image(Image {
            bounds: canvas_bounds,
            tile,
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
        LogicalRect::new(px(86.0), px(46.0), px(208.0), px(18.0)),
        "内嵌 PNG → 图集 tile（1:1 / 2×）",
        text_style(px(11.5), 400, DIM_RGBA),
    );

    let (x, y, width, height) = SCROLL_RECT;
    let mut style = card::absolute_style(LogicalRect::new(x, y, width, height));
    style.clip = Clip::Bounds;
    let scroll = NodeBuilder::boxed()
        .style(style)
        .paint(PaintSpec {
            background: color(TRACK_RGBA),
            corner_radius: [px(8.0); 4],
            ..PaintSpec::default()
        })
        .build(card::tree(app, window), panel)
        .expect("滚动容器挂载");
    for index in 0..ROW_COUNT {
        let shade = 0x1F + (index as u8 % 8) * 7;
        let _ = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(
                px(0.0),
                ROW_HEIGHT * index as f32,
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
        let _ = app.set_scroll_content(
            &handle,
            LogicalSize::new(width, ROW_HEIGHT * ROW_COUNT as f32),
        );
    }
    let _ = card::label(
        app,
        window,
        panel,
        LogicalRect::new(px(18.0), px(212.0), px(276.0), px(16.0)),
        "滚轮滚动（20 行，276×126 视口）",
        text_style(px(11.0), 400, DIM_RGBA),
    );
}
