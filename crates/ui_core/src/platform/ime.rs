//! 平台无关的 IME 纯逻辑：
//!
//! - 提交去重（macOS / Linux 共用）：连续相同字符 / 双空格是合法输入，只有**同一次按键内**的
//!   重复回调才吞掉（`design/phase-12-macos-linux.md` §7.5，P14 终审项 1）。
//! - AppKit `replacementRange`（UTF-16）→ 组合串内替换区间（macOS `NSTextInputClient` 专用，
//!   但换算规则是纯整数处理，与 AppKit 类型无关）。
//!
//! 放在平台无关层是为了让这些纯逻辑能在开发机（Windows）上被单测覆盖：
//! macOS 的 `NSTextInputClient` 包装器只在 macOS 目标上编译，测试跑不到。

use smol_str::SmolStr;

/// AppKit `NSNotFound`：`replacementRange` 用它表示「没有显式替换区间」。
///
/// **值是 `NSIntegerMax`（`isize::MAX`），不是 `NSUInteger::MAX`**：objc2-foundation 的
/// `NSNotFound: NSInteger = NSIntegerMax`，AppKit 传给 `NSUInteger` 形参时是
/// `0x7FFF_FFFF_FFFF_FFFF`（64 位）。写 `usize::MAX` 会让真实输入法的正常路径
/// `{NSNotFound, 0}` 不匹配、退化成「在组合串尾部插入」（第三轮复审 P1）。
///
/// 只有 macOS 包装器、macOS 绑定测试（`platform/macos/ime.rs`）与跨平台单测用得到；
/// Linux 的 `zwp_text_input_v3` 是整串 preedit，没有区间替换概念，因此编译期排除，
/// 避免 `dead_code`。
#[cfg(any(target_os = "macos", test))]
pub(crate) const NS_NOT_FOUND: usize = isize::MAX as usize;

/// AppKit `replacementRange`（UTF-16 码元，**文档坐标**）→ 组合串内要替换的旧文本区间。
///
/// `setMarkedText:selectedRange:replacementRange:` / `insertText:replacementRange:` 的
/// `replacementRange` 语义是「用新文本替换掉该区间里的旧文本」；`{NSNotFound, 0}` 表示没有
/// 显式区间 —— 替换当前的整个 marked text（没有 marked text 时在插入点插入）。
///
/// `location` / `length` 是 AppKit 的**文档坐标**（与 `markedRange` 同一坐标系），不是组合串
/// 内坐标：真实的 `NSTextInputClient` 文本存储覆盖整个文档，Qt 的实现同样先减去
/// `markedRange.location` 再切片。本 view 只把组合串暴露为文本存储，所以
/// `marked_location` 传 `ImeState::marked_range().0`（当前恒为 0）。**该参数必须与引擎真实
/// 文本里组合串的起点一致**，否则替换区间会整体偏移；未来接通 surrounding text 时把真实
/// 起点传进来即可，本函数不用改。
///
/// 先换算成组合串内相对坐标，再按现有规则 clamp：
///
/// - `location` 在组合串之前：相对起点饱和到 `0`（起点 clamp 到组合串开头；区间本身无法
///   表达，属 surrounding-text 未实现的已知限制）；
/// - 越界 clamp：`start` / `end` 都收敛到 `0..=marked.len()`（输入法在候选/重转换路径上
///   可能给出与当前组合串不一致的区间，直接按入参切片会越界）；
/// - 码点边界吸附：区间端点落在代理对中间时向外扩到整个码点（拆开代理对会得到非法 UTF-16，
///   应用侧无法安全切片）。
///
/// 返回的区间满足 `start <= end <= marked.len()`，且两个端点都是**码点边界**。
///
/// 平台无关（入参是 `usize` 而不是 `NSRange`）：macOS 的包装器只在 macOS 目标上编译，
/// 换算规则放这里才能在开发机（Windows）上单测。
#[cfg(any(target_os = "macos", test))]
pub(crate) fn replacement_range(
    location: usize,
    length: usize,
    marked: &[u16],
    marked_location: usize,
) -> Option<std::ops::Range<usize>> {
    if location == NS_NOT_FOUND {
        return None;
    }
    // `marked_location` 是 `markedRange.location`（组合串起点的文档坐标），不可能是
    // `NSNotFound`：真没有组合串时长度也为 0，起点仍是合法位置。
    debug_assert_ne!(
        marked_location, NS_NOT_FOUND,
        "marked_location 必须是组合串起点的文档坐标，不是 NSNotFound"
    );
    // 文档坐标 → 组合串内坐标：起点在组合串之前时饱和到 0（其余 clamp 在下面统一处理）。
    let location = location.saturating_sub(marked_location);
    let len = marked.len();
    let mut start = location.min(len);
    let mut end = location.saturating_add(length).min(len);
    // UTF-16 代理对只占一个码点，但拆成两个码元后不是合法 UTF-16。
    let is_high_surrogate = |unit: u16| (0xD800..=0xDBFF).contains(&unit);
    let is_low_surrogate = |unit: u16| (0xDC00..=0xDFFF).contains(&unit);
    if start > 0 && start < len && is_low_surrogate(marked[start]) {
        // 起点落在低代理码元上 ⇒ 边界回到它前面的高代理（低代理前面必有高代理，不会下溢）。
        start -= 1;
    }
    if end > 0 && end < len && is_high_surrogate(marked[end - 1]) {
        // 终点停在高代理码元之后（= 落在代理对中间）⇒ 收进后面的低代理。
        end += 1;
    }
    Some(start..end)
}

