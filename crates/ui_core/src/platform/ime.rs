#[cfg(any(windows, target_os = "linux", test))]
use std::ops::Range;

#[cfg(any(windows, target_os = "linux", test))]
use smallvec::SmallVec;

#[cfg(any(windows, target_os = "linux", test))]
pub(crate) const PREEDIT_CLAUSE_INLINE: usize = 4;

#[cfg(any(windows, target_os = "linux", test))]
pub(crate) type ClauseRanges = SmallVec<[Range<usize>; PREEDIT_CLAUSE_INLINE]>;

#[cfg(any(windows, test))]
pub(crate) fn highlight_runs(attrs: &[u8], plain: u8) -> ClauseRanges {
    let mut runs = ClauseRanges::new();
    let mut start: Option<usize> = None;
    for (index, &attr) in attrs.iter().enumerate() {
        if attr == plain {
            if let Some(from) = start.take() {
                runs.push(from..index);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start {
        runs.push(from..attrs.len());
    }
    runs
}

#[cfg(any(windows, test))]
pub(crate) fn clause_ranges(offsets: &[u32], len: usize) -> ClauseRanges {
    let mut ranges = ClauseRanges::new();
    let mut start: Option<usize> = None;
    for &raw in offsets {
        let end = (raw as usize).min(len);
        match start {
            Some(from) if end < from => {}
            Some(from) => {
                if end > from {
                    ranges.push(from..end);
                }
                start = Some(end);
            }
            None => start = Some(end),
        }
    }
    ranges
}

#[cfg(any(windows, test))]
pub(crate) fn preedit_clauses(
    attrs: Option<&[u8]>,
    clause_offsets: Option<&[u32]>,
    len: usize,
    plain: u8,
) -> ClauseRanges {
    if let Some(attrs) = attrs
        && !attrs.is_empty()
    {
        return highlight_runs(&attrs[..attrs.len().min(len)], plain);
    }
    if let Some(offsets) = clause_offsets
        && !offsets.is_empty()
    {
        return clause_ranges(offsets, len);
    }
    ClauseRanges::new()
}

#[cfg(any(target_os = "macos", test))]
pub(crate) const NS_NOT_FOUND: usize = isize::MAX as usize;

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
    debug_assert_ne!(
        marked_location, NS_NOT_FOUND,
        "marked_location 必须是组合串起点的文档坐标，不是 NSNotFound"
    );
    let location = location.saturating_sub(marked_location);
    let len = marked.len();
    let mut start = location.min(len);
    let mut end = location.saturating_add(length).min(len);
    let is_high_surrogate = |unit: u16| (0xD800..=0xDBFF).contains(&unit);
    let is_low_surrogate = |unit: u16| (0xDC00..=0xDFFF).contains(&unit);
    if start > 0 && start < len && is_low_surrogate(marked[start]) {
        start -= 1;
    }
    if end > 0 && end < len && is_high_surrogate(marked[end - 1]) {
        end += 1;
    }
    Some(start..end)
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn preedit_carets(
    text: &str,
    cursor_begin: i32,
    cursor_end: i32,
) -> (Option<u32>, ClauseRanges) {
    if cursor_begin < 0 || cursor_end < 0 {
        return (None, ClauseRanges::new());
    }
    let unit_at = |byte: i32| -> usize {
        let limit = byte as usize;
        let mut units = 0;
        let mut bytes = 0;
        for ch in text.chars() {
            let width = ch.len_utf8();
            if bytes + width > limit {
                break;
            }
            bytes += width;
            units += ch.len_utf16();
        }
        units
    };
    let start = unit_at(cursor_begin);
    let end = unit_at(cursor_end);
    let mut clauses = ClauseRanges::new();
    if end > start {
        clauses.push(start..end);
    }
    (Some(start as u32), clauses)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) const fn key_visible(
    ime_consumed: bool,
    composing: bool,
    command_modifiers: bool,
) -> bool {
    command_modifiers || (!ime_consumed && !composing)
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Default)]
pub(crate) struct ImeCompositionEnd {
    pending: bool,
}

#[cfg(any(target_os = "macos", test))]
impl ImeCompositionEnd {
    pub(crate) fn note_preedit(&mut self) {
        self.pending = true;
    }

    pub(crate) fn note_end(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn note_commit(&mut self) -> bool {
        self.note_end()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyScopeToken(u64);

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitVerdict {
    DeliverInScope,
    DeliverOutsideScope,
    Duplicate,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Default)]
pub(crate) struct ImeCommitScope {
    open: Option<KeyScopeToken>,
    delivered: Option<Vec<u16>>,
    next: u64,
}

#[cfg(any(target_os = "macos", test))]
impl ImeCommitScope {
    pub(crate) fn begin_key(&mut self) -> KeyScopeToken {
        self.next += 1;
        let token = KeyScopeToken(self.next);
        self.open = Some(token);
        self.delivered = None;
        token
    }

    pub(crate) fn end_key(&mut self, token: KeyScopeToken) {
        if self.open == Some(token) {
            self.open = None;
            self.delivered = None;
        }
    }

    pub(crate) fn note_commit(&mut self, text: &str) -> CommitVerdict {
        if self.open.is_none() {
            return CommitVerdict::DeliverOutsideScope;
        }
        let utf16: Vec<u16> = text.encode_utf16().collect();
        if utf16.is_empty() {
            return CommitVerdict::DeliverInScope;
        }
        if self.delivered.as_deref() == Some(utf16.as_slice()) {
            return CommitVerdict::Duplicate;
        }
        self.delivered = Some(utf16);
        CommitVerdict::DeliverInScope
    }

    pub(crate) fn takes_text(&self, token: KeyScopeToken) -> bool {
        self.open == Some(token) && self.delivered.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClauseRanges, NS_NOT_FOUND, clause_ranges, highlight_runs, key_visible, preedit_carets,
        preedit_clauses, replacement_range,
    };

    const PLAIN: u8 = 0x00;

    #[test]
    fn highlight_runs_merges_adjacent_units_of_the_same_kind() {
        let attrs = [PLAIN, PLAIN, 0x01, 0x01, PLAIN];

        assert_eq!(
            highlight_runs(&attrs, PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![2..4]
        );
    }

    #[test]
    fn highlight_runs_ignores_the_plain_attribute_and_keeps_edge_runs() {
        assert!(highlight_runs(&[PLAIN, PLAIN, PLAIN], PLAIN).is_empty());
        assert_eq!(
            highlight_runs(&[0x02, PLAIN, 0x02, 0x02], PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..1, 2..4]
        );
        assert!(highlight_runs(&[], PLAIN).is_empty());
    }

    #[test]
    fn clause_ranges_drops_empty_and_non_monotonic_offsets() {
        assert_eq!(
            clause_ranges(&[0, 0, 2, 2, 4], 4)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..2, 2..4]
        );
        assert_eq!(
            clause_ranges(&[0, 3, 1, 4], 4)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..3, 3..4]
        );
    }

    #[test]
    fn clause_ranges_clamps_to_the_composition_length() {
        assert_eq!(
            clause_ranges(&[0, 2, 99], 3)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..2, 2..3]
        );
        assert!(clause_ranges(&[], 3).is_empty());
    }

    #[test]
    fn preedit_clauses_prefers_attributes_over_clause_boundaries() {
        let attrs = [PLAIN, 0x01, PLAIN];
        let offsets = [0, 3];

        assert_eq!(
            preedit_clauses(Some(&attrs), Some(&offsets), 3, PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1..2]
        );
        assert_eq!(
            preedit_clauses(None, Some(&offsets), 3, PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..3]
        );
        assert!(preedit_clauses(None, None, 3, PLAIN).is_empty());
        assert!(preedit_clauses(Some(&[]), Some(&[]), 3, PLAIN).is_empty());
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn ns_not_found_means_replace_the_current_marked_text() {
        assert_eq!(NS_NOT_FOUND, isize::MAX as usize);
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &utf16("ni"), 0), None);
        assert_eq!(replacement_range(NS_NOT_FOUND, 3, &utf16("你好"), 0), None);
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &[], 0), None);
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
        assert_eq!(replacement_range(0, 0, &[], 0), Some(0..0));
        assert_eq!(replacement_range(3, 2, &[], 0), Some(0..0));
    }

    #[test]
    fn document_coordinates_are_rebased_onto_the_marked_text_start() {
        let marked = utf16("shili");
        assert_eq!(replacement_range(7, 1, &marked, 5), Some(2..3));
        assert_eq!(replacement_range(5, 2, &marked, 5), Some(0..2));
        assert_eq!(replacement_range(3, 0, &marked, 5), Some(0..0));
        assert_eq!(replacement_range(0, 0, &marked, 5), Some(0..0));
        assert_eq!(replacement_range(8, 99, &marked, 5), Some(3..5));
        assert_eq!(replacement_range(12, 0, &marked, 5), Some(5..5));
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &marked, 5), None);
    }

    #[test]
    fn cjk_ranges_are_counted_in_utf16_units() {
        let marked = utf16("你好");
        assert_eq!(marked.len(), 2);
        assert_eq!(replacement_range(0, 1, &marked, 0), Some(0..1));
        assert_eq!(replacement_range(1, 1, &marked, 0), Some(1..2));
        assert_eq!(replacement_range(2, 0, &marked, 0), Some(2..2));
    }

    #[test]
    fn surrogate_pairs_are_never_split() {
        let marked = utf16("🌍a");
        assert_eq!(marked.len(), 3);

        assert_eq!(replacement_range(1, 0, &marked, 0), Some(0..2));
        assert_eq!(replacement_range(0, 1, &marked, 0), Some(0..2));
        assert_eq!(replacement_range(0, 2, &marked, 0), Some(0..2));
        assert_eq!(replacement_range(2, 1, &marked, 0), Some(2..3));
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
    fn preedit_caret_is_counted_in_utf16_units() {
        assert_eq!(preedit_carets("ni", 0, 0), (Some(0), ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", 1, 1), (Some(1), ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", 2, 2), (Some(2), ClauseRanges::new()));
    }

    #[test]
    fn preedit_caret_converts_cjk_byte_offsets() {
        let (caret, clauses) = preedit_carets("你好", 0, 0);
        assert_eq!((caret, clauses), (Some(0), ClauseRanges::new()));
        assert_eq!(preedit_carets("你好", 3, 3).0, Some(1));
        assert_eq!(preedit_carets("你好", 6, 6).0, Some(2));
        assert_eq!(preedit_carets("你好", 4, 4).0, Some(1));
    }

    #[test]
    fn preedit_caret_handles_astral_characters() {
        assert_eq!(preedit_carets("🌍a", 4, 4).0, Some(2));
        assert_eq!(preedit_carets("🌍a", 5, 5).0, Some(3));
        assert_eq!(preedit_carets("🌍a", 2, 2).0, Some(0));
    }

    #[test]
    fn a_selected_preedit_range_becomes_a_clause() {
        let (caret, clauses) = preedit_carets("你好", 3, 6);
        assert_eq!(caret, Some(1), "插入点在选区起点");
        assert_eq!(
            clauses.into_iter().collect::<Vec<_>>(),
            vec![1..2],
            "高亮区间是 UTF-16 码元区间"
        );
        let (caret, clauses) = preedit_carets("ni", 2, 1);
        assert_eq!(caret, Some(2));
        assert!(clauses.is_empty());
    }

    #[test]
    fn a_hidden_cursor_yields_no_caret() {
        assert_eq!(preedit_carets("ni", -1, -1), (None, ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", -1, 2), (None, ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", 0, -1), (None, ClauseRanges::new()));
    }

    #[test]
    fn out_of_range_preedit_offsets_are_clamped() {
        assert_eq!(preedit_carets("ni", 99, 99).0, Some(2));
        assert_eq!(preedit_carets("ni", i32::MAX, i32::MAX).0, Some(2));
        assert_eq!(preedit_carets("", 0, 0), (Some(0), ClauseRanges::new()));
    }

    #[test]
    fn composing_without_command_modifiers_hides_the_bare_key() {
        assert!(!key_visible(false, true, false));
        assert!(!key_visible(true, true, false));
    }

    #[test]
    fn composing_with_a_command_modifier_still_delivers() {
        assert!(key_visible(true, true, true));
        assert!(key_visible(false, true, true));
    }

    #[test]
    fn an_ime_consumed_key_is_hidden_even_without_marked_text() {
        assert!(!key_visible(true, false, false));
        assert!(key_visible(true, false, true));
    }

    #[test]
    fn an_ordinary_key_is_visible() {
        assert!(key_visible(false, false, false));
    }
}

