use crate::platform::ime::{CommitVerdict, ImeCommitScope, ImeCompositionEnd, KeyScopeToken};

#[derive(Debug, Default)]
pub(crate) struct ImeState {
    marked: Vec<u16>,
    selected: (usize, usize),
    scope: ImeCommitScope,
    end: ImeCompositionEnd,
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

    pub(crate) fn marked_utf16(&self) -> &[u16] {
        &self.marked
    }

    pub(crate) fn selected_range(&self) -> (usize, usize) {
        self.selected
    }

    pub(crate) fn begin_key(&mut self) -> KeyScopeToken {
        self.scope.begin_key()
    }

    pub(crate) fn end_key(&mut self, token: KeyScopeToken) {
        self.scope.end_key(token);
    }

    pub(crate) fn takes_text(&self, token: KeyScopeToken) -> bool {
        self.scope.takes_text(token)
    }

    pub(crate) fn set_marked(&mut self, text: &[u16], selected: (usize, usize)) {
        self.marked.clear();
        self.marked.extend_from_slice(text);
        let len = self.marked.len();
        let location = selected.0.min(len);
        let length = selected.1.min(len - location);
        self.selected = (location, length);
        if !self.marked.is_empty() {
            self.end.note_preedit();
        }
    }

    pub(crate) fn clear_marked(&mut self) {
        self.marked.clear();
        self.selected = (0, 0);
    }

    pub(crate) fn note_commit(&mut self, text: &str) -> CommitVerdict {
        self.scope.note_commit(text)
    }

    pub(crate) fn note_end(&mut self) -> bool {
        self.end.note_end()
    }

    pub(crate) fn note_commit_end(&mut self) -> bool {
        self.end.note_commit()
    }

    pub(crate) fn has_pending_end(&self) -> bool {
        self.end.is_pending()
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use crate::platform::ime::NS_NOT_FOUND;

    #[test]
    fn ns_not_found_matches_the_objc2_foundation_binding() {
        assert_eq!(objc2_foundation::NSNotFound as usize, isize::MAX as usize);
        assert_eq!(NS_NOT_FOUND, objc2_foundation::NSNotFound as usize);
    }
}
