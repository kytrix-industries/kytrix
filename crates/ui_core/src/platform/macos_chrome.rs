//! macOS 交通灯（红 / 黄 / 绿）安全区的**平台无关纯逻辑**（E6 §8.6.5）。
//!
//! `platform::macos` 整个模块 macOS-only（objc2 依赖），而「三个标准按钮占多大一块安全区」是
//! 纯几何：把按钮在**内容区坐标系**（左上原点、y 向下；logical point）里的 frame 交给
//! [`traffic_light_inset`]，得到应用需要留白的 `(right, bottom)`。这样开发机（Windows）的
//! `cargo test` 也能覆盖边界（空集 / 部分按钮缺失 / 半像素 / 负坐标），不必只在真机上肉眼看。
//!
//! 语义（与 `App::traffic_light_inset` 的公共契约一致）：
//! - 返回值是**占用区域的外边界**（`x + width` 的最大值、`y + height` 的最大值），不额外加
//!   padding —— 应用自己决定留白，引擎不猜「按钮到可点控件的间距」；
//! - 一个按钮都取不到（原生全屏隐藏 / `standardWindowButton:` 返回 nil）⇒ `None`；
//! - 非有限 / 非正尺寸的矩形被忽略（AppKit 在转场期间可能给出 `NSZeroRect`）。

use crate::math::Point;

/// 一个交通灯按钮在**内容区坐标系**里的矩形（logical point，左上原点、y 向下）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LightRect {
    /// 左边界。
    pub x: f64,
    /// 上边界。
    pub y: f64,
    /// 宽（必须 > 0 才参与计算）。
    pub width: f64,
    /// 高（必须 > 0 才参与计算）。
    pub height: f64,
}

/// 三个交通灯占用的安全区：`Point::new(max_right, max_bottom)`（logical point）。
///
/// `None` = 没有可用的按钮矩形（原生全屏 / 窗口尚未布局 / 全部无效）。应用据此决定留白高度与
/// 左侧宽度（§8.6.5：可点击控件不得放到交通灯下，会与系统按钮抢命中）。
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
    // 负坐标（内容区上方 / 左方的异常 frame）对「留白」没有意义：钳到 0。
    Some(Point::new(right.max(0.0) as f32, bottom.max(0.0) as f32))
}

/// 交通灯位置 **set-once** 决策（E6 §8.6.5；平台无关、可在开发机单测）。
///
/// `applied` = 已经写进三个 `standardWindowButton:` 的位置（`None` = 尚未应用）。返回本轮需要
/// 应用的目标位置：**只在第一次**看到 `Some(desired)` 时应用；之后运行期再改（含改回 `None`）
/// 不再调整 —— `standardWindowButton:` 的 frame 是 AppKit 私有布局，引擎不保存系统默认布局的
/// 副本，因此「恢复默认位置」没有可靠路径（文档化限制，见 `App::set_traffic_light_position`）。
pub(crate) fn traffic_light_position_to_apply(
    applied: Option<Point>,
    desired: Option<Point>,
) -> Option<Point> {
    match desired {
        Some(target) if applied.is_none() => Some(target),
        _ => None,
    }
}

/// 交通灯安全区缓存是否需要刷新（E6 §8.6.5；平台无关、可在开发机单测）。
///
/// 安全区只能在 `standardWindowButton:` 布局变化后重算（每轮 3 次 `convertRect:toView:` 是纯
/// 浪费）。刷新时机：首次、位置应用之后、全屏转场（进出全屏时 AppKit 隐藏 / 恢复按钮）、以及
/// 上次没有取到有效矩形时（转场期间可能是 `nil` / `NSZeroRect`，必须下一轮重试）。
/// `fullscreen == true` 时按钮由系统隐藏 ⇒ 安全区恒 `None`，不查询。
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

    /// macOS 13+ 的默认布局：关闭 / 最小化 / 缩放三个 14×16 按钮，间距 6 pt，左边距 7 pt、
    /// 上边距 16 pt（fixture 取整后与真机量级一致；具体值随系统版本变化，纯函数只认矩形）。
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

    /// 原生全屏：AppKit 隐藏标准按钮（`standardWindowButton:` 返回 nil）⇒ 没有安全区。
    #[test]
    fn hidden_lights_have_no_safe_area() {
        assert_eq!(traffic_light_inset(&[]), None);
    }

    /// 全屏转场 / 尚未布局：`NSZeroRect` 或非有限 frame 被忽略；只剩有效按钮时按剩余矩形算。
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

    /// 只有一个按钮（部分系统 / 自定义 styleMask）：安全区 = 该按钮的右下边界。
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

    /// 半像素（分数点）不丢精度；返回的是 f32 逻辑点（引擎的几何契约）。
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

    /// E6 §8.6.5 / P3（审查 F7）：交通灯位置是 **set-once** —— 首次请求应用一次，运行期再改
    /// （含同值重复）不再调用 AppKit；改回 `None` 不可撤销（不保存系统默认布局副本）。
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

    /// E6 §8.6.5 / P3（审查 F7）：安全区只在「首次 / 全屏转场 / 位置应用之后 / 上次无效」时
    /// 重算 —— 稳态每轮 0 次 `standardWindowButton:` + `convertRect:toView:`。
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
