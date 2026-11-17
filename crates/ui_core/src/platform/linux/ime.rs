//! IME 批次状态：`zwp_text_input_v3` 的双缓冲（`design/phase-12-macos-linux.md` §8.5 / F-128）。
//!
//! `preedit_string` / `commit_string` / `delete_surrounding_text` 可能在同一批里到达，
//! **只在 `done` 时原子生效**（否则 UI 会看到半状态）。XML `done` 规定求值顺序：
//!
//! 1. 用光标替换已有组合串；2. 删除请求的周围文本；3. 在光标处插入提交串；
//! 4. 计算要发送的 surrounding text；5. 在光标处插入新组合串；6. 定位组合串内光标；7. 动作。
//!
//! 本结构只负责 1/3/5/6 里**引擎已有通路**的部分（组合串与提交串）；`delete_surrounding_text`
//! 依赖 `set_surrounding_text` 通路，而后者需要 `ImeEvent` 提供「删除周围文本」的变体
//! （由输入波次维护 `crate::event::ImeEvent`），当前未接线 —— 收到时忽略（见 `surface.rs`）。

use smol_str::SmolStr;

use crate::{event::ImeEvent, platform::ime::CommitDeduper};

/// 一个窗口的 IME 状态（主线程独占；由 `WindowSurface` 持有，**每窗口一份**，R10-6）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// `done` 之前的 preedit（`None` = 本批没有 preedit 事件）。
    pending_preedit: Option<Option<String>>,
    /// `done` 之前的 commit。
    pending_commit: Option<String>,
    /// 提交去重（同一批 `done` 内的重复回调才吞；跨批的相同文本是合法输入）。
    dedupe: CommitDeduper,
    /// 当前是否处于组合态。
    composing: bool,
    /// 自上次补 `End` 以来**曾经**有过非空组合串（R10-35 的判据）。
    ///
    /// 不能只看 `composing`：`commit_string` 与 `preedit_string("")` 可能在**同一批**里到达，
    /// 而提交分支会先把 `composing` 清掉；只有这个latch 才能保证「组合在本批结束」仍补一条
    /// [`ImeEvent::End`]，且**恰好一条**。
    had_preedit: bool,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn note_preedit(&mut self, text: Option<String>) {
        // 组合态在**收到** preedit 时即生效（不等 `done`）：键盘通路据此静默，
        // 而组合期按键可能先于 `done` 到达。
        match text.as_deref() {
            Some(text) if !text.is_empty() => {
                self.composing = true;
                self.had_preedit = true;
            }
            _ => self.composing = false,
        }
        self.pending_preedit = Some(text);
        // 新的组合串开始 ⇒ 下一次提交是新的内容。
        self.dedupe.reset();
    }

    pub(crate) fn note_commit(&mut self, text: Option<String>) {
        self.pending_commit = text;
    }

    /// 当前是否处于组合态（键盘通路据此静默：组合串由 [`ImeEvent::Preedit`] 表达，
    /// 组合期的物理按键不产生正文）。
    pub(crate) fn composing(&self) -> bool {
        self.composing
    }

    /// 丢弃当前组合与待生效批次（IME 被停用 / `leave` 时调用）。
    ///
    /// 组合态必须一起丢掉：回到键盘通路后 `composing` 若仍为真，文本会被永久抑制
    /// （与 R02-2 的 P0 回归同一类失效）。XML `leave`：*The client should reset any preedit
    /// string previously set*。
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// `done`：把本批的事件原子转换成 `ImeEvent`（按 XML 的顺序：commit → preedit）。
    ///
    /// R10-35：**commit-only 批次必须显式结束组合**。XML 的求值顺序第 1 步就是「用光标替换
    /// 已有组合串」，因此「本批只有 `commit_string`」意味着组合串已被清除；不发
    /// [`ImeEvent::End`] 会让 App 侧的组合串残留在界面上。
    pub(crate) fn take_events(&mut self) -> Vec<ImeEvent> {
        let mut events = Vec::with_capacity(3);
        // 「本批开始时组合串是否存在」：提交分支会先清 `composing`，所以必须提前取，
        // 且要看 `had_preedit`（同批 `preedit_string("")` 会连 `composing` 一起清掉）。
        let was_composing = self.composing || self.had_preedit;
        let committed = self.pending_commit.take();
        if let Some(commit) = committed.as_ref() {
            // 一次 `done` = 一批提交；批内的重复才吞掉（部分 compositor 会重复发）。
            self.dedupe.begin_key();
            if self.dedupe.note(commit.as_str()) {
                events.push(ImeEvent::Commit {
                    text: SmolStr::new(commit.as_str()),
                    replace: None,
                });
            }
            self.composing = false;
        }
        match self.pending_preedit.take() {
            Some(Some(text)) if !text.is_empty() => {
                self.composing = true;
                self.had_preedit = true;
                // `zwp_text_input_v3` 的 preedit 是整串替换，没有 AppKit 那种区间替换；
                // 组合串内的插入点与子句信息由输入波次定义（`caret` / `clauses`），本平台
                // 先按「插入点在末尾、无子句」交付。
                events.push(ImeEvent::Preedit {
                    text: SmolStr::new(text),
                    caret: None,
                    clauses: smallvec::SmallVec::new(),
                    replace: None,
                });
            }
            Some(_) => {
                // 本批显式清空组合串：组合态结束，补一条 `End`（只补一次）。
                self.composing = false;
                if was_composing {
                    self.had_preedit = false;
                    events.push(ImeEvent::End);
                }
            }
            None => {
                // 组合态被提交结束、且本批没有新的 preedit ⇒ 补一条 `End`（R10-35）。
                if was_composing && committed.is_some() {
                    self.had_preedit = false;
                    events.push(ImeEvent::End);
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::ImeState;
    use crate::event::ImeEvent;

    fn commit_text(events: &[ImeEvent]) -> Option<&str> {
        events.iter().find_map(|event| match event {
            ImeEvent::Commit { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// 组合态里收到提交：本批产出 Commit，且组合态结束。
    #[test]
    fn a_commit_ends_the_composition() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()));
        let events = ime.take_events();
        assert!(matches!(events.as_slice(), [ImeEvent::Preedit { .. }]));
        assert!(ime.composing());

        ime.note_preedit(None);
        ime.note_commit(Some("你".to_owned()));
        let events = ime.take_events();
        assert_eq!(commit_text(&events), Some("你"));
        assert!(
            events.contains(&ImeEvent::End),
            "组合结束必须显式通知: {events:?}"
        );
        assert!(!ime.composing());
    }

    /// R10-35：只有 `commit_string` 的批次也要补 `End`（否则组合串残留）。
    #[test]
    fn a_commit_only_batch_still_ends_the_composition() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()));
        let _ = ime.take_events();
        assert!(ime.composing());

        ime.note_commit(Some("你".to_owned()));
        let events = ime.take_events();
        assert_eq!(commit_text(&events), Some("你"));
        assert!(
            events.contains(&ImeEvent::End),
            "commit-only 批次必须补 End（R10-35）: {events:?}"
        );
    }

    /// 从未组合过时，提交批次不产生多余的 `End`。
    #[test]
    fn a_plain_commit_does_not_emit_an_end() {
        let mut ime = ImeState::new();
        ime.note_commit(Some("a".to_owned()));
        let events = ime.take_events();
        assert_eq!(commit_text(&events), Some("a"));
        assert!(!events.contains(&ImeEvent::End), "{events:?}");
    }

    /// 整串替换：空 preedit 结束组合并产出一条 `End`（不重复）。
    #[test]
    fn an_empty_preedit_ends_the_composition_once() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()));
        assert!(ime.composing());
        ime.note_preedit(None);
        let events = ime.take_events();
        assert!(events.contains(&ImeEvent::End));
        assert!(!ime.composing());
        ime.note_preedit(None);
        assert!(ime.take_events().is_empty(), "没有状态变化时不出事件");
    }

    #[test]
    fn reset_drops_the_pending_batch_and_the_composition() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()));
        let _ = ime.take_events();
        ime.note_commit(Some("你".to_owned()));
        ime.reset();
        assert!(!ime.composing());
        assert!(ime.take_events().is_empty(), "reset 之后旧批次不得再生效");
    }

    /// 同一批里重复的 `commit_string`（部分 compositor 会发两次）只交付一次；跨批的相同
    /// 文本是合法输入（例如连打两个 `l`）。
    #[test]
    fn identical_commits_within_one_batch_are_deduped() {
        let mut ime = ImeState::new();
        ime.note_commit(Some("ll".to_owned()));
        let first = ime.take_events();
        assert_eq!(commit_text(&first), Some("ll"));
        ime.dedupe.begin_key();
        ime.note_commit(Some("ll".to_owned()));
        let second = ime.take_events();
        assert_eq!(commit_text(&second), Some("ll"), "跨批的相同文本必须交付");
    }
}
