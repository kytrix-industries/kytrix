//! IME 批次状态：`zwp_text_input_v3` 的双缓冲（纯逻辑，Windows 可单测）。
//!
//! `preedit_string` / `commit_string` / `delete_surrounding_text` 可能在同一批里到达，
//! **只在 `done` 时原子生效**（否则 UI 会看到半状态）。XML `done` 规定求值顺序：
//!
//! 1. 用光标替换已有组合串；2. 删除请求的周围文本；3. 在光标处插入提交串；
//! 4. 计算要发送的 surrounding text；5. 在光标处插入新组合串；6. 定位组合串内光标；7. 动作。
//!
//! 本结构负责 1/3/5/6 里**引擎已有通路**的部分（组合串、提交串、组合串内插入点与高亮区间）；
//! `delete_surrounding_text` 依赖 `set_surrounding_text` 通路，而后者需要 `ImeEvent` 提供
//! 「删除周围文本」的变体（由输入波次维护 `crate::event::ImeEvent`），当前未接线 —— 收到时
//! 忽略（见 `surface.rs`）。
//!
//! **本平台不做提交去重**（AR1b P3-1）：协议按 `done` 双缓冲交付，一批只有一个
//! `commit_string`；跨批的相同文本（`ll`、双空格）是用户的真实输入，必须放行。旧的
//! `CommitDeduper` 在 Linux 上没有消费者（`begin_key` + `note` 恒为真），已删除。
//!
//! # 为什么在 `platform/wayland/`（B9）
//!
//! 本结构是纯状态机（只依赖 `ImeEvent` / `SmolStr` / 平台无关的 `preedit_carets`），但它原本
//! 放在 Linux-only 的 `platform/linux/ime.rs` 里，开发机（Windows）的 `cargo test` 覆盖不到。
//! B9 要求「IME 在组合中被 disable 时重置状态、并**恰好补一条** `ImeEvent::End`」，
//! 这条清场判定必须有单测兜底（否则「组合残留 / 补两条 End」这类缺陷只能靠真机 IME 观察），
//! 因此与其它 Wayland 纯逻辑一起放到这里：`platform::wayland` 整体在
//! `cfg(any(target_os = "linux", test))` 下编译，Linux 驱动层（`platform/linux/surface.rs`）
//! 只是消费者。

use smol_str::SmolStr;

use crate::{
    event::ImeEvent,
    platform::ime::{ClauseRanges, preedit_carets},
};

/// 一批 `done` 里的组合串：文本 + 串内插入点 + 高亮子句（**双缓冲**，`done` 时原子生效）。
///
/// 协议规定 `text` / `cursor_begin` / `cursor_end` 都按 `done` 批次重置为初值，因此三者必须
/// 一起缓存：只存文本会让光标停在上一批的位置（AR1b N4）。
#[derive(Debug, Default)]
struct PendingPreedit {
    /// `preedit_string` 的 `text`（`None` = 协议发了 `null` 串）。
    text: Option<String>,
    /// 串内插入点（UTF-16 码元，已由字节偏移换算）。
    caret: Option<u32>,
    /// 高亮区间（`cursor_begin != cursor_end` 时的选区）。
    clauses: ClauseRanges,
}

