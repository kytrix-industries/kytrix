//! 动画：进度采样、标量/点/颜色插值（槽与推进在 `App::{animate, tick_animations}`）。
//!
//! 被驱动的值落 `Transient`（不进依赖图）；重绘由**显式注册的节点集合**的 `mark_paint` 给出

use crate::{
    math::Point,
    paint::color::{Color, ColorResolver},
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

/// 颜色插值：**线性预乘空间**（R05-7：与合成 / AA / 渐变同一套数学）。
///
/// 直接委托 [`ColorResolver::lerp_color`]：两端各自解析成线性预乘 → 在预乘分量上插值 →
/// 解回非预乘线性色。旧实现在 **sRGB 编码空间**插值（注释自述「按感知/屏幕空间走」），
/// 于是同一个主题色过渡到一半时，它作为背景（线性合成）与作为动画值（sRGB 插值）是两种颜色。
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
        let components = crate::paint::color::ColorResolver.resolve(mid).to_array();
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
        let alpha = crate::paint::color::ColorResolver.resolve(half).to_array()[3];
        assert!(alpha > 0.0, "半途必须有 alpha");
    }
}
