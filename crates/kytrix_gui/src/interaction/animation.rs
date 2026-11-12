//! 动画：进度采样、标量/点/颜色插值（槽与推进在 `App::{animate, tick_animations}`）。
//!
//! 被驱动的值落 `Transient`（不进依赖图）；重绘由**显式注册的节点集合**的 `mark_paint` 给出
//! （`design/phase-10-interaction.md` §11）。

use crate::{
    math::Point,
    paint::color::{Color, ColorResolver, Interpolation},
};

/// 进度（`0..=1`；`duration == 0` ⇒ 立即 1.0）。
#[must_use]
pub(crate) fn progress(elapsed: std::time::Duration, duration: std::time::Duration) -> f32 {
    let total = duration.as_secs_f64();
    if total <= 0.0 {
        return 1.0;
    }
    (elapsed.as_secs_f64() / total).clamp(0.0, 1.0) as f32
}

/// 标量插值（`t >= 1.0` 时精确返回 `to`）。
#[must_use]
pub(crate) fn lerp(from: f32, to: f32, t: f32) -> f32 {
    if t >= 1.0 {
        return to;
    }
    from + (to - from) * t
}

/// 点插值。
#[must_use]
pub(crate) fn lerp_point(from: Point, to: Point, t: f32) -> Point {
    Point::new(lerp(from.x, to.x, t), lerp(from.y, to.y, t))
}

/// 颜色插值：**sRGB 分量**线性插值（设计 §11.2：主题色过渡要按感知/屏幕空间走，而不是线性光空间）。
///
/// 步骤：各端点解析成线性预乘 → 反预乘 + gamma 编码到 sRGB → 逐分量插值 → gamma 解码回线性 →
/// 用非预乘的结果构造 `Color`（端点精确相等，`t` 只会落在两者之间）。
#[must_use]
pub(crate) fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    if t >= 1.0 {
        return to;
    }
    if t <= 0.0 {
        return from;
    }
    let resolver = ColorResolver::new(Interpolation::default());
    let start = resolver.resolve(from).to_array();
    let end = resolver.resolve(to).to_array();
    let start = to_srgb(start);
    let end = to_srgb(end);
    let mixed = [
        lerp(start[0], end[0], t),
        lerp(start[1], end[1], t),
        lerp(start[2], end[2], t),
        lerp(start[3], end[3], t),
    ];
    let alpha = mixed[3];
    if alpha <= 0.0 {
        return Color::linear_srgb(0.0, 0.0, 0.0, 0.0);
    }
    let linear = [
        from_srgb(mixed[0]),
        from_srgb(mixed[1]),
        from_srgb(mixed[2]),
    ];
    Color::linear_srgb(linear[0], linear[1], linear[2], alpha)
}

/// 线性预乘 → 非预乘 + sRGB 编码（gamma 传输函数，逐分量）。
fn to_srgb(premultiplied: [f32; 4]) -> [f32; 4] {
    let alpha = premultiplied[3];
    if alpha <= 0.0 {
        return [0.0; 4];
    }
    [
        srgb_encode(premultiplied[0] / alpha),
        srgb_encode(premultiplied[1] / alpha),
        srgb_encode(premultiplied[2] / alpha),
        alpha,
    ]
}

/// sRGB 编码（线性 → sRGB；IEC 61966-2-1 分段函数）。
fn srgb_encode(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB 解码（sRGB → 线性）。
fn from_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{lerp, lerp_color, lerp_point, progress};
    use crate::{math::Point, paint::color::Color};

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
        assert_eq!(
            lerp_point(Point::new(0.0, 0.0), Point::new(10.0, 20.0), 0.5),
            Point::new(5.0, 10.0)
        );
    }

    #[test]
    fn color_lerp_hits_the_endpoints_and_the_midpoint() {
        let from = Color::from_rgba8([0, 0, 0, 255]);
        let to = Color::from_rgba8([255, 255, 255, 255]);

        assert_eq!(lerp_color(from, to, 0.0), from);
        assert_eq!(lerp_color(from, to, 1.0), to);

        let mid = lerp_color(from, to, 0.5);
        let components = crate::paint::color::ColorResolver::new(Default::default())
            .resolve(mid)
            .to_array();
        // 线性空间的中点亮度介于两端之间（sRGB 非线性 ⇒ 不会正好是 0.5 的 8 位值）。
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
        let alpha = crate::paint::color::ColorResolver::new(Default::default())
            .resolve(half)
            .to_array()[3];
        assert!(alpha > 0.0, "半途必须有 alpha");
    }
}
