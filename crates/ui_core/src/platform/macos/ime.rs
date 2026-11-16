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

    /// 当前组合串在 AppKit **文档坐标**下的范围 `(location, length)`（UTF-16 码元）。
    ///
    /// `location` 是 `markedRange` 回给 AppKit 的起点，也是 `replacementRange` 换算
    /// （`platform::ime::replacement_range`）的基准：两者必须处于同一文档坐标系，且该值必须
    /// 与引擎真实文本里组合串的起点一致。本 view 还没有 surrounding text（文本存储只覆盖
    /// 组合串），因此起点恒为 0；将来实现 `attributedSubstringForProposedRange:` 时在这里
    /// 返回真实起点即可，`markedRange` 回包与 `replacementRange` 换算都不用改。
    pub(crate) fn marked_range(&self) -> (usize, usize) {
        (0, self.marked.len())
    }

    /// 当前组合串（UTF-16 码元）：`replacementRange` 换算的 clamp 上界 + 代理对边界吸附来源。
    pub(crate) fn marked_utf16(&self) -> &[u16] {
        &self.marked
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

/// `NSNotFound` 哨兵绑定校验（只在 macOS 目标编译：`objc2-foundation` 是 macOS 依赖）。
///
/// 引擎的 [`NS_NOT_FOUND`](crate::platform::ime::NS_NOT_FOUND) 必须与 AppKit 绑定的真实值
/// 完全一致 —— `NSNotFound == NSIntegerMax`（`isize::MAX`）。写错成 `usize::MAX` 会让真实
/// 输入法的正常路径 `{NSNotFound, 0}` 不匹配、退化成「在组合串尾部插入」（第三轮复审 P1）。
/// Windows 开发机跑不到本测试，由 `platform/ime.rs` 的单测覆盖常量值本身。
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use crate::platform::ime::NS_NOT_FOUND;

    #[test]
    fn ns_not_found_matches_the_objc2_foundation_binding() {
        // 绑定侧：`objc2_foundation` 的 `NSNotFound` 是 `NSIntegerMax`（不是 `NSUInteger::MAX`）。
        assert_eq!(objc2_foundation::NSNotFound as usize, isize::MAX as usize);
        // 引擎侧：`replacementRange` 的 `{NSNotFound, 0}` 判断必须用同一个值。
        assert_eq!(NS_NOT_FOUND, objc2_foundation::NSNotFound as usize);
    }
}
