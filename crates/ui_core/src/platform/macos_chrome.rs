use crate::math::Point;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LightRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub(crate) fn traffic_light_inset(lights: &[LightRect]) -> Option<Point> {
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    let mut any = false;
    for light in lights {
        if !(light.x.is_finite()
            && light.y.is_finite()
            && light.width.is_finite()
            && light.height.is_finite()
            && light.width > 0.0
            && light.height > 0.0)
        {
            continue;
        }
        any = true;
        right = right.max(light.x + light.width);
        bottom = bottom.max(light.y + light.height);
    }
    if !any {
        return None;
    }
    Some(Point::new(right.max(0.0) as f32, bottom.max(0.0) as f32))
}

pub(crate) fn traffic_light_position_to_apply(
    applied: Option<Point>,
    desired: Option<Point>,
) -> Option<Point> {
    match desired {
        Some(target) if applied.is_none() => Some(target),
        _ => None,
    }
}

pub(crate) fn should_refresh_traffic_light_inset(
    fullscreen: bool,
    refresh_pending: bool,
    cached: Option<Point>,
) -> bool {
    !fullscreen && (refresh_pending || cached.is_none())
}

#[cfg(test)]
mod tests {
    use super::{
        LightRect, should_refresh_traffic_light_inset, traffic_light_inset,
        traffic_light_position_to_apply,
    };
    use crate::math::Point;

    fn standard_lights() -> Vec<LightRect> {
        [(7.0, 16.0), (27.0, 16.0), (47.0, 16.0)]
            .into_iter()
            .map(|(x, y)| LightRect {
                x,
                y,
                width: 14.0,
                height: 16.0,
            })
            .collect()
    }

    #[test]
    fn the_standard_traffic_lights_occupy_their_union_bounds() {
        let inset = traffic_light_inset(&standard_lights()).expect("three lights");
        assert_eq!(inset.x, 61.0, "最右按钮的右边界 = 47 + 14");
        assert_eq!(inset.y, 32.0, "最低边界 = 16 + 16");
    }

    #[test]
    fn hidden_lights_have_no_safe_area() {
        assert_eq!(traffic_light_inset(&[]), None);
    }

    #[test]
    fn degenerate_frames_are_ignored() {
        let zero = LightRect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
        assert_eq!(traffic_light_inset(&[zero]), None);

        let nan = LightRect {
            x: f64::NAN,
            y: 0.0,
            width: 14.0,
            height: 16.0,
        };
        assert_eq!(traffic_light_inset(&[nan]), None);

        let mut lights = standard_lights();
        lights[1] = zero;
        let inset = traffic_light_inset(&lights).expect("two valid lights");
        assert_eq!(inset.x, 61.0);
        assert_eq!(inset.y, 32.0);
    }

    #[test]
    fn a_single_light_still_reports_its_bounds() {
        let close = LightRect {
            x: 12.0,
            y: 20.0,
            width: 14.0,
            height: 16.0,
        };
        let inset = traffic_light_inset(&[close]).expect("one light");
        assert_eq!(inset.x, 26.0);
        assert_eq!(inset.y, 36.0);
    }

    #[test]
    fn fractional_frames_keep_their_precision() {
        let light = LightRect {
            x: 7.5,
            y: 16.25,
            width: 13.5,
            height: 15.75,
        };
        let inset = traffic_light_inset(&[light]).expect("light");
        assert!((inset.x - 21.0).abs() < f32::EPSILON);
        assert!((inset.y - 32.0).abs() < f32::EPSILON);
    }

    #[test]
    fn traffic_light_position_applies_once_and_never_rolls_back() {
        let target = Point::new(8.0, 8.0);
        let moved = Point::new(40.0, 8.0);
        assert_eq!(
            traffic_light_position_to_apply(None, Some(target)),
            Some(target),
            "第一次请求必须应用"
        );
        assert_eq!(
            traffic_light_position_to_apply(Some(target), Some(target)),
            None,
            "已应用后同值重复调用不得再碰 AppKit"
        );
        assert_eq!(
            traffic_light_position_to_apply(Some(target), Some(moved)),
            None,
            "set-once：运行期改位置被忽略（不可撤销语义在这里固化）"
        );
        assert_eq!(
            traffic_light_position_to_apply(Some(target), None),
            None,
            "改回 None 不能恢复系统默认位置（不保存 AppKit 布局副本）"
        );
        assert_eq!(
            traffic_light_position_to_apply(None, None),
            None,
            "没有请求时不应用"
        );
    }

    #[test]
    fn the_safe_area_is_only_refreshed_when_the_buttons_could_have_moved() {
        let cached = Some(Point::new(61.0, 32.0));
        assert!(
            should_refresh_traffic_light_inset(false, true, cached),
            "位置应用 / 全屏转场之后必须刷新"
        );
        assert!(
            !should_refresh_traffic_light_inset(false, false, cached),
            "稳态（缓存有效且无待刷新）必须 0 次 AppKit 换算"
        );
        assert!(
            should_refresh_traffic_light_inset(false, false, None),
            "上次没有取到有效矩形（转场期 nil / NSZeroRect）时必须下一轮重试"
        );
        assert!(
            !should_refresh_traffic_light_inset(true, true, cached),
            "原生全屏下按钮由系统隐藏：安全区恒 None，不做换算"
        );
    }
}
