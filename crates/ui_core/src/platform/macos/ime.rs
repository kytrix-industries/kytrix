//! IME 状态（`NSTextInputClient` 的组合串与提交去重，`design/phase-12-macos-linux.md` §7.5）。
//!
//! `NSRange` 是 UTF-16 单位，因此组合串与选中范围都按 UTF-16 记录（提交时一次性转 `SmolStr`）。
//! 提交去重的纯逻辑在平台无关的 [`crate::platform::ime::CommitDeduper`]（可在 Windows 上单测）。

use crate::platform::ime::CommitDeduper;

/// 一个窗口的 IME 状态（`GuiView` 的 ivars）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// 组合中的文本（UTF-16 码元；空 = 没有组合）。
    marked: Vec<u16>,
    /// 组合串内的选中范围（`.location` / `.length`，UTF-16）。
    selected: (usize, usize),
    /// 「一次按键」粒度的提交去重。
    dedupe: CommitDeduper,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn has_marked(&self) -> bool {
        !self.marked.is_empty()
    }

    pub(crate) fn marked_range(&self) -> (usize, usize) {
        (0, self.marked.len())
    }

    pub(crate) fn selected_range(&self) -> (usize, usize) {
        self.selected
    }

    /// 一次新的按键开始：清掉「同一按键」的提交记忆（跨按键的相同文本是合法的）。
    pub(crate) fn begin_key(&mut self) {
        self.dedupe.begin_key();
    }

    /// `setMarkedText:selectedRange:replacementRange:`（`text` 已是 UTF-16）。
    pub(crate) fn set_marked(&mut self, text: &[u16], selected: (usize, usize)) {
        self.marked.clear();
        self.marked.extend_from_slice(text);
        let len = self.marked.len();
        let location = selected.0.min(len);
        let length = selected.1.min(len - location);
        self.selected = (location, length);
        // 组合态变化意味着「下一次提交」是新的内容。
        self.dedupe.reset();
    }

    pub(crate) fn clear_marked(&mut self) {
        self.marked.clear();
        self.selected = (0, 0);
    }

    /// 记录一次提交；返回 `false` 表示**同一次按键内**的重复回调（调用方应吞掉）。
    pub(crate) fn note_commit(&mut self, text: &str) -> bool {
        self.dedupe.note(text)
    }
}
