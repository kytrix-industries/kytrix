//! TextRun 的文本语义：逐字符（grapheme）字节长度、词起点、字节偏移 → grapheme 下标。
//!
//! AccessKit 的 `character_lengths` 是"**最小可选单元**"的 UTF-8 字节长度，它的文档明确说
//! "AccessKit 无法从文本自身算出这些长度，必须由文本编辑实现提供"。本模块就是那个实现：
//!
//! - `character_lengths`：grapheme cluster（UAX #29）而不是 `char` —— CJK（3 字节/字）、
//!   emoji（4 字节，UTF-16 里是代理对）、ZWJ 家族（18 字节仍是**一个**可选单元）、区域旗帜
//!   （8 字节一个）都按"用户看到的一个字符"计数；
//! - `word_starts`：Unicode 词边界（UAX #29 的 word segmentation）的起点下标；
//! - 选区：应用/IME 给的 UTF-8 字节偏移换算成 grapheme 下标（AT 侧 `TextPosition` 用下标），
//!   非边界偏移向下吸附到所在 grapheme 的起点、越界夹紧。
//!
//! **AccessKit 的类型限制**：`character_lengths` / `word_starts` 的元素都是 `u8`。
//! 单个 grapheme 超过 255 字节（病态输入）时整段不暴露长度（而不是给出错误的和 ——
//! `Σ character_lengths == value.len()` 是硬约束）；超过 255 个词的长文本只暴露前 255 个
//! 词起点（与 AccessKit 自身的表达能力一致，文本运行本来就应该是小段）。

use unicode_segmentation::UnicodeSegmentation;

use crate::a11y::A11yTextSelection;

/// 文本的 `(character_lengths, word_starts)`；病态 grapheme 超过 `u8` 表示能力时返回 `None`。
pub(crate) fn text_metrics(text: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut lengths = Vec::with_capacity(text.len().min(256));
    for grapheme in text.graphemes(true) {
        let len = u8::try_from(grapheme.len()).ok()?;
        lengths.push(len);
    }
    let mut starts = Vec::new();
    for (byte, _word) in text.unicode_word_indices() {
        let index = grapheme_index(&lengths, byte);
        let Ok(index) = u8::try_from(index) else {
            break;
        };
        starts.push(index);
    }
    Some((lengths, starts))
}

/// 字节偏移 → grapheme 下标（向下吸附、越界夹紧到 `lengths.len()`）。
///
/// `byte == Σ lengths`（文本末尾）⇒ `lengths.len()`（caret 在最后一个字符之后）。
pub(crate) fn grapheme_index(lengths: &[u8], byte: usize) -> usize {
    let mut offset = 0usize;
    for (index, len) in lengths.iter().enumerate() {
        offset += usize::from(*len);
        if byte < offset {
            return index;
        }
    }
    lengths.len()
}

/// 字节偏移的选区 → grapheme 下标的选区。
pub(crate) fn selection(lengths: &[u8], selection: A11yTextSelection) -> A11yTextSelection {
    A11yTextSelection {
        anchor: grapheme_index(lengths, selection.anchor),
        focus: grapheme_index(lengths, selection.focus),
    }
}

#[cfg(test)]
mod tests {
    use super::{grapheme_index, selection, text_metrics};
    use crate::a11y::A11yTextSelection;

    fn lengths(text: &str) -> Vec<u8> {
        text_metrics(text).expect("metrics").0
    }

    /// `Σ character_lengths == value 的字节长度` 是 AccessKit 的硬约束。
    fn assert_sum_invariant(text: &str) {
        let (lengths, _) = text_metrics(text).expect("metrics");
        let sum: usize = lengths.iter().map(|len| usize::from(*len)).sum();
        assert_eq!(sum, text.len(), "character_lengths 之和必须等于文本字节数");
    }

    #[test]
    fn ascii_is_one_grapheme_per_byte() {
        assert_eq!(lengths("42150"), vec![1, 1, 1, 1, 1]);
        assert_sum_invariant("42150");
    }

