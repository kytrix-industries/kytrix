//! macOS 合成线程的帧时钟（B6-2）：display link 的目标呈现时刻 + 单调间隔兜底。
//!
//! 节奏语义在共享层（`render/frame_clock.rs` 的 `FramePacing`）；本模块只提供**时钟源**：
//!
//! - 来源：`CAMetalDisplayLinkUpdate.targetPresentationTimestamp`（host time 秒）。GPU 线程的
//!   display link 回调把它交给 [`note_display_link_target`]，合成线程的
//!   `SignaledIntervalClock` 据此对齐相位并反推刷新率（120 Hz ProMotion 自动跟上）；
//! - 兜底：回调还没接线 / 系统没给目标时刻（headless、窗口不可见）时按累积分钟间隔推算。
//!
//! **host time → `Instant` 的换算只在这里做一次**：锚点用「首次调用时的
//! `(Instant::now(), host_seconds)`」，之后是纯函数（`TimeAnchor::to_instant`，已在共享层单测，
//! 本机可跑）。锚点误差只有一次调用的调度延迟（≤ 数十 µs），对帧节奏没有影响。

use std::{sync::OnceLock, time::Instant};

use crate::render::frame_clock::{SignaledIntervalClock, TimeAnchor, publish_platform_tick};

/// 合成线程的帧时钟（平台回调驱动 + 单调间隔兜底）。
pub(crate) type CompositorClock = SignaledIntervalClock;

/// host time 锚点（进程级只建一次；首次回调时建立）。
fn host_anchor() -> &'static OnceLock<Option<TimeAnchor>> {
    static ANCHOR: OnceLock<Option<TimeAnchor>> = OnceLock::new();
    &ANCHOR
}

/// display link 回调入口（`CAMetalDisplayLinkUpdate` 里调用）。
///
/// `host_seconds` 是 `targetPresentationTimestamp`（与 `CACurrentMediaTime()` 同一时基）。窗口不可见
/// / 系统没给目标时刻（非正值、非有限）⇒ 等价于「只表示可以出帧」，由时钟按间隔推算。
pub(crate) fn note_display_link_target(host_seconds: f64) {
    let now = Instant::now();
    let anchor = host_anchor().get_or_init(|| TimeAnchor::new(now, host_seconds));
    let target = anchor.and_then(|anchor| anchor.to_instant(host_seconds));
    publish_platform_tick(target);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{CompositorClock, note_display_link_target};
    use crate::render::frame_clock::{FALLBACK_REFRESH_HZ, FrameClock};

    /// 没有 display link 回调时按刷新率兜底（不能停帧）。
    #[test]
    fn the_default_clock_falls_back_to_the_refresh_interval() {
        let mut clock = CompositorClock::new(FALLBACK_REFRESH_HZ);
        let tick = clock.wait_next_frame();
        assert!(tick.present_time > tick.now);
        assert_eq!(tick.refresh_hz, FALLBACK_REFRESH_HZ);
    }

    /// 两次相邻的 display link 目标（120 Hz ⇒ 8.3 ms）把刷新率反推到 120 Hz 附近。
    #[test]
    fn display_link_targets_drive_the_phase_and_the_refresh_rate() {
        let mut clock = CompositorClock::new(FALLBACK_REFRESH_HZ);
        let base = Instant::now().elapsed().as_secs_f64();
        note_display_link_target(base + 0.008);
        let first = clock.wait_next_frame();
        assert!(first.present_time > first.now, "目标必须在未来");
        note_display_link_target(base + 0.016);
        let second = clock.wait_next_frame();
        assert!(second.present_time >= second.now);
        assert!(
            (clock.refresh_hz() - 120.0).abs() < 20.0,
            "回调节奏反推的刷新率必须接近 120 Hz，实际 {}",
            clock.refresh_hz()
        );
    }
}
