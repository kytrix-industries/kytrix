#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallbackToken(u64);

pub(crate) struct FrameGate {
    ready: bool,
    pending: Option<CallbackToken>,
    next_token: u64,
}

impl FrameGate {
    pub(crate) const fn new() -> Self {
        Self {
            ready: true,
            pending: None,
            next_token: 0,
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    pub(crate) fn request(&mut self) -> Option<CallbackToken> {
        self.ready = false;
        if self.pending.is_some() {
            return None;
        }
        let token = CallbackToken(self.next_token);
        self.next_token += 1;
        self.pending = Some(token);
        Some(token)
    }

    pub(crate) fn note_not_committed(&mut self) {
        self.ready = true;
    }

    pub(crate) fn on_callback(&mut self, token: CallbackToken) {
        if self.pending != Some(token) {
            return;
        }
        self.pending = None;
        self.ready = true;
    }

    pub(crate) fn force_idle(&mut self) {
        self.ready = true;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::FrameGate;

    #[test]
    fn a_frame_request_is_idempotent_until_the_callback_returns() {
        let mut gate = FrameGate::new();
        assert!(gate.is_ready());

        let first = gate.request().expect("首次请求必须真的发 wl_surface.frame");
        assert!(!gate.is_ready());

        assert!(gate.request().is_none(), "挂起 callback 期间不得重复请求");
        assert!(!gate.is_ready());

        gate.on_callback(first);
        assert!(gate.is_ready(), "callback 到达后门禁重新打开");
        assert!(gate.request().is_some(), "callback 之后可以再次请求");
    }

    #[test]
    fn an_idle_frame_reopens_the_gate_without_duplicating_the_callback() {
        let mut gate = FrameGate::new();
        let callback = gate.request().expect("首次请求挂起 callback");

        gate.note_not_committed();
        assert!(gate.is_ready(), "Idle 没有 commit，门禁必须立即重开");
        assert!(gate.request().is_none(), "挂起 callback 仍在，不能重复请求");
        assert!(!gate.is_ready());

        gate.on_callback(callback);
        assert!(gate.is_ready());
    }

    #[test]
    fn the_callback_clears_the_pending_request() {
        let mut gate = FrameGate::new();
        let callback = gate.request().expect("首次请求挂起 callback");
        gate.note_not_committed();

        gate.on_callback(callback);

        assert!(
            gate.request().is_some(),
            "pending 已清 ⇒ 下一次请求必须重新挂号 callback"
        );
    }

    #[test]
    fn a_closed_gate_waits_for_the_callback_instead_of_a_timeout() {
        let mut gate = FrameGate::new();
        let callback = gate.request().expect("首次请求挂起 callback");

        assert!(!gate.is_ready());
        assert!(
            gate.request().is_none(),
            "时间流逝不得允许重复挂 callback（否则每个 200 ms 就是一次全量重绘）"
        );
        assert!(!gate.is_ready(), "没有 callback 就不能重开");

        gate.on_callback(callback);
        assert!(gate.is_ready(), "只有 callback 才开门");
    }

    #[test]
    fn a_late_callback_after_force_idle_does_not_clear_the_new_request() {
        let mut gate = FrameGate::new();
        let stale = gate.request().expect("首次请求挂起 callback");

        gate.force_idle();
        assert!(gate.is_ready());
        assert_eq!(gate.pending, None, "不可见时放弃旧 callback，允许重新挂号");

        let live = gate
            .request()
            .expect("重新可见之后必须能重新挂一个 callback");
        assert_ne!(live, stale);
        assert!(!gate.is_ready());

        gate.on_callback(stale);
        assert_eq!(
            gate.pending,
            Some(live),
            "迟到的旧 callback 不得清掉新请求的挂起状态"
        );
        assert!(!gate.is_ready(), "新 callback 未返回，门禁不得提前打开");
        assert!(
            gate.request().is_none(),
            "仍有 1 个挂起 ⇒ 第 3 次 request 不能再挂一个"
        );

        gate.on_callback(live);
        assert!(gate.is_ready(), "新 callback 返回后才开门");
        assert_eq!(gate.pending, None);
    }

    #[test]
    fn an_invisible_window_drops_the_gate_without_leaving_a_request() {
        let mut gate = FrameGate::new();
        let stale = gate.request().expect("首次请求挂起 callback");

        gate.force_idle();
        assert!(gate.is_ready());
        assert_eq!(gate.pending, None);

        let fresh = gate.request().expect("重新可见后必须能挂新 callback");
        assert_ne!(fresh, stale, "每次挂号都是新令牌");
        gate.on_callback(stale);
        assert!(
            !gate.is_ready(),
            "旧的（不可见期间被放弃的）callback 迟到不得提前开门"
        );
        gate.on_callback(fresh);
        assert!(gate.is_ready());
    }
}
