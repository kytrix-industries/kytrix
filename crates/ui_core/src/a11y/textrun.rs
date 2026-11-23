use unicode_segmentation::UnicodeSegmentation;

use crate::{
    a11y::A11yTextSelection,
    math::Scale,
    node::{id::NodeId, tree::NodeTree},
    text::TextDirection,
};

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
    if !text.is_empty() && starts.first().copied() != Some(0) {
        starts.insert(0, 0);
    }
    Some((lengths, starts))
}

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

pub(crate) fn byte_index(lengths: &[u8], grapheme: usize) -> usize {
    lengths
        .iter()
        .take(grapheme)
        .map(|len| usize::from(*len))
        .sum()
}

pub(crate) fn selection(lengths: &[u8], selection: A11yTextSelection) -> A11yTextSelection {
    A11yTextSelection {
        anchor: grapheme_index(lengths, selection.anchor),
        focus: grapheme_index(lengths, selection.focus),
    }
}

pub(crate) fn character_metrics(
    tree: &NodeTree,
    node: NodeId,
    value: &str,
    lengths: &[u8],
    scale: Scale,
) -> Option<(Vec<f32>, Vec<f32>)> {
    if lengths.is_empty() {
        return None;
    }
    let content = tree.text_content(node)?;
    if value != content.text.as_str() {
        return None;
    }
    let slot = tree.text_slot(node)?;
    let shaped = slot.shaped();
    let direction_is_ltr = match content.style.direction {
        TextDirection::Ltr => true,
        TextDirection::Rtl => false,
        TextDirection::Auto => shaped.has_only_ltr_runs(),
    };
    if !direction_is_ltr {
        return None;
    }
    tree.text_caret_rect(node, 0).ok()?;
    let width = f32::from_bits(slot.laid_width_bits());
    let alignment = content.style.alignment;
    let scale_factor = scale.factor();
    let content_x = tree.content_offset(node).x;

    let first_y = shaped.caret_rect(width, alignment, 0).min().y;
    let mut positions = Vec::with_capacity(lengths.len());
    let mut byte = 0usize;
    for len in lengths {
        let rect = shaped.caret_rect(width, alignment, byte);
        positions.push((content_x + rect.min().x) * scale_factor);
        byte += usize::from(*len);
    }
    let end = shaped.caret_rect(width, alignment, byte);
    if end.min().y != first_y {
        return None;
    }
    positions.push((content_x + end.min().x) * scale_factor);
    let mut widths = Vec::with_capacity(lengths.len());
    for pair in positions.windows(2) {
        widths.push(pair[1] - pair[0]);
    }
    positions.pop();
    if positions
        .iter()
        .chain(widths.iter())
        .any(|value| !value.is_finite())
        || widths.iter().any(|width| *width < 0.0)
    {
        return None;
    }
    debug_assert_eq!(positions.len(), lengths.len());
    Some((positions, widths))
}

#[cfg(test)]
mod tests {
    use super::{byte_index, grapheme_index, selection, text_metrics};
    use crate::a11y::A11yTextSelection;

    fn lengths(text: &str) -> Vec<u8> {
        text_metrics(text).expect("metrics").0
    }

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

    #[test]
    fn cjk_counts_bytes_per_grapheme() {
        assert_eq!(lengths("你好ab"), vec![3, 3, 1, 1]);
        assert_sum_invariant("你好ab");
        assert_eq!(grapheme_index(&lengths("你好ab"), 0), 0);
        assert_eq!(grapheme_index(&lengths("你好ab"), 3), 1, "第二个汉字起点");
        assert_eq!(grapheme_index(&lengths("你好ab"), 6), 2, "ASCII 起点");
        assert_eq!(grapheme_index(&lengths("你好ab"), 8), 4, "文本末尾");
    }

    #[test]
    fn astral_characters_are_one_grapheme() {
        assert_eq!(lengths("🌍a"), vec![4, 1]);
        assert_sum_invariant("🌍a");
        assert_eq!(grapheme_index(&lengths("🌍a"), 4), 1);
        assert_eq!(grapheme_index(&lengths("🌍a"), 2), 0, "半个代理对向下吸附");
    }

    #[test]
    fn zwj_sequences_are_one_grapheme() {
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(lengths(family), vec![18], "ZWJ 家族是一个可选单元");
        assert_sum_invariant(family);
        assert_eq!(grapheme_index(&lengths(family), 4), 0, "中间不可选");
    }

    #[test]
    fn regional_indicator_flags_are_one_grapheme() {
        let flag = "🇨🇳";
        assert_eq!(lengths(flag), vec![8]);
        assert_sum_invariant(flag);
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let text = "e\u{301}x";
        assert_eq!(lengths(text), vec![3, 1]);
        assert_sum_invariant(text);
    }

    #[test]
    fn word_starts_follow_unicode_word_boundaries() {
        let (_, starts) = text_metrics("BTC price 42,150").expect("metrics");
        assert_eq!(starts, vec![0, 4, 10]);
        let (_, cjk) = text_metrics("最新价 42").expect("metrics");
        assert_eq!(cjk, vec![0, 1, 2, 4], "CJK 逐字词 + 数字词");
    }

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

    #[test]
    fn empty_text_has_no_metrics() {
        assert_eq!(text_metrics(""), Some((Vec::new(), Vec::new())));
    }

    #[test]
    fn word_starts_start_at_zero_for_non_empty_runs() {
        for text in ["  abc", "!!!", "价🌍ab", "a b", "  "] {
            let (_, starts) = text_metrics(text).expect("metrics");
            assert_eq!(
                starts.first().copied(),
                Some(0),
                "{text:?} 的 word_starts 必须以 0 开始：{starts:?}"
            );
            assert!(
                starts.windows(2).all(|pair| pair[0] < pair[1]),
                "{text:?} 的 word_starts 必须严格升序：{starts:?}"
            );
        }
        assert_eq!(
            text_metrics("").expect("metrics").1,
            Vec::<u8>::new(),
            "空文本没有词起点"
        );
    }

    #[test]
    fn byte_index_is_the_inverse_of_grapheme_index() {
        let g = lengths("价🌍ab");
        assert_eq!(g, vec![3, 4, 1, 1]);
        assert_eq!(
            (0..=4)
                .map(|index| byte_index(&g, index))
                .collect::<Vec<_>>(),
            vec![0, 3, 7, 8, 9],
            "grapheme 下标 → 字节偏移（含末尾 caret）"
        );
        for grapheme in 0..=g.len() {
            let byte = byte_index(&g, grapheme);
            assert_eq!(grapheme_index(&g, byte), grapheme);
        }
        assert_eq!(byte_index(&g, 99), 9);
        assert_eq!(byte_index(&[], 0), 0);
        assert_eq!(byte_index(&[], 5), 0);
    }
}
