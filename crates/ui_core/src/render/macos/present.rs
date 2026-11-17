//! 呈现节奏（R09-6 / AR3 §6.2）：`CAMetalDisplayLink` 的目标时刻 → 本 vsync 的呈现决定。
//!
//! 单独成模块的两个理由：
//!
//! 1. **可测性**：`CACurrentMediaTime()` 与真实 drawable 都无法在单测里构造，能钉住的只有
//!    「给定 `(now, target)` 序列 → 呈现决定」这一层纯逻辑；
//! 2. **两段判定同源**：**录制之前**决定本 vsync 提交不提交，**提交之前**用录制结束后的新
//!    时刻二次夹紧。两段必须共用同一套余量定义，放在一起才不会各自漂移。
//!
//! 旧实现（AR3 复审 P1）的问题：判定只在录制前做一次、提交前没有夹紧，且「连续跳过」上界是 2
//! 并在 `Now` 分支把计数清零 ⇒ 只要 `now` 持续落在 `[target - margin, target]` 窗口里，稳态就是
//! `Skip, Skip, Now, Skip, Skip, Now…`，即**每 3 个 vsync 只提交 1 帧**（120 Hz → 40 fps）。

use objc2_core_foundation::CFTimeInterval;

/// 距离目标呈现时刻不足这个量就认为**来不及提交**。
///
/// 一个 vsync 是 8.3 ms（120 Hz）/ 16.7 ms（60 Hz）；留 2 ms 给「提交 → CA 收到」的调度。
/// `present(at:)` 的文档语义是「命令在指定时刻前完成就按该时刻呈现；否则**尽快呈现**」——
/// 过期的提交不会丢帧，但会被 CA 推到下一个 vsync，表现为固定的迟到与抖动。
pub(crate) const PRESENT_MARGIN_SECS: f64 = 0.002;

/// 连续跳过本 vsync 的上限：**1**（旧值是 2）。
///
/// 跳过的唯一收益是「不提交一帧注定迟到的内容」，而迟到帧本来也只会被 CA 推到下一个 vsync；
/// 用 1/3 刷新率去换这点收益是净亏。因此允许跳 **1** 个 vsync 重新对齐；一旦跳过了，之后
/// **保持**「尽快呈现」直到某一个 vsync 重新赶上目标时刻（`Now` 分支**不**清零计数）——
/// 旧实现在 `Now` 里清零，稳态才是 `Skip, Skip, Now` 的 1/3 刷新率（120 Hz → 40 fps）。
pub(crate) const MAX_CONSECUTIVE_SKIPS: u32 = 1;

/// 目标呈现时刻与「现在」之间允许的最大提前量。
///
/// display link 只给下一两个 vsync（≤ 34 ms）。`f64::MAX` 也是 finite：若把这种异常值交给
/// `presentDrawable:atTime:`，这张 drawable 会**永远不上屏**；因此超过本上界的「目标时刻」按
/// 「系统没给可用时刻」处理（尽快呈现）。
const MAX_PRESENT_LEAD_SECS: f64 = 0.1;

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

/// 呈现时刻判定（R09-6；纯状态机便于单测）。
///
/// 输入是 `CACurrentMediaTime()` 与 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp`
/// （同一 host time 时基）。CPU 落后时**最多跳过 1 个** vsync 重新对齐，随后无论如何都提交。
pub(crate) struct PresentPacing {
    /// 已经连续跳过本 vsync 的次数（`0` 或 `1`，见 [`MAX_CONSECUTIVE_SKIPS`]）。
    consecutive_skips: u32,
}

impl PresentPacing {
    pub(crate) const fn new() -> Self {
        Self {
            consecutive_skips: 0,
        }
    }