#[cfg(test)]
mod end_bookkeeping_tests {
    use super::ImeCompositionEnd;

    #[test]
    fn a_synchronous_unmark_during_discard_emits_exactly_one_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end(), "unmarkText: 回调应当投递 End");
        assert!(!end.note_end(), "清场兜底不得再补第二条 End");
        assert!(!end.is_pending());
    }

    #[test]
    fn a_discard_that_only_clears_the_marked_text_still_emits_one_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.is_pending());
        assert!(end.note_end(), "清场必须补一条 End（否则 preedit 残留）");
        assert!(!end.note_end(), "补过之后不得再补");
    }

    #[test]
    fn a_late_unmark_after_the_teardown_fallback_is_swallowed() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end(), "discardMarkedText 返回时还没收尾 ⇒ 补一条");
        assert!(!end.note_end(), "迟到的 unmarkText: 不得再投第二条 End");
    }

    #[test]
    fn a_commit_of_a_composition_is_followed_by_exactly_one_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_commit(), "有组合串的提交之后必须补一条 End");
        assert!(
            !end.note_end(),
            "补过之后，迟到的 unmarkText / 清场不得再补"
        );
        assert!(!end.is_pending());
    }

    #[test]
    fn a_late_unmark_after_a_commit_is_swallowed() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_commit(), "Commit 后补一条 End");
        assert!(!end.note_end(), "提交后的迟到 unmarkText: 被吞");
    }

    #[test]
    fn a_commit_after_the_teardown_fallback_does_not_emit_a_second_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end(), "清场兜底补一条");
        assert!(
            !end.note_commit(),
            "迟到的 Commit 不得再补 End（正文由 Commit 自身交付）"
        );
    }

    #[test]
    fn a_commit_without_a_preedit_needs_no_end() {
        let mut end = ImeCompositionEnd::default();
        assert!(!end.note_commit(), "没有组合串的提交不欠 End");
        assert!(!end.is_pending());
    }

    #[test]
    fn a_new_composition_gets_its_own_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end());
        end.note_preedit();
        assert!(end.note_end(), "下一次组合仍要恰好一条 End");
    }

    #[test]
    fn an_unmark_without_a_preedit_never_emits_an_end() {
        let mut end = ImeCompositionEnd::default();
        assert!(!end.note_end());
        assert!(!end.note_end());
    }
}