/// 提交去重状态机。
#[derive(Debug, Default)]
pub(crate) struct CommitDeduper {
    /// 本按键内最近一次提交。
    last: SmolStr,
    /// 本按键是否已经提交过（`begin_key` 复位）。
    committed_in_key: bool,
}

impl CommitDeduper {
    /// 一次新的按键开始：清掉「同一按键」的提交记忆（跨按键的相同文本是合法的）。
    pub(crate) fn begin_key(&mut self) {
        self.committed_in_key = false;
    }

    /// 组合态变化：下一次提交是新的内容，不该被上一按键的记录吞掉。
    pub(crate) fn reset(&mut self) {
        self.committed_in_key = false;
    }

    /// 记录一次提交；返回 `false` 表示**同一次按键内**的重复回调（调用方应吞掉）。
    ///
    /// 跨按键的相同文本（`"ll"`、双空格）返回 `true`：那是用户真的又敲了一次。
    pub(crate) fn note(&mut self, text: &str) -> bool {
        let next = SmolStr::new(text);
        let duplicate = !text.is_empty() && self.committed_in_key && next == self.last;
        self.committed_in_key = true;
        self.last = next;
        !duplicate
    }
}

#[cfg(test)]
mod tests {
    use super::{CommitDeduper, NS_NOT_FOUND, replacement_range};

