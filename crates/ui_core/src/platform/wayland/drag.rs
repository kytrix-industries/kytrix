use crate::{
    event::{Event, InputEvent},
    math::{Point, Rect},
};

#[must_use]
pub(crate) fn drag_press_position(event: &Event) -> Option<Point> {
    match event {
        Event::Input(InputEvent::PointerDown {
            position, button, ..
        }) if button.is_primary() => Some(*position),
        _ => None,
    }
}

#[must_use]
pub(crate) fn hit(regions: &[Rect], point: Point) -> bool {
    point.is_finite() && regions.iter().any(|region| region.contains(point))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{drag_press_position, hit};
    use crate::{
        event::{Event, InputEvent, PointerButton},
        math::{Point, Rect},
    };

    fn titlebar() -> Rect {
        Rect::new(0.0, 0.0, 800.0, 40.0)
    }

    #[test]
    fn a_point_inside_a_single_region_hits() {
        assert!(
            hit(&[titlebar()], Point::new(0.0, 0.0)),
            "客户区左上角在条带内"
        );
        assert!(hit(&[titlebar()], Point::new(400.0, 20.0)));
        assert!(
            hit(&[titlebar()], Point::new(799.5, 39.5)),
            "右下边界内侧仍是条带"
        );
        for point in [
            Point::new(-0.5, 20.0),
            Point::new(400.0, -0.5),
            Point::new(800.0, 20.0),
            Point::new(400.0, 40.0),
            Point::new(400.0, 400.0),
        ] {
            assert!(!hit(&[titlebar()], point), "{point:?} 在条带外");
        }
    }

    #[test]
    fn the_region_bounds_are_half_open() {
        let strip = titlebar();
        assert!(hit(&[strip], Point::new(strip.origin.x, strip.origin.y)));
        assert!(hit(&[strip], Point::new(strip.origin.x, 39.9)));
        assert!(
            !hit(&[strip], Point::new(strip.max().x, 20.0)),
            "x == max.x 属于区域外"
        );
        assert!(
            !hit(&[strip], Point::new(400.0, strip.max().y)),
            "y == max.y 属于区域外"
        );
    }

    #[test]
    fn multiple_regions_are_a_union() {
        let strip = titlebar();
        let handle = Rect::new(0.0, 100.0, 24.0, 80.0);
        let regions = [strip, handle];
        assert!(hit(&regions, Point::new(400.0, 20.0)), "顶部条带命中");
        assert!(hit(&regions, Point::new(10.0, 120.0)), "侧栏把手命中");
        assert!(
            !hit(&regions, Point::new(400.0, 60.0)),
            "两个区域之间（标题栏下方 40..100）不得命中"
        );
        assert!(
            !hit(&regions, Point::new(40.0, 120.0)),
            "把手宽度之外（x > 24）不得命中"
        );
    }

    #[test]
    fn no_regions_never_hit() {
        assert!(!hit(&[], Point::new(0.0, 0.0)));
        assert!(!hit(&[], Point::new(400.0, 20.0)));
    }

    #[test]
    fn negative_origin_regions_work() {
        let region = Rect::new(-20.0, -10.0, 40.0, 30.0);
        assert!(hit(&[region], Point::new(-20.0, -10.0)), "负原点本身命中");
        assert!(hit(&[region], Point::new(0.0, 0.0)));
        assert!(!hit(&[region], Point::new(20.0, -10.0)), "右边界外");
        assert!(!hit(&[region], Point::new(-20.5, 0.0)), "左边界外");
    }

    #[test]
    fn degenerate_regions_never_hit() {
        assert!(!hit(
            &[Rect::new(0.0, 0.0, 0.0, 40.0)],
            Point::new(0.0, 0.0)
        ));
        assert!(!hit(
            &[Rect::new(0.0, 0.0, 800.0, 0.0)],
            Point::new(0.0, 0.0)
        ));
        assert!(!hit(
            &[Rect::new(0.0, 0.0, -10.0, -10.0)],
            Point::new(-5.0, -5.0)
        ));
    }

    #[test]
    fn huge_regions_hit_but_non_finite_points_never_do() {
        let huge = Rect::new(-1.0e30, -1.0e30, 2.0e30, 2.0e30);
        assert!(hit(&[huge], Point::new(0.0, 0.0)));
        assert!(hit(&[huge], Point::new(-9.0e29, -9.0e29)));
        assert!(hit(&[huge], Point::new(9.0e29, 9.0e29)));
        for point in [
            Point::new(f32::NAN, 0.0),
            Point::new(0.0, f32::NAN),
            Point::new(f32::INFINITY, 0.0),
            Point::new(0.0, f32::NEG_INFINITY),
        ] {
            assert!(!hit(&[titlebar()], point), "{point:?} 不是有限坐标");
        }
    }

    #[test]
    fn only_a_primary_press_starts_a_drag() {
        let at = Point::new(120.0, 20.0);
        let primary = Event::Input(InputEvent::PointerDown {
            position: at,
            button: PointerButton::Primary,
            modifiers: crate::event::Modifiers::empty(),
            timestamp: Instant::now(),
        });
        assert_eq!(drag_press_position(&primary), Some(at));

        for button in [
            PointerButton::Secondary,
            PointerButton::Middle,
            PointerButton::X1,
            PointerButton::X2,
        ] {
            let press = Event::Input(InputEvent::PointerDown {
                position: at,
                button,
                modifiers: crate::event::Modifiers::empty(),
                timestamp: Instant::now(),
            });
            assert_eq!(
                drag_press_position(&press),
                None,
                "{button:?} 的按下不得触发窗口拖动"
            );
        }

        let events = [
            Event::Input(InputEvent::PointerMove {
                position: at,
                modifiers: crate::event::Modifiers::empty(),
                timestamp: Instant::now(),
            }),
            Event::Input(InputEvent::PointerUp {
                position: at,
                button: PointerButton::Primary,
                modifiers: crate::event::Modifiers::empty(),
                timestamp: Instant::now(),
            }),
            Event::Input(InputEvent::PointerLeave {
                position: at,
                modifiers: crate::event::Modifiers::empty(),
                timestamp: Instant::now(),
            }),
            Event::Input(InputEvent::PointerCancel {
                timestamp: Instant::now(),
            }),
            Event::FocusChanged { focused: true },
        ];
        for event in &events {
            assert_eq!(
                drag_press_position(event),
                None,
                "只有主键按下才算拖动开始：{event:?}"
            );
        }
    }
}
