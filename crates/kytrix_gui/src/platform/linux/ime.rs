//! IME 状态机：`zwp_text_input_v3` 的双缓冲（`design/phase-12-macos-linux.md` §8.5 / F-128）。
//!
//! `preedit_string` / `commit_string` / `delete_surrounding_text` 可能在同一批里到达，
//! **只在 `done` 时原子生效**（否则 UI 会看到半状态）。

use smol_str::SmolStr;

use crate::event::ImeEvent;

/// 一个窗口的 IME 状态（主线程独占；由 `WaylandState` 持有）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// `done` 之前的 preedit（`None` = 本批没有 preedit 事件）。
    pending_preedit: Option<Option<String>>,
    /// `done` 之前的 commit。
    pending_commit: Option<String>,
    /// 上一次生效的 commit（去重：GNOME 的部分版本会重复发）。
    last_commit: SmolStr,
    /// 当前是否处于组合态。
    composing: bool,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn note_preedit(&mut self, text: Option<String>) {
        self.pending_preedit = Some(text);
    }

    pub(crate) fn note_commit(&mut self, text: Option<String>) {
        self.pending_commit = text;
    }

    /// `done`：把本批的事件原子转换成 `ImeEvent`（按「commit → preedit」顺序）。
    pub(crate) fn take_events(&mut self) -> Vec<ImeEvent> {
        let mut events = Vec::with_capacity(2);
        if let Some(commit) = self.pending_commit.take() {
            self.composing = false;
            let next = SmolStr::new(commit.as_str());
            if !commit.is_empty() && next != self.last_commit {
                self.last_commit = next;
                events.push(ImeEvent::Commit(SmolStr::new(commit)));
            }
        }
        if let Some(preedit) = self.pending_preedit.take() {
            match preedit {
                Some(text) if !text.is_empty() => {
                    self.composing = true;
                    events.push(ImeEvent::Preedit(SmolStr::new(text)));
                }
                _ => {
                    if self.composing {
                        self.composing = false;
                        events.push(ImeEvent::End);
                    }
                }
            }
        }
        events
    }
}