    /// UTF-16 码元视图（AppKit 的 `NSRange` 单位）。
    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn ns_not_found_means_replace_the_current_marked_text() {
        // AppKit 的 `{NSNotFound, 0}`：没有显式区间，调用方替换整个组合串。
        // 真实哨兵是 `NSIntegerMax`，不是 `NSUInteger::MAX`（复审 P1 的回归点）。
        assert_eq!(NS_NOT_FOUND, isize::MAX as usize);
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &utf16("ni"), 0), None);
        assert_eq!(replacement_range(NS_NOT_FOUND, 3, &utf16("你好"), 0), None);
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &[], 0), None);
        // `NSNotFound` 与组合串起点无关：文档坐标起点非 0 时同样表示整段替换。
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &utf16("ni"), 5), None);
        assert_eq!(replacement_range(1, 0, &utf16("ni"), 0), Some(1..1));
        assert_eq!(replacement_range(2, 0, &utf16("ni"), 0), Some(2..2));
    }

    #[test]
    fn an_empty_range_is_an_insertion_point_inside_the_composition() {
        assert_eq!(replacement_range(0, 0, &utf16("ni"), 0), Some(0..0));
        assert_eq!(replacement_range(2, 0, &utf16("ni"), 0), Some(2..2));
    }

    #[test]
    fn a_range_inside_the_marked_text_is_kept() {
        assert_eq!(replacement_range(1, 2, &utf16("shili"), 0), Some(1..3));
        assert_eq!(replacement_range(0, 4, &utf16("shili"), 0), Some(0..4));
    }

    #[test]
    fn out_of_bounds_ranges_are_clamped_to_the_marked_text() {
        let marked = utf16("ni");
        assert_eq!(replacement_range(9, 1, &marked, 0), Some(2..2));
        assert_eq!(replacement_range(1, 99, &marked, 0), Some(1..2));
        assert_eq!(replacement_range(0, usize::MAX, &marked, 0), Some(0..2));
        assert_eq!(
            replacement_range(NS_NOT_FOUND - 1, 2, &marked, 0),
            Some(2..2)
        );
        // 没有组合串时（例如 `insertText:` 在提交前清空）任何区间都收敛成插入点。
        assert_eq!(replacement_range(0, 0, &[], 0), Some(0..0));
        assert_eq!(replacement_range(3, 2, &[], 0), Some(0..0));
    }

    /// `replacementRange` 是 AppKit 的**文档坐标**：组合串起点不是 0 时（surrounding text
    /// 接通后的形态），先减去 `markedRange.location` 再 clamp/吸附。
    #[test]
    fn document_coordinates_are_rebased_onto_the_marked_text_start() {
        let marked = utf16("shili");
        // 组合串在文档 [5, 10)；文档区间 [7, 8) ⇒ 组合串内 [2, 3)。
        assert_eq!(replacement_range(7, 1, &marked, 5), Some(2..3));
        // 文档位置正好等于组合串起点。
        assert_eq!(replacement_range(5, 2, &marked, 5), Some(0..2));
        // 区间起点在组合串之前：相对起点饱和到 0（clamp 到组合串开头，不越界）。
        assert_eq!(replacement_range(3, 0, &marked, 5), Some(0..0));
        assert_eq!(replacement_range(0, 0, &marked, 5), Some(0..0));
        // 文档区间跨过组合串末尾：终点 clamp 到组合串长度。
        assert_eq!(replacement_range(8, 99, &marked, 5), Some(3..5));
        // 指向组合串之后的已提交文本：收敛成末尾插入点（surrounding text 未实现，已知限制）。
        assert_eq!(replacement_range(12, 0, &marked, 5), Some(5..5));
        // `NSNotFound` 与起点无关：仍然表示「替换整个组合串」。
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &marked, 5), None);
    }

    #[test]
    fn cjk_ranges_are_counted_in_utf16_units() {
        // 每个汉字都是 1 个 UTF-16 码元（与 `str::len` 的 3 字节不同）。
        let marked = utf16("你好");
        assert_eq!(marked.len(), 2);
        assert_eq!(replacement_range(0, 1, &marked, 0), Some(0..1));
        assert_eq!(replacement_range(1, 1, &marked, 0), Some(1..2));
        assert_eq!(replacement_range(2, 0, &marked, 0), Some(2..2));
    }

    #[test]
    fn surrogate_pairs_are_never_split() {
        // 🌍 = U+1F30D = 高代理 D83C + 低代理 DF0D（2 个码元），'a' 1 个码元。
        let marked = utf16("🌍a");
        assert_eq!(marked.len(), 3);

        // 起点落在低代理上：边界回到高代理；长度 0 也要覆盖整个代理对。
        assert_eq!(replacement_range(1, 0, &marked, 0), Some(0..2));
        // 终点落在高代理之后：收进低代理。
        assert_eq!(replacement_range(0, 1, &marked, 0), Some(0..2));
        // 整个 emoji 的组合串内替换区间。
        assert_eq!(replacement_range(0, 2, &marked, 0), Some(0..2));
        // 代理对之后的普通字符不受影响。
        assert_eq!(replacement_range(2, 1, &marked, 0), Some(2..3));
        // emoji 在结尾：终点越界 clamp 后再吸附回码点边界。
        let trailing = utf16("a🌍");
        assert_eq!(replacement_range(1, 1, &trailing, 0), Some(1..3));
        assert_eq!(replacement_range(1, 9, &trailing, 0), Some(1..3));
    }

    #[test]
    fn replacement_range_is_idempotent_for_already_valid_ranges() {
        let marked = utf16("🌍你好");
        assert_eq!(marked.len(), 4);
        for range in [0..0, 0..2, 2..3, 2..4, 4..4] {
            let start = range.start;
            let len = range.end - range.start;
            assert_eq!(
                replacement_range(start, len, &marked, 0),
                Some(range),
                "合法区间必须原样返回"
            );
        }
    }

    #[test]
    fn consecutive_identical_commits_are_delivered() {
        let mut dedupe = CommitDeduper::default();
        dedupe.begin_key();
        assert!(dedupe.note("l"), "first `l` must be delivered");
        dedupe.begin_key();
        assert!(
            dedupe.note("l"),
            "second `l` is a new keystroke, not a duplicate"
        );
    }

    #[test]
    fn consecutive_spaces_are_delivered() {
        let mut dedupe = CommitDeduper::default();
        dedupe.begin_key();
        assert!(dedupe.note(" "));
        dedupe.begin_key();
        assert!(dedupe.note(" "), "double space must not be swallowed");
    }

    #[test]
    fn duplicate_commit_within_one_key_is_swallowed() {
        let mut dedupe = CommitDeduper::default();
        dedupe.begin_key();
        assert!(dedupe.note("你"));
        assert!(
            !dedupe.note("你"),
            "same text twice in one keystroke is an IME duplicate"
        );
    }

    #[test]
    fn a_different_text_in_the_same_key_is_delivered() {
        let mut dedupe = CommitDeduper::default();
        dedupe.begin_key();
        assert!(dedupe.note("a"));
        assert!(dedupe.note("b"));
    }

    #[test]
    fn resetting_the_window_makes_the_same_text_legal_again() {
        let mut dedupe = CommitDeduper::default();
        dedupe.begin_key();
        assert!(dedupe.note("ni"));
        dedupe.reset();
        assert!(dedupe.note("ni"));
    }

    #[test]
    fn three_identical_commits_across_keys_all_pass() {
        let mut dedupe = CommitDeduper::default();
        for _ in 0..3 {
            dedupe.begin_key();
            assert!(dedupe.note("a"));
        }
    }
}
