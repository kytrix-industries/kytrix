use smol_str::SmolStr;

use crate::{
    event::ImeEvent,
    platform::ime::{ClauseRanges, preedit_carets},
};

#[derive(Debug, Default)]
struct PendingPreedit {
    text: Option<String>,
    caret: Option<u32>,
    clauses: ClauseRanges,
}

#[derive(Debug, Default)]
pub(crate) struct ImeState {
    pending_preedit: Option<PendingPreedit>,
    pending_commit: Option<String>,
    composing: bool,
    had_preedit: bool,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn note_preedit(
        &mut self,
        text: Option<String>,
        cursor_begin: i32,
        cursor_end: i32,
    ) {
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

    pub(crate) fn composing(&self) -> bool {
        self.composing
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn reset_ending(&mut self) -> bool {
        let had_composition = self.composing || self.had_preedit;
        self.reset();
        had_composition
    }

    pub(crate) fn take_events(&mut self) -> Vec<ImeEvent> {
        let mut events = Vec::with_capacity(3);
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
                events.push(ImeEvent::Preedit {
                    text: SmolStr::new(text),
                    caret,
                    clauses,
                    replace: None,
                });
            }
            Some(_) => {
                self.composing = false;
                if was_composing {
                    self.had_preedit = false;
                    events.push(ImeEvent::End);
                }
            }
            None => {
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

    #[test]
    fn a_plain_commit_does_not_emit_an_end() {
        let mut ime = ImeState::new();
        ime.note_commit(Some("a".to_owned()));
        let events = ime.take_events();
        assert_eq!(commit_text(&events), Some("a"));
        assert!(!events.contains(&ImeEvent::End), "{events:?}");
    }

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

    #[test]
    fn disabling_during_a_composition_resets_it_and_ends_it_exactly_once() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("ni".to_owned()), 2, 2);
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

    #[test]
    fn disabling_with_a_pending_batch_drops_it_and_ends_once() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("nihao".to_owned()), 5, 5);
        assert!(ime.composing());

        assert!(ime.reset_ending());
        assert!(ime.take_events().is_empty(), "未生效的批次不得在停用后交付");
        assert!(!ime.reset_ending());
    }

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

    #[test]
    fn the_protocol_cursor_positions_reach_the_preedit_event() {
        let mut ime = ImeState::new();
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

    #[test]
    fn the_protocol_cursor_is_converted_from_bytes_to_utf16_units() {
        let mut ime = ImeState::new();
        ime.note_preedit(Some("你好".to_owned()), 3, 3);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit { caret, .. }) = preedit(&events) else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(*caret, Some(1));

        ime.note_preedit(Some("🌍a".to_owned()), 4, 5);
        let events = ime.take_events();
        let Some(ImeEvent::Preedit { caret, clauses, .. }) = preedit(&events) else {
            panic!("必须交付 preedit: {events:?}");
        };
        assert_eq!(*caret, Some(2));
        assert_eq!(clauses.iter().cloned().collect::<Vec<_>>(), vec![2..3]);
    }

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
