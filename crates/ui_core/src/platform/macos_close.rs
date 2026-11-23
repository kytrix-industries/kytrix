use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseState {
    Idle,
    Requested,
    WillClose,
}

#[derive(Debug)]
pub(crate) struct CloseBridge {
    state: Cell<CloseState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseAction {
    Keep,
    Close,
    RemoveNative,
}

impl CloseBridge {
    pub(crate) const fn new() -> Self {
        Self {
            state: Cell::new(CloseState::Idle),
        }
    }

    pub(crate) fn request(&self) -> bool {
        if self.state.get() != CloseState::Idle {
            return false;
        }
        self.state.set(CloseState::Requested);
        true
    }

    pub(crate) fn will_close(&self) -> bool {
        if self.state.get() == CloseState::WillClose {
            return false;
        }
        self.state.set(CloseState::WillClose);
        true
    }

    pub(crate) fn decide(&self, verdict: impl FnOnce() -> bool) -> CloseAction {
        match self.state.get() {
            CloseState::WillClose => {
                self.state.set(CloseState::Idle);
                CloseAction::RemoveNative
            }
            CloseState::Requested => {
                self.state.set(CloseState::Idle);
                if verdict() {
                    CloseAction::Close
                } else {
                    CloseAction::Keep
                }
            }
            CloseState::Idle => CloseAction::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseAction, CloseBridge};

    #[test]
    fn a_gesture_is_delivered_and_decided_exactly_once() {
        let bridge = CloseBridge::new();
        assert!(bridge.request());
        assert!(!bridge.request(), "重复 performClose 必须在桥上去重");
        let mut verdicts = 0_u8;
        let action = bridge.decide(|| {
            verdicts += 1;
            true
        });
        assert_eq!(action, CloseAction::Close);
        assert_eq!(verdicts, 1, "一次手势只能裁决一次");
        assert_eq!(
            bridge.decide(|| panic!("没有新请求时不得再次询问裁决")),
            CloseAction::Keep
        );
    }

    #[test]
    fn a_veto_keeps_the_window_and_returns_to_idle() {
        let bridge = CloseBridge::new();
        assert!(bridge.request());
        assert_eq!(bridge.decide(|| false), CloseAction::Keep);
        assert!(bridge.request());
        assert_eq!(bridge.decide(|| true), CloseAction::Close);
    }

    #[test]
    fn an_external_close_never_takes_the_gesture_path() {
        let bridge = CloseBridge::new();
        assert!(bridge.will_close());
        assert!(!bridge.will_close(), "重复通知不得重复记账");
        assert_eq!(
            bridge.decide(|| panic!("外部关闭不得询问 on_close")),
            CloseAction::RemoveNative
        );
        assert_eq!(
            bridge.decide(|| panic!("外部关闭只记账一次")),
            CloseAction::Keep
        );
    }

    #[test]
    fn an_external_close_supersedes_a_pending_gesture() {
        let bridge = CloseBridge::new();
        assert!(bridge.request());
        assert!(bridge.will_close());
        assert!(!bridge.request(), "关闭中的窗口不得再接受新的手势");
        assert_eq!(
            bridge.decide(|| panic!("已被 AppKit 关掉的窗口不得再询问裁决")),
            CloseAction::RemoveNative
        );
    }
}
