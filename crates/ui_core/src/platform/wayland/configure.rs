#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AckAction {
    AckNow,
    Defer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Initial,
    Mapped,
}

pub(crate) struct ConfigureLatch {
    phase: Phase,
    pending: Option<u32>,
    changed: bool,
}

impl ConfigureLatch {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Initial,
            pending: None,
            changed: false,
        }
    }

    pub(crate) fn on_toplevel_configure(&mut self, buffer_state_changed: bool) {
        if buffer_state_changed {
            self.changed = true;
        }
    }

    pub(crate) fn on_surface_configure(&mut self, serial: u32) -> AckAction {
        if self.phase == Phase::Initial {
            self.phase = Phase::Mapped;
            self.changed = false;
            return AckAction::AckNow;
        }
        if self.changed {
            self.pending = Some(serial);
            AckAction::Defer
        } else {
            AckAction::AckNow
        }
    }

    pub(crate) fn on_frame_published(&mut self) -> Option<u32> {
        let serial = self.pending.take()?;
        self.changed = false;
        Some(serial)
    }
}

#[cfg(test)]
mod tests {
    use super::{AckAction, ConfigureLatch};

    fn mapped() -> ConfigureLatch {
        let mut latch = ConfigureLatch::new();
        assert_eq!(
            latch.on_surface_configure(1),
            AckAction::AckNow,
            "初始 configure 必须立即 ack（否则不能 attach buffer 映射）"
        );
        latch
    }

    #[test]
    fn the_initial_configure_is_acked_immediately() {
        let mut latch = ConfigureLatch::new();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(7), AckAction::AckNow);
        assert_eq!(
            latch.on_frame_published(),
            None,
            "初始 configure 已经 ack 过，不得再排队等帧"
        );

        assert_eq!(latch.on_surface_configure(8), AckAction::AckNow);
        assert_eq!(latch.on_frame_published(), None);
    }

    #[test]
    fn a_buffer_state_change_is_acked_after_the_frame_is_published() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(9), AckAction::Defer);
        assert_eq!(
            latch.on_frame_published(),
            Some(9),
            "帧发布之后才把 serial 交给 ack 路径"
        );
        assert_eq!(
            latch.on_frame_published(),
            None,
            "同一个 serial 只能 ack 一次"
        );
    }

    #[test]
    fn only_the_last_configure_is_acked() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(10), AckAction::Defer);
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(11), AckAction::Defer);
        assert_eq!(
            latch.pending,
            Some(11),
            "pending 只保留最后一条 configure 的 serial"
        );
        assert_eq!(latch.on_frame_published(), Some(11));
        assert_eq!(
            latch.on_frame_published(),
            None,
            "10 已被 11 隐式消费，不得再 ack"
        );
    }

    #[test]
    fn an_unchanged_configure_is_acked_immediately() {
        let mut latch = mapped();
        latch.on_toplevel_configure(false);
        assert_eq!(latch.on_surface_configure(12), AckAction::AckNow);
        assert_eq!(latch.on_frame_published(), None);

        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(13), AckAction::Defer);
        assert_eq!(latch.on_surface_configure(14), AckAction::Defer);
        assert_eq!(latch.on_frame_published(), Some(14));
    }

    #[test]
    fn a_resize_sequence_acks_once_per_published_frame() {
        let mut latch = mapped();
        let mut serial = 20;
        for _ in 0..4 {
            latch.on_toplevel_configure(true);
            assert_eq!(latch.on_surface_configure(serial), AckAction::Defer);
            assert_eq!(latch.on_frame_published(), Some(serial));
            serial += 1;
        }
        assert_eq!(latch.on_frame_published(), None);
        assert_eq!(latch.on_surface_configure(serial), AckAction::AckNow);
    }

    #[test]
    fn a_forced_ack_consumes_the_pending_serial() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(30), AckAction::Defer);
        assert_eq!(latch.on_frame_published(), Some(30), "force ack 走同一出口");
        assert_eq!(
            latch.on_surface_configure(31),
            AckAction::AckNow,
            "force ack 后 changed 清零：没有新状态变化就立即 ack"
        );
    }

    #[test]
    fn an_idle_window_keeps_its_pending_serial() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(40), AckAction::Defer);

        for _ in 0..3 {
            latch.on_toplevel_configure(false);
            assert_eq!(latch.pending, Some(40), "空闲期间 pending 不得丢失");
        }
        assert_eq!(latch.on_frame_published(), Some(40));
    }

    #[test]
    fn a_frame_published_before_the_surface_configure_does_not_clear_the_change() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_frame_published(), None, "还没有 pending serial");
        assert_eq!(
            latch.on_surface_configure(50),
            AckAction::Defer,
            "changed 必须保留到该 configure 被 ack"
        );
        assert_eq!(latch.on_frame_published(), Some(50));
    }

    #[test]
    fn latches_are_independent_per_window() {
        let mut first = mapped();
        let mut second = mapped();
        first.on_toplevel_configure(true);
        assert_eq!(first.on_surface_configure(60), AckAction::Defer);
        second.on_toplevel_configure(true);
        assert_eq!(second.on_surface_configure(61), AckAction::Defer);

        assert_eq!(second.on_frame_published(), Some(61));
        assert_eq!(
            first.pending,
            Some(60),
            "另一窗口的帧发布不得消费本窗口的 pending"
        );
        assert_eq!(first.on_frame_published(), Some(60));
    }
}