#[cfg(test)]
mod scope_tests {
    use super::{CommitVerdict, ImeCommitScope};

    #[test]
    fn a_commit_outside_a_key_never_suppresses_the_next_key() {
        let mut scope = ImeCommitScope::default();

        assert_eq!(scope.note_commit("你"), CommitVerdict::DeliverOutsideScope);

        let token = scope.begin_key();
        assert!(!scope.takes_text(token), "作用域外提交不得影响本按键");
        scope.end_key(token);
    }

    #[test]
    fn a_commit_inside_the_key_suppresses_that_keys_text() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("ni"), CommitVerdict::DeliverInScope);
        assert!(scope.takes_text(token));
        scope.end_key(token);
    }

    #[test]
    fn a_stale_token_never_sees_or_sets_suppression() {
        let mut scope = ImeCommitScope::default();
        let first = scope.begin_key();
        assert_eq!(scope.note_commit("ni"), CommitVerdict::DeliverInScope);
        scope.end_key(first);

        assert!(!scope.takes_text(first));
        let second = scope.begin_key();
        scope.end_key(first);
        assert!(scope.open == Some(second), "旧令牌的收尾不得关闭新作用域");
        assert!(!scope.takes_text(second), "新作用域是干净的");
        scope.end_key(second);
    }

    #[test]
    fn a_duplicate_commit_within_one_key_is_swallowed() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("你"), CommitVerdict::DeliverInScope);
        assert_eq!(scope.note_commit("你"), CommitVerdict::Duplicate);
        assert!(scope.takes_text(token));
        scope.end_key(token);
    }

    #[test]
    fn the_same_text_in_the_next_key_is_delivered_again() {
        let mut scope = ImeCommitScope::default();
        for _ in 0..3 {
            let token = scope.begin_key();
            assert_eq!(scope.note_commit("l"), CommitVerdict::DeliverInScope);
            scope.end_key(token);
        }
    }

    #[test]
    fn different_texts_in_one_key_are_both_delivered() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("a"), CommitVerdict::DeliverInScope);
        assert_eq!(scope.note_commit("b"), CommitVerdict::DeliverInScope);
        assert!(scope.takes_text(token));
        scope.end_key(token);
    }

    #[test]
    fn an_empty_commit_does_not_suppress_text() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit(""), CommitVerdict::DeliverInScope);
        assert!(!scope.takes_text(token));
        scope.end_key(token);
    }

    #[test]
    fn commits_are_compared_in_utf16_units() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("🌍"), CommitVerdict::DeliverInScope);
        assert_eq!(scope.note_commit("🌍"), CommitVerdict::Duplicate);
        assert_eq!(scope.note_commit("🌍a"), CommitVerdict::DeliverInScope);
        scope.end_key(token);
    }
}
