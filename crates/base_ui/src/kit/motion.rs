//! 深度条缓动的响应曲线（旧 `crate::motion` 的引擎化移植）。
//!
//! 纯数学：给「已经走了多少进度 + 这一帧过了多久」，算出新进度。**不建帧循环**：调用方（组件）
//! 在引擎 `App::animate` / `Transient` 的驱动下按帧调用本函数；本模块只保证「同一段时间无论被
//! 切成几帧，结果一致」这一条（指数响应而不是线性逼近）。
//!
//! 数值与旧实现逐位一致（常量 + 公式照搬；测试直接对旧函数现场比对，不硬编码期望值）。

use std::time::Duration;

/// 深度条向新比例收敛的速度（每秒 e 折数）。
pub const DEPTH_ANIMATION_RESPONSE_PER_SECOND: f32 = 12.0;

/// 允许「已经看不见了」的剩余位移：剩这么点就当作到位（再算下去只是白烧帧）。
pub const DEPTH_ANIMATION_SETTLE_EPSILON: f32 = 0.001;

/// 走了 `elapsed` 之后的总进度（`0..=1`）。
///
/// 语义：
/// - `progress` 先 clamp 到 `0..=1`（上游瞬态值可能短暂越界）；
/// - 指数收敛：`remaining = (1 - progress) * exp(-12 * elapsed)`，所以被打断后保留速度、不重启；
/// - 剩余**可见位移**（`transition_span * remaining`）小于 `EPSILON` 时直接到位（返回 `1.0`），
///   避免永远差一点的无限出帧。
#[must_use]
pub fn depth_animation_progress_after(
    progress: f32,
    transition_span: f32,
    elapsed: Duration,
) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    let remaining_progress =
        (1.0 - progress) * (-DEPTH_ANIMATION_RESPONSE_PER_SECOND * elapsed.as_secs_f32()).exp();
    if transition_span * remaining_progress <= DEPTH_ANIMATION_SETTLE_EPSILON {
        1.0
    } else {
        1.0 - remaining_progress
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEPTH_ANIMATION_SETTLE_EPSILON, depth_animation_progress_after};

    /// 旧实现与新实现在一组输入上逐位一致（旧模块还在 crate 里，现场比对）。
    #[test]
    fn the_curve_is_bit_identical_to_the_legacy_implementation() {
        let mut checked = 0usize;
        for progress in [0.0, 0.1, 0.5, 0.9, 1.0, -1.0, 2.0] {
            for span in [0.0, 0.001, 0.5, 1.0, 42.0] {
                for millis in [0u64, 1, 8, 16, 33, 100, 150, 1000] {
                    let elapsed = Duration::from_millis(millis);
                    assert_eq!(
                        depth_animation_progress_after(progress, span, elapsed),
                        crate::motion::depth_animation_progress_after(progress, span, elapsed),
                        "progress={progress} span={span} elapsed={millis}ms 的曲线漂移"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked >= 100, "抽样数必须足够（实际 {checked}）");
    }

    /// 语义旧测试（保留旧用例的断言口径）：1 ms 内已经动、150 ms 未完成、结算阈值、越界 clamp。
    #[test]
    fn the_legacy_semantics_are_preserved() {
        let early = depth_animation_progress_after(0.0, 1.0, Duration::from_millis(1));
        assert!(early > 0.0 && early < 1.0, "1 ms 内必须已经动起来");

        let mid = depth_animation_progress_after(0.0, 1.0, Duration::from_millis(150));
        assert!(mid < 1.0, "150 ms 时不能强行算作完成");

        assert_eq!(
            depth_animation_progress_after(0.0, 1.0, Duration::from_secs(1)),
            1.0,
            "1 s 后必须收敛"
        );
        assert_eq!(
            depth_animation_progress_after(
                0.0,
                DEPTH_ANIMATION_SETTLE_EPSILON / 2.0,
                Duration::ZERO
            ),
            1.0,
            "看不见的位移应直接到位"
        );
        assert_eq!(
            depth_animation_progress_after(2.0, 1.0, Duration::ZERO),
            1.0
        );
        assert_eq!(
            depth_animation_progress_after(-1.0, 1.0, Duration::ZERO),
            0.0
        );
    }

    /// 帧切分无关：33 ms 一次 == 16 ms + 17 ms 两次（旧实现的核心承诺）。
    #[test]
    fn the_curve_is_independent_of_frame_partitioning() {
        let single = depth_animation_progress_after(0.0, 1.0, Duration::from_millis(33));
        let split = depth_animation_progress_after(
            depth_animation_progress_after(0.0, 1.0, Duration::from_millis(16)),
            1.0,
            Duration::from_millis(17),
        );
        assert!((single - split).abs() < 1e-6);
    }
}
