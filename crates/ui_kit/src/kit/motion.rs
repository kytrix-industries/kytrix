use std::time::Duration;

pub const DEPTH_ANIMATION_RESPONSE_PER_SECOND: f32 = 12.0;

pub const DEPTH_ANIMATION_SETTLE_EPSILON: f32 = 0.001;

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

    const FROZEN_SAMPLES: [(f32, f32, u64, f32); 30] = [
        (0.0, 1.0, 0, 0.0),
        (0.0, 1.0, 8, 0.091535985),
        (0.0, 1.0, 33, 0.3269933),
        (0.0, 1.0, 150, 0.8347011),
        (0.0, 1.0, 1000, 1.0),
        (0.0, 0.0005, 0, 1.0),
        (0.0, 0.0005, 8, 1.0),
        (0.0, 0.0005, 33, 1.0),
        (0.0, 0.0005, 150, 1.0),
        (0.0, 0.0005, 1000, 1.0),
        (0.25, 1.0, 0, 0.25),
        (0.25, 1.0, 8, 0.31865197),
        (0.25, 1.0, 33, 0.49524498),
        (0.25, 1.0, 150, 0.87602586),
        (0.25, 1.0, 1000, 1.0),
        (0.25, 0.0005, 0, 1.0),
        (0.25, 0.0005, 8, 1.0),
        (0.25, 0.0005, 33, 1.0),
        (0.25, 0.0005, 150, 1.0),
        (0.25, 0.0005, 1000, 1.0),
        (0.9, 1.0, 0, 0.9),
        (0.9, 1.0, 8, 0.9091536),
        (0.9, 1.0, 33, 0.9326993),
        (0.9, 1.0, 150, 0.9834701),
        (0.9, 1.0, 1000, 1.0),
        (0.9, 0.0005, 0, 1.0),
        (0.9, 0.0005, 8, 1.0),
        (0.9, 0.0005, 33, 1.0),
        (0.9, 0.0005, 150, 1.0),
        (0.9, 0.0005, 1000, 1.0),
    ];

    #[test]
    fn the_curve_matches_the_frozen_legacy_samples() {
        for (progress, span, millis, expected) in FROZEN_SAMPLES {
            assert_eq!(
                depth_animation_progress_after(progress, span, Duration::from_millis(millis)),
                expected,
                "progress={progress} span={span} elapsed={millis}ms 的曲线漂移"
            );
        }
    }

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