/// 一个窗口的 IME 状态（主线程独占；由 `WindowSurface` 持有，**每窗口一份**，R10-6）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// `done` 之前的 preedit（`None` = 本批没有 preedit 事件）。
    pending_preedit: Option<PendingPreedit>,
    /// `done` 之前的 commit。
    pending_commit: Option<String>,
    /// 当前是否处于组合态。
    composing: bool,
    /// 自上次补 `End` 以来**曾经**有过非空组合串（R10-35 的判据）。
    ///
    /// 不能只看 `composing`：`commit_string` 与 `preedit_string("")` 可能在**同一批**里到达，
    /// 而提交分支会先把 `composing` 清掉；只有这个 latch 才能保证「组合在本批结束」仍补一条
    /// [`ImeEvent::End`]，且**恰好一条**。
    had_preedit: bool,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 记录一条 `preedit_string`（**不立即生效**，等 `done`）。
    ///
    /// `cursor_begin` / `cursor_end` 是协议给的**字节偏移**（`-1` = 隐藏光标），在这里换成
    /// 引擎的 UTF-16 码元单位并缓存；`done` 时与文本一起交付（AR1b N4：旧实现只取文本，
    /// 协议已经给出的插入点被静默丢弃，Linux 的 preedit 光标恒在末尾）。
    pub(crate) fn note_preedit(
        &mut self,
        text: Option<String>,
        cursor_begin: i32,
        cursor_end: i32,
    ) {
        // 组合态在**收到** preedit 时即生效（不等 `done`）：键盘通路据此静默，
        // 而组合期按键可能先于 `done` 到达。
        let (caret, clauses) = match text.as_deref() {
            Some(text) if !text.is_empty() => {
                self.composing = true;
                self.had_preedit = true;
                preedit_carets(text, cursor_begin, cursor_end)
            }
            _ => {
                self.composing = false;
                (None, ClauseRanges::new())
            }
        };
        self.pending_preedit = Some(PendingPreedit {
            text,
            caret,
            clauses,
        });
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

    /// 因 IME 被**停用**而清场：丢弃批次与组合态，并报告是否需要给应用补一条
    /// [`ImeEvent::End`]（B9）。
    ///
    /// 返回 `true` ⇔ 停用前确实存在组合串（`composing` 或本批之前的 `had_preedit`）——
    /// 此时应用界面上的 preedit 必须被显式结束，否则会永久残留；没有组合时返回 `false`
    /// （不得凭空补 `End`）。清场是**破坏性**的：同一次停用只能调用一次，调用方按
    /// 「disable 只在收敛时出现一次」保证 `End` 恰好一条。
    pub(crate) fn reset_ending(&mut self) -> bool {
        let had_composition = self.composing || self.had_preedit;
        self.reset();
        had_composition
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
            events.push(ImeEvent::Commit {
                text: SmolStr::new(commit.as_str()),
                replace: None,
            });
            self.composing = false;
        }
        match self.pending_preedit.take() {
            Some(PendingPreedit {
                text: Some(text),
                caret,
                clauses,
            }) if !text.is_empty() => {
                self.composing = true;
                self.had_preedit = true;
                // `zwp_text_input_v3` 的 preedit 是整串替换，没有 AppKit 那种区间替换；
                // 串内插入点与高亮区间来自 `preedit_string` 的 `cursor_begin/end`（字节偏移，
                // 已在该事件的 `note_preedit` 里换算成 UTF-16 码元）。
                events.push(ImeEvent::Preedit {
                    text: SmolStr::new(text),
                    caret,
                    clauses,
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

    fn preedit(events: &[ImeEvent]) -> Option<&ImeEvent> {
        events
            .iter()
            .find(|event| matches!(event, ImeEvent::Preedit { .. }))
    }

    /// 组合态里收到提交：本批产出 Commit，且组合态结束。
    #[test]
    fn a_commit_ends_the_composition() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()), 2, 2);
        let events = ime.take_events();
        assert!(matches!(events.as_slice(), [ImeEvent::Preedit { .. }]));
        assert!(ime.composing());

        ime.note_preedit(None, -1, -1);
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
        ime.note_preedit(Some("ni".to_owned()), 2, 2);
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
        ime.note_preedit(Some("ni".to_owned()), 0, 0);
        assert!(ime.composing());
        ime.note_preedit(None, -1, -1);
        let events = ime.take_events();
        assert!(events.contains(&ImeEvent::End));
        assert!(!ime.composing());
        ime.note_preedit(None, -1, -1);
        assert!(ime.take_events().is_empty(), "没有状态变化时不出事件");
    }

    #[test]
    fn reset_drops_the_pending_batch_and_the_composition() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()), 2, 2);
        let _ = ime.take_events();
        ime.note_commit(Some("你".to_owned()));
        ime.reset();
        assert!(!ime.composing());
        assert!(ime.take_events().is_empty(), "reset 之后旧批次不得再生效");
    }

    /// B9：IME 在组合中被**停用**（应用 `set_ime_enabled(false)` / 焦点移到非编辑控件）
    /// ⇒ 组合与待生效批次必须清空，并且恰好要求补一条 [`ImeEvent::End`]。
    ///
    /// 返回值的语义是「调用方需要补 End」：第一次停用为 `true`，清场之后再调为 `false` ——
    /// 调用方只在收敛点（disable 只出现一次）补，因此 `End` 至多一条。
    #[test]
    fn disabling_during_a_composition_resets_it_and_ends_it_exactly_once() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()), 2, 2);
        // 组合串已经上屏（组合态立即生效，应用侧画着 preedit）。
        let _ = ime.take_events();
        assert!(ime.composing());

        assert!(
            ime.reset_ending(),
            "组合中停用 ⇒ 必须让调用方补一条 ImeEvent::End"
        );
        assert!(
            !ime.composing(),
            "停用后组合态必须清空（否则键盘文本被永久抑制）"
        );
        assert!(ime.take_events().is_empty(), "待生效批次随停用一起丢弃");
        assert!(
            !ime.reset_ending(),
            "已清场 ⇒ 不得再补 End（同一次停用只能一条）"
        );
    }

    /// 组合批次**尚未** `done`（还在双缓冲里）时停用：同样丢弃并补 End。
    #[test]
    fn disabling_with_a_pending_batch_drops_it_and_ends_once() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("nihao".to_owned()), 5, 5);
        assert!(ime.composing());

        assert!(ime.reset_ending());
        assert!(ime.take_events().is_empty(), "未生效的批次不得在停用后交付");
        assert!(!ime.reset_ending());
    }

    /// 正常批次已经交付过 `End`（commit-only 批次会显式结束组合）：随后停用不得重复补。
    #[test]
    fn an_ime_end_already_delivered_is_not_duplicated_on_disable() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()), 2, 2);
        let _ = ime.take_events();
        ime.note_commit(Some("你".to_owned()));
        let events = ime.take_events();
        assert!(
            events.contains(&ImeEvent::End),
            "commit-only 批次必须先结束组合: {events:?}"
        );

        assert!(
            !ime.reset_ending(),
            "应用已经收到 End ⇒ 停用时不得再补第二条"
        );
    }

    /// AR1b N4（P2 回归）：协议给的 `cursor_begin` / `cursor_end` 必须进
    /// [`ImeEvent::Preedit`]（插入点 + 高亮区间），不得静默丢弃。
    #[test]
    fn the_protocol_cursor_positions_reach_the_preedit_event() {
        let mut ime = ImeState::new();
        // "nihao" 的插入点在 3 号字节（UTF-16 码元 3）；没有选区。
        ime.note_preedit(Some("nihao".to_owned()), 3, 3);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit {
            caret,
            clauses,
            text,
            ..
        }) = preedit(&events)
        else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(text.as_str(), "nihao");
        assert_eq!(*caret, Some(3), "插入点来自 cursor_begin");
        assert!(clauses.is_empty(), "两者相等 ⇒ 没有高亮区间");

        // 选区：cursor_begin=1、cursor_end=4 ⇒ 高亮 [1,4)。
        ime.note_preedit(Some("nihao".to_owned()), 1, 4);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit { caret, clauses, .. }) = preedit(&events) else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(*caret, Some(1));
        assert_eq!(
            clauses.iter().cloned().collect::<Vec<_>>(),
            vec![1..4],
            "cursor_begin != cursor_end ⇒ 高亮区间"
        );
    }

    /// AR1b N4 的 Unicode 分支：中文 / emoji 的字节偏移必须先换算成 UTF-16 码元。
    #[test]
    fn the_protocol_cursor_is_converted_from_bytes_to_utf16_units() {
        let mut ime = ImeState::new();
        // "你好" = 6 字节 / 2 码元：字节 3 是第二个汉字之前 ⇒ 码元 1。
        ime.note_preedit(Some("你好".to_owned()), 3, 3);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit { caret, .. }) = preedit(&events) else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(*caret, Some(1));

        // "🌍a" = 5 字节 / 3 码元：字节 4 ⇒ 码元 2。
        ime.note_preedit(Some("🌍a".to_owned()), 4, 5);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit { caret, clauses, .. }) = preedit(&events) else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(*caret, Some(2));
        assert_eq!(clauses.iter().cloned().collect::<Vec<_>>(), vec![2..3]);
    }

    /// 协议「两者都是 -1」= 隐藏光标：引擎没有该状态，`caret = None`（在 `ImeEvent` 文档写明）。
    #[test]
    fn a_hidden_protocol_cursor_yields_a_none_caret() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()), -1, -1);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit { caret, clauses, .. }) = preedit(&events) else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(*caret, None);
        assert!(clauses.is_empty());
    }

    /// 跨批的相同文本必须都交付（AR1b P3-1：本平台不做提交去重）。
    #[test]
    fn identical_commits_across_batches_are_all_delivered() {
        let mut ime = ImeState::new();
        for _ in 0..2 {
            ime.note_commit(Some("ll".to_owned()));
            let events = ime.take_events();
            assert_eq!(
                commit_text(&events),
                Some("ll"),
                "跨批的相同文本是用户的真实输入"
            );
        }
    }
}
