use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME},
    window::FrameIndex,
};

pub(crate) fn presentation_clock_supported(clock_id: i32) -> bool {
    matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC)
}

#[derive(Debug)]
pub(crate) struct PresentationValidity {
    valid: AtomicBool,
}

impl PresentationValidity {
    pub(crate) const fn new() -> Self {
        Self {
            valid: AtomicBool::new(true),
        }
    }

    pub(crate) fn invalidate(&self) -> bool {
        self.valid.swap(false, Ordering::Release)
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }
}

impl Default for PresentationValidity {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
pub(crate) type SharedPresentationValidity = std::sync::Arc<PresentationValidity>;

pub(crate) struct PresentedOrder {
    last: Option<FrameIndex>,
}

impl PresentedOrder {
    pub(crate) const fn new() -> Self {
        Self { last: None }
    }

    pub(crate) fn accept(&mut self, frame: FrameIndex) -> bool {
        if self.last.is_some_and(|last| frame.get() <= last.get()) {
            return false;
        }
        self.last = Some(frame);
        true
    }

    #[cfg(test)]
    pub(crate) const fn last(&self) -> Option<FrameIndex> {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{PresentationValidity, PresentedOrder, presentation_clock_supported};
    use crate::{
        render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME, presented_event_instant},
        window::FrameIndex,
    };

    fn frame(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    #[test]
    fn only_real_time_and_monotonic_are_supported_presentation_clocks() {
        assert!(presentation_clock_supported(CLOCK_REALTIME));
        assert!(presentation_clock_supported(CLOCK_MONOTONIC));
        for clock in [2, 3, 4, 5, 6, 7, -1] {
            assert!(
                !presentation_clock_supported(clock),
                "clock_id={clock} 不是数据面支持的呈现时钟"
            );
        }
    }

    #[test]
    fn presented_evidence_must_arrive_in_submission_order() {
        let mut order = PresentedOrder::new();
        assert!(order.accept(frame(3)), "首个 presented 证据");
        assert!(order.accept(frame(5)), "跳号（中间帧没提交）仍递增");
        assert!(!order.accept(frame(4)), "旧帧迟到 ⇒ 丢弃");
        assert!(!order.accept(frame(5)), "重复事件 ⇒ 丢弃");
        assert_eq!(order.last(), Some(frame(5)), "watermark 只前进");
        assert!(order.accept(frame(6)));
        assert_eq!(order.last(), Some(frame(6)));
    }

    #[test]
    fn a_removed_presentation_global_strictly_invalidates_existing_handles() {
        let validity = PresentationValidity::new();
        assert!(validity.is_valid(), "初始有效");
        assert!(validity.invalidate(), "首次失效用于一次性诊断");
        assert!(!validity.is_valid(), "失效后句柄不得再请求呈现反馈");
        assert!(!validity.invalidate(), "重复失效幂等（不重复诊断）");
    }

    #[test]
    fn a_clock_anchor_read_after_the_instant_maps_presented_strictly_early() {
        let anchor = Instant::now();
        let clock_at_anchor = 1000.0_f64;
        let clock_anchor = clock_at_anchor + 0.02;
        let mapped = presented_event_instant(
            0,
            1000,
            0,
            CLOCK_MONOTONIC,
            clock_anchor,
            anchor,
            anchor + Duration::from_secs(1),
        )
        .expect("偏差 20ms 在可信范围内");
        assert!(
            mapped < anchor,
            "presented_at 必须是真实上屏时刻的下界（不得晚于锚点）"
        );
        assert_eq!(
            mapped,
            anchor
                .checked_sub(Duration::from_secs_f64(0.02))
                .expect("Instant 减法")
        );
    }
}
