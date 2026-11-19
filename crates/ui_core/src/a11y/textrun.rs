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

use crate::{
    a11y::A11yTextSelection,
    math::Scale,
    node::{id::NodeId, tree::NodeTree},
    text::TextDirection,
};

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
    // AccessKit 契约：词起点之后的每个字符都必须归入某个词（前导非词字符 / 纯标点文本也要有
    // 0 起点），否则读屏的"按词移动"会漏掉开头这段（`unicode_word_indices` 不产生 0 起点）。
    if !text.is_empty() && starts.first().copied() != Some(0) {
        starts.insert(0, 0);
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

/// grapheme 下标 → 该 grapheme 起点的**字节偏移**（[`grapheme_index`] 的逆；E4a）。
///
/// AT 侧（`accesskit::TextPosition.character_index`）是 `character_lengths` 的下标；引擎与
/// 应用层的文本 API 是 UTF-8 字节偏移。下标越界时夹紧到 `Σ lengths`（= 文本末尾的 caret），
/// 因此返回值恒落在 char boundary 上（`lengths` 本身来自 grapheme 划分）。
pub(crate) fn byte_index(lengths: &[u8], grapheme: usize) -> usize {
    lengths
        .iter()
        .take(grapheme)
        .map(|len| usize::from(*len))
        .sum()
}

/// 字节偏移的选区 → grapheme 下标的选区。
pub(crate) fn selection(lengths: &[u8], selection: A11yTextSelection) -> A11yTextSelection {
    A11yTextSelection {
        anchor: grapheme_index(lengths, selection.anchor),
        focus: grapheme_index(lengths, selection.focus),
    }
}

/// TextRun 的逐 grapheme 字符度量（AccessKit `character_positions` / `character_widths`）。
///
/// 返回 `(positions, widths)`：位置是**节点 bounds 原点起算的物理像素 x**（AccessKit consumer
/// 用 `point.x - bounds.x0` 对比 `character_positions`，见 `accesskit_consumer::text` 的
/// `character_index_at_point`），宽度是同一 grapheme 的 advance。数组长度 == `lengths.len()`。
///
/// **只在能正确表达时返回**（否则 `None` ⇒ AT 回退到无 caret 高亮，绝不发错值）：
/// - **文本口径一致**：`Σ character_lengths == content.text.len()`。`character_lengths` 来自
///   节点 a11y `value`（可被 `A11yPatch::value` 覆盖），位置来自**已 shape 的真实文本**；
///   两者不一致（TextRun 上覆盖了 value）时描述的是两串不同文本，输出位置就是错位的 caret
///   表（RE-02：重复位置 + 零宽尾部），因此省略；
/// - `TextRun` 已被 shape 且与当前文本一致：直接复用 `NodeTree::text_caret_rect` 的**全部**
///   输入校验（`text == shaped.text`、`same_shaping_inputs`、行内盒一致、laid 宽度 == 期望宽度、
///   byte 在 char boundary）—— 与 `apply` 用的是同一份真值，不会读到半新半旧的 shape；
/// - **显式 LTR**：`Auto` 需要逐段方向判定、RTL 的 inline 轴方向相反，而 consumer 只处理单一
///   `text_direction`（D6-A）；
/// - **单行**：所有 grapheme 起点 + 末尾 caret 必须落在同一 y（软换行/硬换行都会让 y 变化；
///   AccessKit 的位置模型忽略 y，多行发出去会命中错行）。
pub(crate) fn character_metrics(
    tree: &NodeTree,
    node: NodeId,
    lengths: &[u8],
    scale: Scale,
) -> Option<(Vec<f32>, Vec<f32>)> {
    if lengths.is_empty() {
        return None;
    }
    let content = tree.text_content(node)?;
    // RE-02：`character_lengths` 来自节点的 a11y `value`（`A11yPatch::value` 可覆盖），而位置是用
    // **已 shape 的真实文本** `content.text` 算的。TextRun 上覆盖出不同的 value 时，两者描述的是
    // 两串不同文本 —— 此时发出位置就是「错位的 caret 表」（实测会得到重复位置 + 零宽尾部）。
    // 只在 Σlengths == 真实文本字节数时输出，否则省略（AT 回退）。
    let described_bytes: usize = lengths.iter().map(|len| usize::from(*len)).sum();
    if described_bytes != content.text.len() {
        return None;
    }
    if content.style.direction != TextDirection::Ltr {
        return None;
    }
    let slot = tree.text_slot(node)?;
    // 完整输入校验（失败 = 文本/样式/宽度与上次 shape 不一致，等文本 pass 之后再来）。
    tree.text_caret_rect(node, 0).ok()?;
    let shaped = slot.shaped();
    let width = f32::from_bits(slot.laid_width_bits());
    let alignment = content.style.alignment;
    let scale_factor = scale.factor();
    let content_x = tree.content_offset(node).x;

    let mut positions = Vec::with_capacity(lengths.len());
    let mut byte = 0usize;
    let mut line_y: Option<f32> = None;
    for len in lengths {
        let rect = shaped.caret_rect(width, alignment, byte);
        let y = rect.min().y;
        match line_y {
            None => line_y = Some(y),
            Some(expected) if expected != y => return None,
            Some(_) => {}
        }
        positions.push((content_x + rect.min().x) * scale_factor);
        byte += usize::from(*len);
    }
    // 末尾 caret 参与单行判定，并提供最后一个 grapheme 的 advance。
    let end = shaped.caret_rect(width, alignment, byte);
    if Some(end.min().y) != line_y {
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

    /// E4b：非空文本的 `word_starts` 必须以 0 开始（AccessKit 契约：词起点之后的每个字符都
    /// 必须归入某个词；`unicode_word_indices` 对前导非词字符 / 纯标点文本不产生 0 起点）。
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

    /// E4a：AT 的 grapheme 下标 → 应用层的 UTF-8 字节偏移（`grapheme_index` 的逆）。
    #[test]
    fn byte_index_is_the_inverse_of_grapheme_index() {
        let g = lengths("价🌍ab");
        assert_eq!(g, vec![3, 4, 1, 1]);
        // 每个 grapheme 的起点字节。
        assert_eq!(
            (0..=4)
                .map(|index| byte_index(&g, index))
                .collect::<Vec<_>>(),
            vec![0, 3, 7, 8, 9],
            "grapheme 下标 → 字节偏移（含末尾 caret）"
        );
        // 往返：字节起点 → grapheme 下标 → 同一字节。
        for grapheme in 0..=g.len() {
            let byte = byte_index(&g, grapheme);
            assert_eq!(grapheme_index(&g, byte), grapheme);
        }
        // 越界夹紧到文本末尾（不 panic、不越过 Σ lengths）。
        assert_eq!(byte_index(&g, 99), 9);
        assert_eq!(byte_index(&[], 0), 0);
        assert_eq!(byte_index(&[], 5), 0);
    }
}
