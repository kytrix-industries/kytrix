//! ① 焦点与动作：3 个色块（互斥选择 = `RadioButton`）+ 选中说明
//! （`design/phase-15-demo-showcase.md` §2.2）。

use kytrix_gui::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    event::Action,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::{Cursor, PaintSpec, StylePatch},
    window::WindowId,
};
use smol_str::{SmolStr, format_smolstr};

use super::{Scene, TEXT_RGBA, card, color, ring, text_style};

/// 三个色块的可访问名 / 焦点状态名（顺序 = Tab 序）。
pub(crate) const LABELS: [&str; 3] = ["色块 1（红）", "色块 2（绿）", "色块 3（蓝）"];

/// 常态颜色（sRGB）。
const BASES: [[u8; 4]; 3] = [
    [0xE5, 0x48, 0x4D, 0xFF],
    [0x30, 0xA4, 0x6C, 0xFF],
    [0x00, 0x91, 0xFF, 0xFF],
];

/// 选中后的高亮颜色。
const HIGHLIGHTS: [[u8; 4]; 3] = [
    [0xF2, 0x55, 0x5A, 0xFF],
    [0x3C, 0xB1, 0x79, 0xFF],
    [0x2B, 0x9F, 0xFF, 0xFF],
];

/// 写入剪贴板的十六进制值（点击色块时）。
const HEX: [&str; 3] = ["#E5484D", "#30A46C", "#0091FF"];

/// 色块在卡片内的逻辑矩形（x, y, w, h）。
const SLOTS: [(f32, f32, f32, f32); 3] = [
    (18.0, 42.0, 150.0, 88.0),
    (202.0, 42.0, 150.0, 88.0),
    (386.0, 42.0, 150.0, 88.0),
];

/// 建 3 个色块 + 选中说明；返回色块节点（顺序 = Tab 序）。
pub(crate) fn build(
    app: &mut App,
    window: WindowId,
    panel: NodeId,
    active: Signal<u8>,
) -> Vec<NodeId> {
    let mut swatches = Vec::with_capacity(SLOTS.len());
    for (index, slot) in SLOTS.iter().enumerate() {
        let base = color(BASES[index]);
        let highlight = color(HIGHLIGHTS[index]);
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(slot.0, slot.1, slot.2, slot.3))
            .paint(PaintSpec {
                background: base,
                corner_radius: [12.0; 4],
                ..PaintSpec::default()
            })
            .on_click(Action::custom("swatch.select"))
            .focus_ring(ring())
            .a11y(A11yOverride {
                role: Some(A11yRole::RadioButton),
                label: Some(SmolStr::new_static(LABELS[index])),
                ..A11yOverride::default()
            })
            .bind_style(move |cx| {
                let selected = usize::from(cx.read(&active)) == index;
                StylePatch {
                    background: Some(if selected { highlight } else { base }),
                    cursor: Some(Cursor::Pointer),
                    ..StylePatch::default()
                }
            })
            .bind_a11y(move |cx| A11yPatch {
                checked: Some(usize::from(cx.read(&active)) == index),
                ..A11yPatch::default()
            })
            .build(card::tree(app, window), panel)
            .expect("色块挂载");
        swatches.push(node);
    }
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 142.0, 520.0, 20.0),
        text_style(12.5, 400, TEXT_RGBA),
        move |cx| selected_line(cx.read(&active)),
    );
    swatches
}

/// 选中说明的文案。
fn selected_line(active: u8) -> SmolStr {
    let index = usize::from(active);
    if index < LABELS.len() {
        format_smolstr!("选中：{} · {}", LABELS[index], HEX[index])
    } else {
        SmolStr::new_static("选中：（无）—— 点击任一色块")
    }
}

/// 注册色块的动作处理器（同一个 `Action` 被指针点击 / 键盘激活 / AT `Click` 共享）。
pub(crate) fn install(app: &mut App, scene: &Scene) {
    let swatches = scene.swatches.clone();
    let active = scene.active;
    app.on_action(Action::custom("swatch.select"), move |ctx| {
        let Some(index) = swatches.iter().position(|node| *node == ctx.node()) else {
            return;
        };
        ctx.write(&active, index as u8);
        // 剪贴板：把该色块的十六进制值写出去（无原生窗口/被占用时失败，不影响 UI）。
        let _ = ctx.app_mut().set_clipboard_text(HEX[index]);
    });
}

#[cfg(test)]
mod tests {
    use super::{HEX, LABELS, selected_line};

    #[test]
    fn selected_line_names_the_swatch_and_its_hex() {
        assert_eq!(selected_line(0).as_str(), "选中：色块 1（红） · #E5484D");
        assert_eq!(selected_line(2).as_str(), "选中：色块 3（蓝） · #0091FF");
        assert_eq!(
            selected_line(u8::MAX).as_str(),
            "选中：（无）—— 点击任一色块"
        );
    }

    #[test]
    fn labels_and_hex_are_aligned() {
        assert_eq!(LABELS.len(), HEX.len());
        for (label, hex) in LABELS.iter().zip(HEX) {
            assert!(label.starts_with("色块"));
            assert!(hex.starts_with('#'));
            assert_eq!(hex.len(), 7);
        }
    }
}
