//! IME 状态（`NSTextInputClient` 的组合串与提交去重，`design/phase-12-macos-linux.md` §7.5）。
//!
//! `NSRange` 是 UTF-16 单位，因此组合串与选中范围都按 UTF-16 记录（提交时一次性转 `SmolStr`）。

use smol_str::SmolStr;

/// 一个窗口的 IME 状态（`GuiView` 的 ivars）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// 组合中的文本（UTF-16 码元；空 = 没有组合）。
    marked: Vec<u16>,
    /// 组合串内的选中范围（`.location` / `.length`，UTF-16）。
    selected: (usize, usize),
    /// 最近一次提交（去重：`commit` 后紧随的字符键可能重复）。
    last_commit: SmolStr,
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

    /// `setMarkedText:selectedRange:replacementRange:`（`text` 已是 UTF-16）。
    pub(crate) fn set_marked(&mut self, text: &[u16], selected: (usize, usize)) {
        self.marked.clear();
        self.marked.extend_from_slice(text);
        let len = self.marked.len();
        let location = selected.0.min(len);
        let length = selected.1.min(len - location);
        self.selected = (location, length);
    }

    pub(crate) fn clear_marked(&mut self) {
        self.marked.clear();
        self.selected = (0, 0);
    }

    /// 记录一次提交；返回 `false` 表示与上一次提交相同（调用方应吞掉）。
    pub(crate) fn note_commit(&mut self, text: &str) -> bool {
        let next = SmolStr::new(text);
        let duplicate = !text.is_empty() && next == self.last_commit;
        self.last_commit = next;
        !duplicate
    }
}
