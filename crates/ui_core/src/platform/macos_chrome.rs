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

#[cfg(test)]
mod tests {
    use super::{LightRect, traffic_light_inset};

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
}