    /// CJK：每个汉字 3 个 UTF-8 字节，但仍是**一个**可选字符。
    #[test]
    fn cjk_counts_bytes_per_grapheme() {
        assert_eq!(lengths("你好ab"), vec![3, 3, 1, 1]);
        assert_sum_invariant("你好ab");
        assert_eq!(grapheme_index(&lengths("你好ab"), 0), 0);
        assert_eq!(grapheme_index(&lengths("你好ab"), 3), 1, "第二个汉字起点");
        assert_eq!(grapheme_index(&lengths("你好ab"), 6), 2, "ASCII 起点");
        assert_eq!(grapheme_index(&lengths("你好ab"), 8), 4, "文本末尾");
    }

    /// 代理对（astral plane）：UTF-16 里是两个码元，UTF-8 里是 4 字节 = 一个 grapheme。
    #[test]
    fn astral_characters_are_one_grapheme() {
        assert_eq!(lengths("🌍a"), vec![4, 1]);
        assert_sum_invariant("🌍a");
        assert_eq!(grapheme_index(&lengths("🌍a"), 4), 1);
        assert_eq!(grapheme_index(&lengths("🌍a"), 2), 0, "半个代理对向下吸附");
    }

    /// ZWJ 组合（👨‍👩‍👧 = 3 个 emoji + 2 个 ZWJ）在用户看来是**一个**字符。
    #[test]
    fn zwj_sequences_are_one_grapheme() {
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(lengths(family), vec![18], "ZWJ 家族是一个可选单元");
        assert_sum_invariant(family);
        assert_eq!(grapheme_index(&lengths(family), 4), 0, "中间不可选");
    }

    /// 区域旗帜（两个 regional indicator）= 一个 grapheme。
    #[test]
    fn regional_indicator_flags_are_one_grapheme() {
        let flag = "🇨🇳";
        assert_eq!(lengths(flag), vec![8]);
        assert_sum_invariant(flag);
    }

    /// 组合附加符号（e + U+0301）= 一个 grapheme。
    #[test]
    fn combining_marks_stay_with_their_base() {
        let text = "e\u{301}x";
        assert_eq!(lengths(text), vec![3, 1]);
        assert_sum_invariant(text);
    }

    /// 词起点：CJK 与 ASCII 混排、标点与空白不产生"词起点"以外的语义。
    #[test]
    fn word_starts_follow_unicode_word_boundaries() {
        let (_, starts) = text_metrics("BTC price 42,150").expect("metrics");
        // unicode_words: "BTC"(0), "price"(4), "42,150"(10) —— UAX #29 的 MidNum 规则把
        // 逗号分隔的千位数字视为**同一个**词，所以没有第 4 个起点。
        assert_eq!(starts, vec![0, 4, 10]);
        let (_, cjk) = text_metrics("最新价 42").expect("metrics");
        // UAX #29 的默认分词把每个汉字当作独立词（没有词典时的标准行为），所以 CJK 段的
        // 词起点是逐字的；画面/读屏的"按词移动"在 CJK 里本来就是逐字，一致。
        assert_eq!(cjk, vec![0, 1, 2, 4], "CJK 逐字词 + 数字词");
    }

    /// 选区：字节偏移 → grapheme 下标（CJK/emoji 混排下不能错位）。
    #[test]
    fn selection_converts_bytes_to_grapheme_indices() {
        let text = "价🌍ab";
        let g = lengths(text);
        assert_eq!(g, vec![3, 4, 1, 1]);
        let caret = selection(
            &g,
            A11yTextSelection {
                anchor: 7,
                focus: 7,
            },
        );
        assert_eq!((caret.anchor, caret.focus), (2, 2), "第 3 个字符前");
        let range = selection(
            &g,
            A11yTextSelection {
                anchor: 0,
                focus: 101,
            },
        );
        assert_eq!((range.anchor, range.focus), (0, 4), "越界夹紧到末尾");
        let mid = selection(
            &g,
            A11yTextSelection {
                anchor: 1,
                focus: 5,
            },
        );
        assert_eq!((mid.anchor, mid.focus), (0, 1), "非边界偏移向下吸附");
    }

    /// 空文本：没有长度、没有词起点（不得 panic）。
    #[test]
    fn empty_text_has_no_metrics() {
        assert_eq!(text_metrics(""), Some((Vec::new(), Vec::new())));
    }
}
