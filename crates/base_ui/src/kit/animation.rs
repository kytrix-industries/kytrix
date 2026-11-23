use std::time::Duration;

use crate::theme::components::ComponentTokens;

#[must_use]
pub fn animation_tick() -> Duration {
    ComponentTokens::standard().animation_tick
}

#[must_use]
pub fn popup_should_render(opened: bool, progress: f32) -> bool {
    opened || progress > 0.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ui_core::interaction::{AnimatedValue, AnimationSpec, Easing};

    use super::{animation_tick, popup_should_render};
    use crate::kit::test_support::test_app;

    #[test]
    fn the_animation_tick_is_the_themed_legacy_interval() {
        assert_eq!(animation_tick(), Duration::from_millis(150));
    }

    #[test]
    fn the_truth_table_matches_the_legacy_intent() {
        assert!(!popup_should_render(false, 0.0));
        assert!(popup_should_render(false, 0.001));
        assert!(popup_should_render(false, 1.0));
        assert!(popup_should_render(true, 0.0));
        assert!(popup_should_render(true, 1.0));
        assert!(!popup_should_render(false, f32::NAN));
    }

    #[test]
    fn the_engine_animation_clock_drives_the_opacity_transient() {
        let mut app = test_app();
        let window = app.window_id();
        let opacity = app.app_mut().transient(0.0f32);
        let spec = AnimationSpec {
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        };
        app.app_mut()
            .animate(
                window,
                AnimatedValue::Scalar {
                    target: opacity,
                    to: 1.0,
                },
                &[],
                spec,
            )
            .expect("注册动画");
        assert!(app.app().animations_running(window), "动画已注册");

        let _ = app.drain().expect("首帧");
        app.advance(Duration::from_millis(150));
        let _ = app.drain().expect("动画帧");
        let value = *app.app().read_transient(&opacity);
        assert_eq!(value, 1.0, "动画结束后瞬态值必须到目标值");
        assert!(!app.app().animations_running(window), "动画结束即停帧");
    }
}
