//! 拖拽：阈值判定与位移（会话状态在 `Window.pointer.drag`）。
//!
//! 引擎**不**在拖拽里改布局：应用在自己的 handler 里写 `LayoutOverride` 或 `Signal`
//! （`design/phase-10-interaction.md` §10）。

use crate::math::Point;

/// 位移是否超过「拖动 vs 点击」阈值（超过即抑制点击）。
#[must_use]
pub(crate) fn crosses_threshold(delta: Point, distance: f32) -> bool {
    delta.x.abs() > distance || delta.y.abs() > distance
}

/// 两点之间的曼哈顿式「超阈值」判定（用于双击距离：任一轴超过即不算同一击）。
#[must_use]
pub(crate) fn within(a: Point, b: Point, distance: f32) -> bool {
    (a.x - b.x).abs() <= distance && (a.y - b.y).abs() <= distance
}

#[cfg(test)]
mod tests {
    use super::{crosses_threshold, within};
    use crate::math::Point;

    #[test]
    fn threshold_is_inclusive_on_both_axes() {
        let limit = 4.0;

        assert!(!crosses_threshold(Point::new(4.0, 0.0), limit));
        assert!(!crosses_threshold(Point::new(0.0, -4.0), limit));
        assert!(crosses_threshold(Point::new(4.5, 0.0), limit));
        assert!(crosses_threshold(Point::new(0.0, -5.0), limit));
    }

    #[test]
    fn double_click_distance_is_a_box() {
        let origin = Point::new(10.0, 10.0);

        assert!(within(origin, Point::new(14.0, 6.0), 4.0));
        assert!(!within(origin, Point::new(14.1, 10.0), 4.0));
        assert!(!within(origin, Point::new(10.0, 5.0), 4.0));
    }
}