    /// **录制之前**的判定：本 vsync 提交不提交、提交时钉不钉目标时刻。
    pub(crate) fn plan(&mut self, now: CFTimeInterval, target: CFTimeInterval) -> PresentPlan {
        // 系统没给可用目标时刻（headless / 异常值 / 巨型提前量）：回到「尽快呈现」。
        let Some(target) = usable_target(now, target) else {
            self.consecutive_skips = 0;
            return PresentPlan::Now;
        };
        if now + PRESENT_MARGIN_SECS <= target {
            self.consecutive_skips = 0;
            return PresentPlan::AtTime(target);
        }
        if self.consecutive_skips >= MAX_CONSECUTIVE_SKIPS {
            // 已经跳过一个 vsync：之后**保持**「尽快呈现」，直到某个 vsync 重新赶上目标时刻
            // （只有 `AtTime` 分支清零计数）。在这里清零会回到 Skip, Skip, Now 的 1/3 刷新率。
            PresentPlan::Now
        } else {
            self.consecutive_skips += 1;
            PresentPlan::Skip
        }
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
    usable_target(now, target).filter(|target| now + PRESENT_MARGIN_SECS <= *target)
}

/// 目标时刻是否可用：有限、为正、且与「现在」的提前量在上界内。
fn usable_target(now: CFTimeInterval, target: CFTimeInterval) -> Option<CFTimeInterval> {
    if !now.is_finite() || !(target.is_finite() && target > 0.0) {
        return None;
    }
    (target - now <= MAX_PRESENT_LEAD_SECS).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CONSECUTIVE_SKIPS, MAX_PRESENT_LEAD_SECS, PRESENT_MARGIN_SECS, PresentPacing,
        PresentPlan, clamp_present_time,
    };

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
        let late = target - PRESENT_MARGIN_SECS / 2.0;
        assert_eq!(pacing.plan(late, target), PresentPlan::Skip, "落后：跳一次");
        for _ in 0..5 {
            assert_eq!(
                pacing.plan(late, target),
                PresentPlan::Now,
                "跳过之后必须保持提交（不能每隔几个 vsync 再跳一次）"
            );
        }
        assert_eq!(
            pacing.consecutive_skips, MAX_CONSECUTIVE_SKIPS,
            "Now 分支不清零：否则会回到 Skip, Skip, Now"
        );
        // 追上之后回到钉时刻（计数随之清零）。
        assert_eq!(
            pacing.plan(1_000.0, target + 0.010),
            PresentPlan::AtTime(target + 0.010)
        );
        assert_eq!(pacing.consecutive_skips, 0);
    }

    /// 系统不给目标时刻（headless / 0 / NaN / 负 / 巨型提前量）：回到「尽快呈现」。
    #[test]
    fn a_missing_or_absurd_target_timestamp_presents_immediately() {
        for target in [0.0, f64::NAN, f64::INFINITY, -1.0, f64::MAX] {
            let mut pacing = PresentPacing::new();
            assert_eq!(pacing.plan(123.0, target), PresentPlan::Now, "{target:?}");
        }
        // 有限但提前量荒谬（1 s 后才上屏）：`presentDrawable:atTime:` 会让这张 drawable 长期
        // 不上屏，所以按「不可用」处理。上界本身必须足够宽（display link 正常只给 ≤ 2 个 vsync）。
        let mut pacing = PresentPacing::new();
        assert_eq!(
            pacing.plan(100.0, 100.0 + MAX_PRESENT_LEAD_SECS / 2.0),
            PresentPlan::AtTime(100.0 + MAX_PRESENT_LEAD_SECS / 2.0),
            "上界以内仍是正常的钉时刻路径"
        );
        assert_eq!(pacing.plan(100.0, 100.0 + 1.0), PresentPlan::Now);
    }

    /// 慢帧 / 唤醒抖动序列（120 Hz，GPU 线程每次都在 vsync 之后 7 ms 才被唤醒）绝不能退化成
    /// 「每 3 个 vsync 只提交 1 帧」。
    ///
    /// 复刻 AR3 复审的探针（`probe_pacing2.rs`：120 Hz、唤醒延迟 7 ms ⇒ 提交 40 / 跳过 80 =
    /// 40 fps）。现在断言：**最多跳过 1 个** vsync，之后保持「尽快呈现」，提交数接近 vsync 数。
    #[test]
    fn a_slow_120hz_frame_series_never_drops_to_one_third_rate() {
        const WAKE_DELAY: f64 = 0.007;
        const VSYNCS: u32 = 30;
        let mut pacing = PresentPacing::new();
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
            clamp_present_time(target, target - PRESENT_MARGIN_SECS / 2.0),
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
