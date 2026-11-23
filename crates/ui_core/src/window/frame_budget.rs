use std::time::{Duration, Instant};

const MIN_REFRESH_HZ: f32 = 24.0;
const MAX_REFRESH_HZ: f32 = 480.0;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameBudget {
    interval: Option<Duration>,
    next_allowed: Option<Instant>,
    deferrals: u64,
    deferred: bool,
}

impl FrameBudget {
    pub(crate) fn set_refresh_hz(&mut self, hz: f32) {
        let interval = if hz.is_finite() && (MIN_REFRESH_HZ..=MAX_REFRESH_HZ).contains(&hz) {
            Some(Duration::from_secs_f64(1.0 / f64::from(hz)))
        } else {
            None
        };
        if interval != self.interval {
            self.interval = interval;
            self.next_allowed = None;
            self.deferred = false;
        }
    }

    pub(crate) fn refresh_hz(&self) -> Option<f32> {
        self.interval
            .map(|interval| 1.0 / interval.as_secs_f64() as f32)
    }

    pub(crate) fn admits(&self, now: Instant, forced: bool) -> bool {
        if forced {
            return true;
        }
        let Some(interval) = self.interval else {
            return true;
        };
        self.next_allowed
            .is_none_or(|next| now + interval / 10 >= next)
    }

    pub(crate) fn note_start(&mut self, now: Instant) {
        self.deferred = false;
        if let Some(interval) = self.interval {
            self.next_allowed = Some(now + interval);
        }
    }

    pub(crate) fn note_deferred(&mut self) -> bool {
        self.deferrals = self.deferrals.saturating_add(1);
        let first = !self.deferred;
        self.deferred = true;
        first
    }

    pub(crate) fn is_deferred(&self) -> bool {
        self.deferred
    }

    pub(crate) fn clear_deferred(&mut self) {
        self.deferred = false;
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deferred.then_some(self.next_allowed).flatten()
    }

    pub(crate) fn deferrals(&self) -> u64 {
        self.deferrals
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_test_interval(&mut self, interval: Option<Duration>) {
        self.interval = interval;
        self.next_allowed = None;
        self.deferred = false;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::FrameBudget;

    fn budget(hz: f32) -> FrameBudget {
        let mut budget = FrameBudget::default();
        budget.set_refresh_hz(hz);
        budget
    }

    #[test]
    fn a_second_request_inside_the_refresh_period_is_deferred() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        assert!(budget.admits(t0, false), "第一帧必须放行");
        budget.note_start(t0);

        assert!(
            !budget.admits(t0 + Duration::from_millis(8), false),
            "同一 refresh 周期内的第二次请求必须被推迟"
        );
        assert!(
            budget.admits(t0 + Duration::from_millis(17), false),
            "下一拍必须放行"
        );
    }

    #[test]
    fn a_deferred_request_is_admitted_at_the_next_tick_and_clears_the_flag() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        budget.note_start(t0);
        assert!(!budget.admits(t0 + Duration::from_millis(1), false));
        assert!(budget.note_deferred(), "第一笔推迟必须让平台挂唤醒");
        assert!(budget.is_deferred());
        #[cfg(target_os = "linux")]
        {
            let deadline = budget.deadline().expect("推迟后必须有到期时刻");
            assert_eq!(deadline, t0 + Duration::from_secs_f64(1.0 / 60.0));
        }

        budget.note_start(t0 + Duration::from_secs_f64(1.0 / 60.0));
        assert!(!budget.is_deferred());
        assert!(!budget.admits(
            t0 + Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1),
            false
        ));
        assert!(budget.note_deferred());
    }

    #[test]
    fn the_force_channel_bypasses_the_budget_but_advances_the_phase() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        assert!(budget.admits(t0, true), "force 必须放行");
        budget.note_start(t0);
        assert!(
            budget.admits(t0 + Duration::from_millis(1), true),
            "force 在周期内也必须放行"
        );
        budget.note_start(t0 + Duration::from_millis(1));
        assert!(!budget.admits(t0 + Duration::from_millis(2), false));
        assert!(budget.admits(t0 + Duration::from_millis(18), false));
    }

    #[test]
    fn an_unknown_or_invalid_refresh_rate_disables_the_gate() {
        let mut budget = FrameBudget::default();
        let t0 = Instant::now();
        assert!(budget.admits(t0, false), "门禁未启用 ⇒ 永远放行");
        budget.note_start(t0);
        assert!(budget.admits(t0, false));

        for hz in [0.0, -60.0, f32::NAN, f32::INFINITY, 1.0, 1000.0] {
            budget.set_refresh_hz(hz);
            assert_eq!(budget.refresh_hz(), None, "非法提示 {hz} 必须禁用门禁");
            assert!(budget.admits(t0, false));
        }

        budget.set_refresh_hz(24.0);
        assert!(
            budget
                .refresh_hz()
                .is_some_and(|hz| (hz - 24.0).abs() < 0.01)
        );
        budget.set_refresh_hz(480.0);
        assert!(
            budget
                .refresh_hz()
                .is_some_and(|hz| (hz - 480.0).abs() < 0.01)
        );
        for hz in [23.9, 480.1] {
            budget.set_refresh_hz(hz);
            assert_eq!(budget.refresh_hz(), None, "边界外提示 {hz} 必须禁用门禁");
        }
    }

    #[test]
    fn a_new_refresh_rate_restarts_the_phase() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        budget.note_start(t0);
        assert!(!budget.admits(t0 + Duration::from_millis(1), false));
        budget.set_refresh_hz(120.0);
        assert!(budget.admits(t0, false), "提示变化必须重置相位并立即放行");
        assert!((budget.refresh_hz().expect("120 Hz") - 120.0).abs() < 0.01);
        budget.note_start(t0);
        assert!(budget.admits(t0 + Duration::from_millis(17), false));
        assert!(!budget.admits(t0 + Duration::from_millis(5), false));
        budget.note_start(t0 + Duration::from_millis(20));
        budget.set_refresh_hz(120.0);
        assert!(
            !budget.admits(t0 + Duration::from_millis(21), false),
            "同值写入不得重置相位"
        );
        assert_eq!(budget.deferrals(), 0, "相位重置不清累计计数");
    }

    #[test]
    fn clearing_the_deferred_flag_stops_the_one_shot_wakeup_without_forging_a_frame() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        budget.note_start(t0);
        assert!(!budget.admits(t0 + Duration::from_millis(1), false));
        assert!(budget.note_deferred());
        assert!(budget.is_deferred());
        budget.clear_deferred();
        assert!(
            !budget.is_deferred(),
            "不可见窗口的推迟标记必须清掉（RA-5）"
        );
        assert!(
            !budget.admits(t0 + Duration::from_millis(2), false),
            "清除标记不等于放行（脏位保留、相位不变）"
        );
        assert_eq!(budget.deferrals(), 1, "累计计数保留（诊断量）");
    }
}
