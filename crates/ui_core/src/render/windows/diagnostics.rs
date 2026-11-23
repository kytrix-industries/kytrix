use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Diagnostics {
    pub frame_wait_timeouts: u64,
    pub present_waitable_timeouts: u64,
    pub present_failed: u64,
    pub present_retries: u64,
    pub occluded_skips: u64,
    pub device_recoveries: u64,
    pub composition_recoveries: u64,
    pub display_recoveries: u64,
    pub atlas_deferred_rebuilds: u64,
    pub resize_fallbacks: u64,
    pub op_visits: u64,
    pub op_replay_budget: u64,
    pub debug_warnings: u64,
    pub debug_errors: u64,
    pub debug_corruption: u64,
}

static FRAME_WAIT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_WAITABLE_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_FAILED: AtomicU64 = AtomicU64::new(0);
static PRESENT_RETRIES: AtomicU64 = AtomicU64::new(0);
static OCCLUDED_SKIPS: AtomicU64 = AtomicU64::new(0);
static DEVICE_RECOVERIES: AtomicU64 = AtomicU64::new(0);
static COMPOSITION_RECOVERIES: AtomicU64 = AtomicU64::new(0);
static DISPLAY_RECOVERIES: AtomicU64 = AtomicU64::new(0);
static ATLAS_DEFERRED_REBUILDS: AtomicU64 = AtomicU64::new(0);
static RESIZE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static OP_VISITS: AtomicU64 = AtomicU64::new(0);
static OP_REPLAY_BUDGET: AtomicU64 = AtomicU64::new(0);

pub(crate) fn frame_wait_timeout() {
    FRAME_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn present_waitable_timeout() {
    PRESENT_WAITABLE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn present_failed() {
    PRESENT_FAILED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn present_retried() {
    PRESENT_RETRIES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn occluded_skipped() {
    OCCLUDED_SKIPS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn device_recovered() {
    DEVICE_RECOVERIES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn composition_recovered() {
    COMPOSITION_RECOVERIES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn display_recovered() {
    DISPLAY_RECOVERIES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn atlas_deferred_rebuild() {
    ATLAS_DEFERRED_REBUILDS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn resize_fallback() {
    RESIZE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn op_visits(visited: u32, budget: u32) {
    OP_VISITS.fetch_add(u64::from(visited), Ordering::Relaxed);
    OP_REPLAY_BUDGET.fetch_add(u64::from(budget), Ordering::Relaxed);
}

pub(crate) fn snapshot() -> Diagnostics {
    let debug = super::debug::message_counts();
    Diagnostics {
        frame_wait_timeouts: FRAME_WAIT_TIMEOUTS.load(Ordering::Relaxed),
        present_waitable_timeouts: PRESENT_WAITABLE_TIMEOUTS.load(Ordering::Relaxed),
        present_failed: PRESENT_FAILED.load(Ordering::Relaxed),
        present_retries: PRESENT_RETRIES.load(Ordering::Relaxed),
        occluded_skips: OCCLUDED_SKIPS.load(Ordering::Relaxed),
        device_recoveries: DEVICE_RECOVERIES.load(Ordering::Relaxed),
        composition_recoveries: COMPOSITION_RECOVERIES.load(Ordering::Relaxed),
        display_recoveries: DISPLAY_RECOVERIES.load(Ordering::Relaxed),
        atlas_deferred_rebuilds: ATLAS_DEFERRED_REBUILDS.load(Ordering::Relaxed),
        resize_fallbacks: RESIZE_FALLBACKS.load(Ordering::Relaxed),
        op_visits: OP_VISITS.load(Ordering::Relaxed),
        op_replay_budget: OP_REPLAY_BUDGET.load(Ordering::Relaxed),
        debug_warnings: debug.warning,
        debug_errors: debug.error,
        debug_corruption: debug.corruption,
    }
}

impl Diagnostics {
    pub(crate) fn describe(&self) -> smol_str::SmolStr {
        smol_str::format_smolstr!(
            "wait_timeouts={} present_waitable_timeouts={} present_failed={} present_retries={} occluded_skips={} device_recoveries={} composition_recoveries={} display_recoveries={} atlas_deferred={} resize_fallbacks={} op_visits={}/{} debug(w/e/c)={}/{}/{}",
            self.frame_wait_timeouts,
            self.present_waitable_timeouts,
            self.present_failed,
            self.present_retries,
            self.occluded_skips,
            self.device_recoveries,
            self.composition_recoveries,
            self.display_recoveries,
            self.atlas_deferred_rebuilds,
            self.resize_fallbacks,
            self.op_visits,
            self.op_replay_budget,
            self.debug_warnings,
            self.debug_errors,
            self.debug_corruption
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Diagnostics, snapshot};

    #[test]
    fn the_snapshot_is_monotonic_and_describable() {
        let first = snapshot();
        super::frame_wait_timeout();
        super::occluded_skipped();
        super::present_retried();
        super::display_recovered();
        let second = snapshot();
        assert!(second.frame_wait_timeouts > first.frame_wait_timeouts);
        assert!(second.occluded_skips > first.occluded_skips);
        assert!(second.present_retries > first.present_retries);
        assert!(
            second.display_recoveries > first.display_recoveries,
            "显示恢复必须进诊断快照"
        );

        let text = second.describe();
        for needle in [
            "wait_timeouts=",
            "op_visits=",
            "occluded_skips=",
            "display_recoveries=",
            "debug(w/e/c)=",
        ] {
            assert!(text.contains(needle), "missing {needle} in {text}");
        }
        assert_eq!(Diagnostics::default().op_visits, 0);
    }
}
