//! 呈现节奏（R09-6 / AR3 §6.2）：`CAMetalDisplayLink` 的目标时刻 → 本 vsync 的呈现决定。
//!
//! **本文件只做时基换算**：判定逻辑是共享层的 `render::frame_clock::FramePacing`（三平台唯一
//! 一份实现）。macOS 独有的东西只有两样：
//!
//! 1. 输入是 `CFTimeInterval`（host time 秒，与 `CACurrentMediaTime()` 同一时基），而共享层用
//!    `Instant` —— 因此这里维护一个进程级锚点做纯换算；
//! 2. `presentDrawable:atTime:` 需要一个 `CFTimeInterval`，所以判定结果要换回 host time。
//!
//! 旧实现（AR3 复审 P1）的两个缺陷已由共享实现修掉：连续跳过上界是 **1**，且 `Now` 分支**不**
//! 清零计数（否则稳态是 `Skip, Skip, Now` = 每 3 个 vsync 只提交 1 帧，120 Hz → 40 fps）。

use std::{sync::OnceLock, time::Instant};

use objc2_core_foundation::CFTimeInterval;

use crate::render::frame_clock::{FALLBACK_REFRESH_HZ, FramePacing, FrameTick, TimeAnchor};

/// 本 vsync 的呈现决定（[`PresentPacing::plan`] 的结论）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PresentPlan {
    /// `presentDrawable:atTime:`：钉到 display link 要求的那一帧。
    AtTime(CFTimeInterval),
    /// `presentDrawable:`：目标时刻不可用，或连续迟到到不值得再等。
    Now,
    /// 跳过本 vsync：目标时刻已经不够提交（提交注定被推到下一帧）。
    Skip,
}

/// host time 锚点（进程级一次）：`CFTimeInterval` ⇄ `Instant` 的纯换算。
fn host_anchor() -> Option<&'static TimeAnchor> {
    static ANCHOR: OnceLock<Option<TimeAnchor>> = OnceLock::new();
    ANCHOR
        .get_or_init(|| TimeAnchor::new(Instant::now(), host_now()))
        .as_ref()
}

/// 当前 host time（`CACurrentMediaTime` 同一时基；失败时用 0 让锚点退化为「从进程启动算起」）。
fn host_now() -> f64 {
    objc2_quartz_core::CACurrentMediaTime()
}

/// 呈现时刻判定（薄壳：状态在共享层的 [`FramePacing`] 里）。
pub(crate) struct PresentPacing {
    inner: FramePacing,
}

impl PresentPacing {
    pub(crate) const fn new() -> Self {
        Self {
            inner: FramePacing::new(),
        }
    }

    /// **录制之前**的判定：本 vsync 提交不提交、提交时钉不钉目标时刻。
    pub(crate) fn plan(&mut self, now: CFTimeInterval, target: CFTimeInterval) -> PresentPlan {
        // 时基换算失败（锚点缺失 / 异常值）：退回「尽快呈现」，绝不把非法时刻交给 CA。
        let Some(tick) = self.tick(now, target) else {
            return PresentPlan::Now;
        };
        match self.inner.plan(tick) {
            // 钉时刻时把**原始的** host time 还给调用方（避免 Instant 往返的量化误差）。
            crate::render::frame_clock::PresentDecision::AtTime(_) => PresentPlan::AtTime(target),
            crate::render::frame_clock::PresentDecision::Now => PresentPlan::Now,
            crate::render::frame_clock::PresentDecision::Skip => PresentPlan::Skip,
        }
    }

    fn tick(&self, now: CFTimeInterval, target: CFTimeInterval) -> Option<FrameTick> {
        let anchor = host_anchor()?;
        Some(FrameTick {
            now: anchor.to_instant(now)?,
            present_time: anchor.to_instant(target)?,
            refresh_hz: FALLBACK_REFRESH_HZ,
            completed: true,
        })
    }
}

/// **提交之前**的二次夹紧：用录制结束后的新时刻重新判定目标是否还够提交。
///
/// `None` = 目标被录制耗时吃掉了（resize 重建 persist、首帧同步编译 PSO、整页图集上传都可能
/// 花掉整个余量），调用方必须退回 `presentDrawable:`（尽快呈现），而不是把一个**已经过期**的
/// 时刻交给 CA —— 后者会被推到下一个 vsync，正是 R09-6 想消除的「迟到一帧 + 抖动」。
pub(crate) fn clamp_present_time(
    target: CFTimeInterval,
    now: CFTimeInterval,
) -> Option<CFTimeInterval> {
    let anchor = host_anchor()?;
    let target_instant = anchor.to_instant(target)?;
    let now_instant = anchor.to_instant(now)?;
    FramePacing::clamp_before_present(target_instant, now_instant).map(|_| target)
}

#[cfg(test)]
mod tests {
    use super::{PresentPacing, PresentPlan, clamp_present_time};
    use crate::render::frame_clock::{MAX_CONSECUTIVE_SKIPS, PRESENT_MARGIN};

