use ui_core::{
    app::App,
    math::{LogicalRect, px},
    node::id::NodeId,
    text::TextStyle,
    window::WindowId,
};

use super::{MUTED_RGBA, TEXT_RGBA, card, color, text_style};

const SAMPLES: [&str; 4] = [
    "简体中文：布局由 taffy 计算，字形由 swash 光栅化，支持中英混排。",
    "繁體中文：斷行依 Unicode 規則處理全形標點，例如「引號」與、頓號。",
    "日本語：フォーカス、入力メソッド、アクセシビリティを一つの木で扱います。",
    "한국어: 포커스, 입력기, 접근성을 하나의 트리에서 다룹니다.",
];

const MIXED: &str = "混排断行：Kytrix GUI 用 Parley 的 Unicode 断行，中英混排与全角标点（，。！？）都能正确换行；这一段在 544px 内自动折行，不出现豆腐块。";

pub(crate) fn build(app: &mut App, window: WindowId, panel: NodeId) {
    for (index, sample) in SAMPLES.iter().enumerate() {
        let y = px(40.0) + px(26.0) * index as f32;
        let _ = card::label(
            app,
            window,
            panel,
            LogicalRect::new(px(18.0), y, px(544.0), px(22.0)),
            *sample,
            text_style(px(13.5), 400, TEXT_RGBA),
        );
    }
    let _ = card::label(
        app,
        window,
        panel,
        LogicalRect::new(px(18.0), px(142.0), px(544.0), px(44.0)),
        MIXED,
        TextStyle::new(px(13.0))
            .with_color(color(MUTED_RGBA))
            .with_line_height(1.45),
    );
}

#[cfg(test)]
mod tests {
    use super::{MIXED, SAMPLES};

    fn is_cjk(character: char) -> bool {
        matches!(
            character as u32,
            0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
                | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF
        )
    }

    #[test]
    fn every_sample_contains_at_least_one_cjk_character() {
        for sample in SAMPLES {
            assert!(
                sample.chars().any(is_cjk),
                "样例必须包含 CJK 字符：{sample}"
            );
        }
    }

    #[test]
    fn samples_cover_the_four_requested_scripts() {
        assert!(SAMPLES[0].starts_with("简体中文"));
        assert!(SAMPLES[1].starts_with("繁體中文"));
        assert!(SAMPLES[2].starts_with("日本語"));
        assert!(SAMPLES[3].starts_with("한국어"));
    }

    #[test]
    fn mixed_paragraph_has_both_ascii_and_cjk() {
        assert!(
            MIXED
                .chars()
                .any(|character| character.is_ascii_alphanumeric())
        );
        assert!(MIXED.chars().any(is_cjk));
    }
}

#[cfg(all(test, feature = "test-support"))]
mod showcase_tests {
    use ui_core::{math::px, test_support::record::PrimitiveKind};

    use crate::scene::test_support::{frame, setup};

    #[test]
    fn the_cjk_paragraph_wraps_within_its_card() {
        let (mut test, _window, _scene) = setup();
        frame(&mut test);
        let frame = test
            .frame_record()
            .frame
            .as_ref()
            .expect("提交帧必须有绘制快照");

        let card = crate::scene::CJK_CARD;
        let lower = card.origin.y + 142.0;
        let upper = lower + 44.0;
        let wrapped = frame
            .primitives
            .iter()
            .filter(|primitive| {
                primitive.kind == PrimitiveKind::GlyphRun
                    && primitive.bounds.min().x < card.origin.x + card.size.width
                    && primitive.bounds.min().y >= lower
                    && primitive.bounds.max().y <= upper
                    && primitive.bounds.size.width > px(40.0)
            })
            .count();
        assert!(
            wrapped >= 2,
            "544px 内的长段落必须折行（至少 2 条 run）：{wrapped}"
        );
    }
}
