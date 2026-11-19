//! macOS 自绘标题栏**拖动区**的命中判定纯逻辑（X2）。
//!
//! # 坐标语义
//!
//! 拖动区是**逻辑窗口坐标**：客户区左上角为原点、x 向右、y 向下（与
//! `Window::set_drag_regions` 文档一致）。`GuiView` 实现了 `isFlipped == YES`，因此
//! `-[NSView convertPoint:fromView:nil]` 返回的 view 坐标本来就是左上原点、y 向下 ——
//! 平台层用 `mouseDown:` 的 `event.locationInWindow` 经同一转换得到引擎逻辑点，**不需要**
//! 再做 AppKit 左下原点的翻转。这里因此只做矩形包含判定。
//!
//! # 为什么在 `platform/` 根而不是 `platform/macos/` 下
//!
//! `platform::macos` 整模块只在 macOS 目标编译（objc2 依赖也是 macOS-only）；命中判定是纯
//! 函数，必须在开发机（Windows）单测 —— 与 [`crate::platform::macos_placement`]、
//! [`crate::platform::macos_close`]、[`crate::platform::macos_owner`] 同一模式。真机侧只
//! 负责把 `mouseDown:` 的逻辑点喂进来、命中时调 `performWindowDragWithEvent:`。

use crate::math::{Point, Rect};

/// 命中哪个拖动区（按传入顺序返回**第一个**命中的下标；无命中 = `None`）。
///
/// 判定用半开区间 `[x, x + width) × [y, y + height)`：右/下边界不属于区域（相邻区域可以
/// 无缝拼接而不重叠）。非正尺寸（空/负宽高）的区域按空集处理，永不命中；非有限坐标不命中
/// （避免 NaN 比较的未定义语义把点击误判成拖动）。
pub(crate) fn drag_region_at(regions: &[Rect], point: Point) -> Option<usize> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    regions.iter().position(|region| {
        let (x, y) = (region.origin.x, region.origin.y);
        let (width, height) = (region.size.width, region.size.height);
        width > 0.0
            && height > 0.0
            && point.x >= x
            && point.x < x + width
            && point.y >= y
            && point.y < y + height
    })
}

#[cfg(test)]
mod tests {
    use super::drag_region_at;
    use crate::math::{Point, Rect};

    fn point(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    /// 没有拖动区（应用的默认状态）⇒ 一次点击都不算拖动区。
    #[test]
    fn no_regions_never_hit() {
        assert_eq!(drag_region_at(&[], point(10.0, 10.0)), None);
    }

    /// 区域内部命中；区域外不命中。
    #[test]
    fn points_inside_and_outside_a_region() {
        let regions = [Rect::new(10.0, 20.0, 100.0, 40.0)];
        assert_eq!(drag_region_at(&regions, point(50.0, 30.0)), Some(0));
        assert_eq!(drag_region_at(&regions, point(9.99, 30.0)), None);
        assert_eq!(drag_region_at(&regions, point(50.0, 19.99)), None);
    }

    /// 边界语义：左上含、右下不含（半开区间）；右/下边界外一像素属于窗口内容。
    #[test]
    fn region_bounds_are_half_open() {
        let regions = [Rect::new(10.0, 20.0, 100.0, 40.0)];
        assert_eq!(
            drag_region_at(&regions, point(10.0, 20.0)),
            Some(0),
            "左上角含"
        );
        assert_eq!(
            drag_region_at(&regions, point(109.9, 59.9)),
            Some(0),
            "右下角内侧命中"
        );
        assert_eq!(
            drag_region_at(&regions, point(110.0, 30.0)),
            None,
            "右边界不含"
        );
        assert_eq!(
            drag_region_at(&regions, point(50.0, 60.0)),
            None,
            "下边界不含"
        );
    }

    /// y 向下语义（自绘顶部条带）：窗口顶部 y=0..38 的条带命中，窗口下方的同样 x 不命中。
    /// `GuiView` 是 flipped，`logical_point` 返回的正是这套坐标（无需再翻转）。
    #[test]
    fn regions_use_top_left_origin_with_y_pointing_down() {
        let regions = [Rect::new(0.0, 0.0, 400.0, 38.0)];
        assert_eq!(
            drag_region_at(&regions, point(200.0, 10.0)),
            Some(0),
            "顶部条带"
        );
        assert_eq!(
            drag_region_at(&regions, point(200.0, 500.0)),
            None,
            "窗口底部（y 向下更大）不在条带内"
        );
    }

    /// 多个区域：返回第一个命中的下标；相邻区域无缝拼接不重叠。
    #[test]
    fn multiple_regions_report_the_first_match() {
        let regions = [
            Rect::new(0.0, 0.0, 100.0, 30.0),
            Rect::new(100.0, 0.0, 100.0, 30.0),
            Rect::new(0.0, 0.0, 10.0, 10.0),
        ];
        assert_eq!(drag_region_at(&regions, point(5.0, 5.0)), Some(0));
        assert_eq!(drag_region_at(&regions, point(150.0, 5.0)), Some(1));
        assert_eq!(
            drag_region_at(&regions, point(100.0, 5.0)),
            Some(1),
            "拼接处归后一个区域"
        );
        assert_eq!(drag_region_at(&regions, point(250.0, 5.0)), None);
    }

    /// 非正尺寸（空/负宽高）的区域是空集，不命中；非有限点也不命中。
    #[test]
    fn degenerate_regions_and_points_never_hit() {
        let regions = [
            Rect::new(0.0, 0.0, 0.0, 30.0),
            Rect::new(0.0, 0.0, 100.0, 0.0),
            Rect::new(0.0, 0.0, -10.0, -10.0),
        ];
        assert_eq!(drag_region_at(&regions, point(0.0, 0.0)), None);
        let finite = [Rect::new(0.0, 0.0, 100.0, 30.0)];
        assert_eq!(drag_region_at(&finite, point(f32::NAN, 5.0)), None);
        assert_eq!(drag_region_at(&finite, point(5.0, f32::INFINITY)), None);
    }
}