    /// 120 Hz 的 vsync 周期（秒）。
    const VSYNC_120: f64 = 1.0 / 120.0;

    /// 还来得及提交：钉到目标呈现时刻。
    #[test]
    fn a_fresh_target_presentation_timestamp_is_pinned() {
        let mut pacing = PresentPacing::new();
        let target = 1_000.010;
        assert_eq!(pacing.plan(1_000.0, target), PresentPlan::AtTime(target));
    }

    /// 连续迟到：**最多**跳 1 个 vsync，之后保持「尽快呈现」，直到重新赶上目标时刻
    /// （不允许 1/3 刷新率，也不在 `Now` 分支清零计数）。
    #[test]
    fn a_late_target_skips_once_then_presents_immediately() {
        let mut pacing = PresentPacing::new();
        let target = 1_000.0;
        let late = target - PRESENT_MARGIN.as_secs_f64() / 2.0;
        assert_eq!(pacing.plan(late, target), PresentPlan::Skip, "落后：跳一次");
        for _ in 0..5 {
            assert_eq!(
                pacing.plan(late, target),
                PresentPlan::Now,
                "跳过之后必须保持提交（不能每隔几个 vsync 再跳一次）"
            );
        }
        // 追上之后回到钉时刻（计数随之清零）。
        assert_eq!(
            pacing.plan(1_000.0, target + 0.010),
            PresentPlan::AtTime(target + 0.010)
        );
    }

    /// 系统不给目标时刻（headless / 0 / NaN / 负 / 巨型提前量）：回到「尽快呈现」。
    #[test]
    fn a_missing_or_absurd_target_timestamp_presents_immediately() {
        for target in [0.0, f64::NAN, f64::INFINITY, -1.0, f64::MAX] {
            let mut pacing = PresentPacing::new();
            assert_eq!(pacing.plan(123.0, target), PresentPlan::Now, "{target:?}");
        }
    }

    /// 慢帧 / 唤醒抖动序列（120 Hz，GPU 线程每次都在 vsync 之后 7 ms 才被唤醒）绝不能退化成
    /// 「每 3 个 vsync 只提交 1 帧」。
    #[test]
    fn a_slow_120hz_frame_series_never_drops_to_one_third_rate() {
        const WAKE_DELAY: f64 = 0.007;
        const VSYNCS: u32 = 30;
        let mut pacing = PresentPacing::new();
        pacing.set_refresh_hz(120.0);
        let mut submitted = 0u32;
        let mut consecutive_skips = 0u32;
        let mut max_consecutive_skips = 0u32;
        for vsync in 0..VSYNCS {
            let now = f64::from(vsync) * VSYNC_120 + WAKE_DELAY;
            // display link 要求本帧在**下一个** vsync 上屏。
            let target = f64::from(vsync + 1) * VSYNC_120;
            match pacing.plan(now, target) {
                PresentPlan::Skip => {
                    consecutive_skips += 1;
                    max_consecutive_skips = max_consecutive_skips.max(consecutive_skips);
                }
                PresentPlan::AtTime(_) | PresentPlan::Now => {
                    consecutive_skips = 0;
                    submitted += 1;
                }
            }
        }
        assert!(
            max_consecutive_skips <= MAX_CONSECUTIVE_SKIPS,
            "最多连续跳过 {max_consecutive_skips} 次：1/3 刷新率回归"
        );
        assert!(
            submitted >= VSYNCS - 2,
            "慢帧下只提交了 {submitted}/{VSYNCS} 帧（旧实现的 1/3 刷新率 = 10 帧）"
        );
    }

    /// 录制耗时把目标吃掉：提交前必须改成「立即呈现」，不能把过期时刻交给 CA。
    #[test]
    fn a_target_that_expires_during_recording_presents_immediately() {
        let target = 1_000.010;
        // 录制 5 ms：还剩 3 ms 余量，仍可钉时刻。
        assert_eq!(
            clamp_present_time(target, target - 0.005),
            Some(target),
            "录制后仍来得及提交"
        );
        // 只剩一半余量：已经不够提交（用「明显不足」而不是卡在浮点边界上）。
        assert_eq!(
            clamp_present_time(target, target - PRESENT_MARGIN.as_secs_f64() / 2.0),
            None
        );
        // 已经过期：立即呈现。
        assert_eq!(clamp_present_time(target, target + 0.001), None);
        // 异常目标时刻：同样退回立即呈现。
        assert_eq!(clamp_present_time(f64::MAX, 1_000.0), None);
        assert_eq!(clamp_present_time(0.0, 1_000.0), None);
    }

    /// 一次迟到不会粘住状态：紧随其后的正常 vsync 仍走钉时刻路径。
    #[test]
    fn one_late_vsync_does_not_stick() {
        let mut pacing = PresentPacing::new();
        assert_eq!(pacing.plan(1_000.0, 1_000.001), PresentPlan::Skip);
        assert_eq!(
            pacing.plan(1_000.002, 1_000.010),
            PresentPlan::AtTime(1_000.010),
            "重新对齐后必须恢复钉时刻"
        );
    }
}
