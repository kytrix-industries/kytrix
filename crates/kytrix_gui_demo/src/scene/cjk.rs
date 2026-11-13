//! ③ CJK 排版：四条脚本样例 + 一段中英混排自动换行
//! （`design/phase-15-demo-showcase.md` §1、§4）。
//!
//! 引擎的字体栈（`FontStack::system_ui`）显式列出 CJK 家族，缺字形时继续往后找；本模块只给文本与
//! 宽度，断行 / 光栅 / 图集全部由文本 pass 完成。集成测试断言**没有** `MissingGlyph`（豆腐块）。

use kytrix_gui::{app::App, math::Rect, node::id::NodeId, text::TextStyle, window::WindowId};

use super::{MUTED_RGBA, TEXT_RGBA, card, color, text_style};

/// 单行脚本样例（顺序即绘制序）。
const SAMPLES: [&str; 4] = [
    "简体中文：布局由 taffy 计算，字形由 swash 光栅化，支持中英混排。",
    "繁體中文：斷行依 Unicode 規則處理全形標點，例如「引號」與、頓號。",
    "日本語：フォーカス、入力メソッド、アクセシビリティを一つの木で扱います。",
    "한국어: 포커스, 입력기, 접근성을 하나의 트리에서 다룹니다.",
];

/// 中英混排 + 全角标点的自动换行段落（544px 内应折成多行）。
const MIXED: &str = "混排断行：Kytrix GUI 用 Parley 的 Unicode 断行，中英混排与全角标点（，。！？）都能正确换行；这一段在 544px 内自动折行，不出现豆腐块。";

/// 建 CJK 样例（无交互；不参与 Tab 序）。
pub(crate) fn build(app: &mut App, window: WindowId, panel: NodeId) {
    for (index, sample) in SAMPLES.iter().enumerate() {
        let y = 40.0 + index as f32 * 26.0;
        let _ = card::label(
            app,
            window,
            panel,
            Rect::new(18.0, y, 544.0, 22.0),
            *sample,
            text_style(13.5, 400, TEXT_RGBA),
        );
    }
    let _ = card::label(
        app,
        window,
        panel,
        Rect::new(18.0, 142.0, 544.0, 44.0),
        MIXED,
        TextStyle::new(13.0)
            .with_color(color(MUTED_RGBA))
            .with_line_height(1.45),
    );
}

#[cfg(test)]
mod tests {
    use super::{MIXED, SAMPLES};

    /// 粗略的 CJK 判定：CJK 统一表意文字 / 扩展 A / 假名 / 谚文 / CJK 标点。
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
