//! 提交去重（平台无关）：连续相同字符 / 双空格是合法输入，只有**同一次按键内**的
//! 重复回调才吞掉（`design/phase-12-macos-linux.md` §7.5，P14 终审项 1）。
//!
//! 放在平台无关层是为了让这段纯逻辑能在开发机（Windows）上被单测覆盖：
//! macOS 的 `NSTextInputClient` 包装器只在 macOS 目标上编译，测试跑不到。

use smol_str::SmolStr;

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
    use super::CommitDeduper;

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
