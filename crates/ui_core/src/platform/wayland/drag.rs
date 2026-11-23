use crate::{
    event::{Event, InputEvent},
    math::{LogicalPoint, LogicalRect},
};

#[must_use]
pub(crate) fn drag_press_position(event: &Event) -> Option<LogicalPoint> {
    match event {
        Event::Input(InputEvent::PointerDown {
            position, button, ..
        }) if button.is_primary() => Some(*position),
        _ => None,
    }
}

#[must_use]
pub(crate) fn hit(regions: &[LogicalRect], point: LogicalPoint) -> bool {
    point.is_finite() && regions.iter().any(|region| region.contains(point))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{drag_press_position, hit};
    use crate::{
        event::{Event, InputEvent, PointerButton},
        math::{LogicalPoint, LogicalRect, px},
    };

    fn titlebar() -> LogicalRect {
        LogicalRect::new(px(0.0), px(0.0), px(800.0), px(40.0))
    }

    #[test]
    fn a_point_inside_a_single_region_hits() {
        assert!(
            hit(&[titlebar()], LogicalPoint::new(px(0.0), px(0.0))),
            "客户区左上角在条带内"
        );
        assert!(hit(&[titlebar()], LogicalPoint::new(px(400.0), px(20.0))));
        assert!(
            hit(&[titlebar()], LogicalPoint::new(px(799.5), px(39.5))),
            "右下边界内侧仍是条带"
        );
        for point in [
            LogicalPoint::new(px(-0.5), px(20.0)),
            LogicalPoint::new(px(400.0), px(-0.5)),
            LogicalPoint::new(px(800.0), px(20.0)),
            LogicalPoint::new(px(400.0), px(40.0)),
            LogicalPoint::new(px(400.0), px(400.0)),
        ] {
            assert!(!hit(&[titlebar()], point), "{point:?} 在条带外");
        }
    }

    #[test]
    fn the_region_bounds_are_half_open() {
        let strip = titlebar();
        assert!(hit(
            &[strip],
            LogicalPoint::new(strip.origin.x, strip.origin.y)
        ));
        assert!(hit(&[strip], LogicalPoint::new(strip.origin.x, px(39.9))));
        assert!(
            !hit(&[strip], LogicalPoint::new(strip.max().x, px(20.0))),
            "x == max.x 属于区域外"
        );
        assert!(
            !hit(&[strip], LogicalPoint::new(px(400.0), strip.max().y)),
            "y == max.y 属于区域外"
        );
    }

    #[test]
    fn multiple_regions_are_a_union() {
        let strip = titlebar();
        let handle = LogicalRect::new(px(0.0), px(100.0), px(24.0), px(80.0));
        let regions = [strip, handle];
        assert!(
            hit(&regions, LogicalPoint::new(px(400.0), px(20.0))),
            "顶部条带命中"
        );
        assert!(
            hit(&regions, LogicalPoint::new(px(10.0), px(120.0))),
            "侧栏把手命中"
        );
        assert!(
            !hit(&regions, LogicalPoint::new(px(400.0), px(60.0))),
            "两个区域之间（标题栏下方 40..100）不得命中"
        );
        assert!(
            !hit(&regions, LogicalPoint::new(px(40.0), px(120.0))),
            "把手宽度之外（x > 24）不得命中"
        );
    }

    #[test]
    fn no_regions_never_hit() {
        assert!(!hit(&[], LogicalPoint::new(px(0.0), px(0.0))));
        assert!(!hit(&[], LogicalPoint::new(px(400.0), px(20.0))));
    }

    #[test]
    fn negative_origin_regions_work() {
        let region = LogicalRect::new(px(-20.0), px(-10.0), px(40.0), px(30.0));
        assert!(
            hit(&[region], LogicalPoint::new(px(-20.0), px(-10.0))),
            "负原点本身命中"
        );
        assert!(hit(&[region], LogicalPoint::new(px(0.0), px(0.0))));
        assert!(
            !hit(&[region], LogicalPoint::new(px(20.0), px(-10.0))),
            "右边界外"
        );
        assert!(
            !hit(&[region], LogicalPoint::new(px(-20.5), px(0.0))),
            "左边界外"
        );
    }

    #[test]
    fn degenerate_regions_never_hit() {
        assert!(!hit(
            &[LogicalRect::new(px(0.0), px(0.0), px(0.0), px(40.0))],
            LogicalPoint::new(px(0.0), px(0.0))
        ));
        assert!(!hit(
            &[LogicalRect::new(px(0.0), px(0.0), px(800.0), px(0.0))],
            LogicalPoint::new(px(0.0), px(0.0))
        ));
        assert!(!hit(
            &[LogicalRect::new(px(0.0), px(0.0), px(-10.0), px(-10.0))],
            LogicalPoint::new(px(-5.0), px(-5.0))
        ));
    }

    #[test]
    fn huge_regions_hit_but_non_finite_points_never_do() {
        let huge = LogicalRect::new(px(-1.0e30), px(-1.0e30), px(2.0e30), px(2.0e30));
        assert!(hit(&[huge], LogicalPoint::new(px(0.0), px(0.0))));
        assert!(hit(&[huge], LogicalPoint::new(px(-9.0e29), px(-9.0e29))));
        assert!(hit(&[huge], LogicalPoint::new(px(9.0e29), px(9.0e29))));
        for point in [
            LogicalPoint::new(px(f32::NAN), px(0.0)),
            LogicalPoint::new(px(0.0), px(f32::NAN)),
            LogicalPoint::new(px(f32::INFINITY), px(0.0)),
            LogicalPoint::new(px(0.0), px(f32::NEG_INFINITY)),
        ] {
            assert!(!hit(&[titlebar()], point), "{point:?} 不是有限坐标");
        }
    }

    #[test]
    fn only_a_primary_press_starts_a_drag() {
        let at = LogicalPoint::new(px(120.0), px(20.0));
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
