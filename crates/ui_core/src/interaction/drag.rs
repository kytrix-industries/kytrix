use crate::math::{LogicalPoint, px};

#[must_use]
pub(crate) fn crosses_threshold(delta: LogicalPoint, distance: f32) -> bool {
    delta.x.abs() > px(distance) || delta.y.abs() > px(distance)
}

#[must_use]
pub(crate) fn within(a: LogicalPoint, b: LogicalPoint, distance: f32) -> bool {
    (a.x - b.x).abs() <= px(distance) && (a.y - b.y).abs() <= px(distance)
}

#[cfg(test)]
mod tests {
    use super::{crosses_threshold, within};
    use crate::math::{LogicalPoint, px};

    #[test]
    fn threshold_is_inclusive_on_both_axes() {
        let limit = 4.0;

        assert!(!crosses_threshold(
            LogicalPoint::new(px(4.0), px(0.0)),
            limit
        ));
        assert!(!crosses_threshold(
            LogicalPoint::new(px(0.0), px(-4.0)),
            limit
        ));
        assert!(crosses_threshold(
            LogicalPoint::new(px(4.5), px(0.0)),
            limit
        ));
        assert!(crosses_threshold(
            LogicalPoint::new(px(0.0), px(-5.0)),
            limit
        ));
    }

    #[test]
    fn double_click_distance_is_a_box() {
        let origin = LogicalPoint::new(px(10.0), px(10.0));

        assert!(within(origin, LogicalPoint::new(px(14.0), px(6.0)), 4.0));
        assert!(!within(origin, LogicalPoint::new(px(14.1), px(10.0)), 4.0));
        assert!(!within(origin, LogicalPoint::new(px(10.0), px(5.0)), 4.0));
    }
}
