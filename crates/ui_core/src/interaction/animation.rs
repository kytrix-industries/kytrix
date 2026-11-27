use crate::paint::color::{Color, ColorResolver};

#[must_use]
pub(crate) fn progress(elapsed: std::time::Duration, duration: std::time::Duration) -> f32 {
    let total = duration.as_secs_f64();
    if total <= 0.0 {
        return 1.0;
    }
    (elapsed.as_secs_f64() / total).clamp(0.0, 1.0) as f32
}

#[must_use]
pub(crate) fn lerp(from: f32, to: f32, t: f32) -> f32 {
    if t >= 1.0 {
        return to;
    }
    from + (to - from) * t
}

#[must_use]
pub(crate) fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    if t >= 1.0 {
        return to;
    }
    if t <= 0.0 {
        return from;
    }
    ColorResolver.lerp_color(from, to, t)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{lerp, lerp_color, progress};
    use crate::paint::color::Color;

    #[test]
    fn progress_clamps_and_handles_zero_duration() {
        assert_eq!(progress(Duration::ZERO, Duration::from_secs(1)), 0.0);
        assert_eq!(
            progress(Duration::from_millis(500), Duration::from_secs(1)),
            0.5
        );
        assert_eq!(
            progress(Duration::from_secs(2), Duration::from_secs(1)),
            1.0
        );
        assert_eq!(progress(Duration::from_millis(1), Duration::ZERO), 1.0);
    }

    #[test]
    fn lerp_hits_the_endpoints_exactly() {
        assert_eq!(lerp(10.0, 20.0, 0.0), 10.0);
        assert_eq!(lerp(10.0, 20.0, 1.0), 20.0);
        assert_eq!(lerp(10.0, 20.0, 0.5), 15.0);
    }

    #[test]
    fn color_lerp_hits_the_endpoints_and_the_midpoint() {
        let from = Color::from_rgba8([0, 0, 0, 255]);
        let to = Color::from_rgba8([255, 255, 255, 255]);

        assert_eq!(lerp_color(from, to, 0.0), from);
        assert_eq!(lerp_color(from, to, 1.0), to);

        let mid = lerp_color(from, to, 0.5);
        let components = crate::paint::color::ColorResolver.resolve(mid).to_array();
        assert!(components[3] > 0.99, "alpha 必须保持不透明");
        for axis in &components[..3] {
            assert!(
                *axis > 0.0 && *axis < 1.0,
                "中点分量必须严格介于两端：{axis}"
            );
        }
    }

    #[test]
    fn transparent_endpoints_stay_transparent() {
        let clear = Color::linear_srgb(0.0, 0.0, 0.0, 0.0);
        let solid = Color::from_rgba8([10, 20, 30, 255]);

        let mixed = lerp_color(clear, solid, 0.0);
        assert!(mixed.is_transparent(), "t=0 原样返回 from");
        let half = lerp_color(clear, solid, 0.5);
        let alpha = crate::paint::color::ColorResolver.resolve(half).to_array()[3];
        assert!(alpha > 0.0, "半途必须有 alpha");
    }
}
