use crate::math::Point;

#[must_use]
pub(crate) fn crosses_threshold(delta: Point, distance: f32) -> bool {
    delta.x.abs() > distance || delta.y.abs() > distance
}

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
