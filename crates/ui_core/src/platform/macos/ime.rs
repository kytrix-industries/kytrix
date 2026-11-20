//! IME 状态（`NSTextInputClient` 的组合串 + 按键作用域的提交记账）。
//!
//! `NSRange` 是 UTF-16 单位，因此组合串与选中范围都按 UTF-16 记录（提交时一次性转 `SmolStr`）。
//! 提交去重与 `Text` 抑制的作用域记账在平台无关的
//! [`crate::platform::ime::ImeCommitScope`] 里（可在开发机上单测）。

use crate::platform::ime::{CommitVerdict, ImeCommitScope, ImeCompositionEnd, KeyScopeToken};

/// 一个窗口的 IME 状态（`GuiView` 的 ivars）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// 组合中的文本（UTF-16 码元；空 = 没有组合）。
    marked: Vec<u16>,
    /// 组合串内的选中范围（`.location` / `.length`，UTF-16）。
    selected: (usize, usize),
    /// 按键作用域的提交记账（R09-3）：本按键的正文是否已由 IME 交付。
    scope: ImeCommitScope,
    /// 「恰好一条 `End`」的收尾记账（RB-09）：应用收到过 `Preedit` 就欠一条收尾，
    /// `End`/`Commit` 谁先到谁销账。
    end: ImeCompositionEnd,
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

    /// 打开一次按键作用域（`keyDown:` 入口），返回用于后续查询/收尾的令牌。
    pub(crate) fn begin_key(&mut self) -> KeyScopeToken {
        self.scope.begin_key()
    }

    /// 关闭按键作用域：清空本按键的全部记账（RAII 守卫调用，panic 也会走到）。
    pub(crate) fn end_key(&mut self, token: KeyScopeToken) {
        self.scope.end_key(token);
    }

    /// 本作用域内是否已交付正文（`keyDown:` 据此抑制同一次按键的 `Text`）。
    pub(crate) fn takes_text(&self, token: KeyScopeToken) -> bool {
        self.scope.takes_text(token)
    }

    /// `setMarkedText:selectedRange:replacementRange:`（`text` 已是 UTF-16）。
    pub(crate) fn set_marked(&mut self, text: &[u16], selected: (usize, usize)) {
        self.marked.clear();
        self.marked.extend_from_slice(text);
        let len = self.marked.len();
        let location = selected.0.min(len);
        let length = selected.1.min(len - location);
        self.selected = (location, length);
        // RB-09：非空组合串已经作为 `Preedit` 交给应用 ⇒ 欠一条收尾。空串是 AppKit
        // 「清空组合」的形态（`setMarkedText:@""`），不算新的组合开始。
        if !self.marked.is_empty() {
            self.end.note_preedit();
        }
    }

    pub(crate) fn clear_marked(&mut self) {
        self.marked.clear();
        self.selected = (0, 0);
    }

    /// 记录一次提交并给出裁决（作用域内/外/重复，见 [`CommitVerdict`]）。
    ///
    /// 提交**不**动收尾记账（RBM-02）：有组合串的提交之后仍欠一条 `End`，由调用方在投完
    /// `Commit` 后用 [`Self::note_end`] 补（与 Win32 `WM_IME_ENDCOMPOSITION` / Wayland
    /// commit-only 批次一致）。
    pub(crate) fn note_commit(&mut self, text: &str) -> CommitVerdict {
        self.scope.note_commit(text)
    }

    /// 投递 `ImeEvent::End` 的决策（RB-09）：`true` = 现在该投（并销账）。
    pub(crate) fn note_end(&mut self) -> bool {
        self.end.note_end()
    }

    /// 提交路径的收尾决策（RBM-02）：有组合串的提交之后补一条 `End`（并销账）。
    pub(crate) fn note_commit_end(&mut self) -> bool {
        self.end.note_commit()
    }

    /// 是否欠一条 `End`（清场入口据此决定要不要 `discardMarkedText`）。
    pub(crate) fn has_pending_end(&self) -> bool {
        self.end.is_pending()
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
