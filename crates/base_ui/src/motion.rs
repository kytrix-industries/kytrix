//! Motion shared by more than one widget.
//!
//! A widget that animates inside its cached body drives the movement itself: it
//! advances the state once per frame and asks for the next frame only while
//! something is still visibly moving. The rules those movements follow live here
//! so two widgets that move the same kind of bar move it the same way.

use std::time::Duration;

/// How fast a depth bar closes on its new ratio, in e-folds per second.
pub const DEPTH_ANIMATION_RESPONSE_PER_SECOND: f32 = 12.0;

/// The movement a bar may still have left and be considered settled: easing the
/// rest of the way costs frames no one can see.
pub const DEPTH_ANIMATION_SETTLE_EPSILON: f32 = 0.001;

/// How far a depth-bar easing has come after `elapsed`, given where it started.
///
/// The approach is exponential rather than linear, so a bar that is interrupted
/// by another update keeps its speed instead of restarting, and the movement is
/// computed from the progress and the frame's elapsed time rather than from a
/// frame count, which makes it independent of how the frames happen to be spaced.
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

    #[test]
    fn depth_animation_should_advance_before_the_old_thirty_fps_interval() {
        let progress = depth_animation_progress_after(0.0, 1.0, Duration::from_millis(1));

        assert!(progress > 0.0);
        assert!(progress < 1.0);
    }

    #[test]
    fn depth_animation_should_not_be_forced_complete_at_150_ms() {
        let progress = depth_animation_progress_after(0.0, 1.0, Duration::from_millis(150));

        assert!(progress < 1.0);
    }

    #[test]
    fn depth_animation_should_be_independent_of_frame_partitioning() {
        let single = depth_animation_progress_after(0.0, 1.0, Duration::from_millis(33));
        let split = depth_animation_progress_after(
            depth_animation_progress_after(0.0, 1.0, Duration::from_millis(16)),
            1.0,
            Duration::from_millis(17),
        );

        assert!((single - split).abs() < 1e-6);
    }

    #[test]
    fn depth_animation_should_settle_after_visual_convergence() {
        let progress = depth_animation_progress_after(0.0, 1.0, Duration::from_secs(1));

        assert_eq!(progress, 1.0);
    }

    #[test]
    fn a_movement_smaller_than_anyone_can_see_should_arrive_at_once() {
        let progress = depth_animation_progress_after(
            0.0,
            DEPTH_ANIMATION_SETTLE_EPSILON / 2.0,
            Duration::ZERO,
        );

        assert_eq!(progress, 1.0);
    }

    #[test]
    fn a_progress_outside_the_unit_range_should_be_clamped() {
        assert_eq!(
            depth_animation_progress_after(2.0, 1.0, Duration::ZERO),
            1.0
        );
        assert_eq!(
            depth_animation_progress_after(-1.0, 1.0, Duration::ZERO),
            0.0
        );
    }
}
