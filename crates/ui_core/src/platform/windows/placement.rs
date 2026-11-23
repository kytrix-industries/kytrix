use std::mem::size_of;

use windows::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
};

use crate::{
    math::{DeviceRect, LogicalSize, dp},
    platform::windows::dpi,
    window::WindowOptions,
};

pub(crate) fn work_areas() -> Vec<DeviceRect> {
    let mut areas: Vec<DeviceRect> = Vec::with_capacity(2);
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_work_area),
            windows::Win32::Foundation::LPARAM((&mut areas as *mut Vec<DeviceRect>) as isize),
        );
    }
    areas
}

unsafe extern "system" fn collect_work_area(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    let areas = unsafe { &mut *(data.0 as *mut Vec<DeviceRect>) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        areas.push(DeviceRect::new(
            dp(info.rcWork.left),
            dp(info.rcWork.top),
            dp(info.rcWork.right),
            dp(info.rcWork.bottom),
        ));
    }
    windows::core::BOOL::from(true)
}

pub(crate) fn clamp_to_work_area(target: DeviceRect, work_areas: &[DeviceRect]) -> DeviceRect {
    let Some(area) = nearest_work_area(target, work_areas) else {
        return target;
    };
    let width = target.width();
    let height = target.height();
    let x = if width <= area.width() {
        target.x0.max(area.x0).min(area.x1 - width)
    } else {
        area.x0
    };
    let y = if height <= area.height() {
        target.y0.max(area.y0).min(area.y1 - height)
    } else {
        area.y0
    };
    DeviceRect::from_edges(x, y, x + width, y + height)
}

fn nearest_work_area(target: DeviceRect, work_areas: &[DeviceRect]) -> Option<DeviceRect> {
    let mut best: Option<(DeviceRect, i64, i64)> = None;
    for &area in work_areas {
        let intersection = target.intersect(area).area();
        let distance = gap_distance_squared(target, area);
        let replaces = match best {
            None => true,
            Some((_, best_intersection, best_distance)) => {
                match (intersection > 0, best_intersection > 0) {
                    (true, false) => true,
                    (false, true) => false,
                    (true, true) => intersection > best_intersection,
                    (false, false) => distance < best_distance,
                }
            }
        };
        if replaces {
            best = Some((area, intersection, distance));
        }
    }
    best.map(|(area, _, _)| area)
}

fn gap_distance_squared(a: DeviceRect, b: DeviceRect) -> i64 {
    let dx = (i64::from(b.x0.value()) - i64::from(a.x1.value()))
        .max(i64::from(a.x0.value()) - i64::from(b.x1.value()))
        .max(0);
    let dy = (i64::from(b.y0.value()) - i64::from(a.y1.value()))
        .max(i64::from(a.y0.value()) - i64::from(b.y1.value()))
        .max(0);
    dx * dx + dy * dy
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClientLimits {
    min: LogicalSize,
    max: Option<LogicalSize>,
}

impl ClientLimits {
    pub(crate) fn from_options(options: &WindowOptions) -> Self {
        let min = options.min_size.unwrap_or(dpi::MIN_SIZE_LOGICAL);
        let max = options.max_size;
        let min = match max {
            Some(max) => LogicalSize::new(min.width.min(max.width), min.height.min(max.height)),
            None => min,
        };
        Self { min, max }
    }

    pub(crate) fn clamp(self, size: LogicalSize) -> LogicalSize {
        let mut width = size.width.max(self.min.width);
        let mut height = size.height.max(self.min.height);
        if let Some(max) = self.max {
            width = width.min(max.width);
            height = height.min(max.height);
        }
        LogicalSize::new(width, height)
    }

    pub(crate) fn track_sizes(
        self,
        dpi: u32,
        decorations: crate::window::Decorations,
    ) -> ((u32, u32), Option<(u32, u32)>) {
        (
            dpi::track_size(self.min, dpi, decorations),
            self.max.map(|max| dpi::track_size(max, dpi, decorations)),
        )
    }
}

impl Default for ClientLimits {
    fn default() -> Self {
        Self {
            min: dpi::MIN_SIZE_LOGICAL,
            max: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientLimits, clamp_to_work_area, gap_distance_squared};
    use crate::{
        math::{DeviceRect, LogicalSize, dp, px},
        window::WindowOptions,
    };

    fn rect(x0: i32, y0: i32, width: i32, height: i32) -> DeviceRect {
        DeviceRect::new(dp(x0), dp(y0), dp(x0 + width), dp(y0 + height))
    }

    #[test]
    fn an_off_screen_window_is_moved_back_inside_the_work_area() {
        let work = [rect(0, 0, 1920, 1040)];
        assert_eq!(
            clamp_to_work_area(rect(2500, 100, 800, 600), &work),
            rect(1120, 100, 800, 600)
        );
        assert_eq!(
            clamp_to_work_area(rect(-2000, 100, 800, 600), &work),
            rect(0, 100, 800, 600)
        );
        assert_eq!(
            clamp_to_work_area(rect(100, 2000, 800, 600), &work),
            rect(100, 440, 800, 600)
        );
        assert_eq!(
            clamp_to_work_area(rect(100, -900, 800, 600), &work),
            rect(100, 0, 800, 600)
        );
    }

    #[test]
    fn an_oversized_window_is_aligned_to_the_work_area_top_left() {
        let work = [rect(0, 0, 1280, 720)];
        let placed = clamp_to_work_area(rect(-50, -50, 2000, 1500), &work);
        assert_eq!(placed, rect(0, 0, 2000, 1500), "只移动、不缩放");
    }

    #[test]
    fn a_window_that_already_fits_is_not_moved() {
        let work = [rect(0, 0, 1920, 1040)];
        assert_eq!(
            clamp_to_work_area(rect(0, 0, 800, 600), &work),
            rect(0, 0, 800, 600),
            "恰好贴左上边"
        );
        assert_eq!(
            clamp_to_work_area(rect(1120, 440, 800, 600), &work),
            rect(1120, 440, 800, 600),
            "恰好贴右下边"
        );
        assert_eq!(
            clamp_to_work_area(rect(1500, 800, 800, 600), &work),
            rect(1120, 440, 800, 600),
            "右下部分越界 ⇒ 只推到刚好完整可见"
        );
    }

    #[test]
    fn side_by_side_monitors_keep_the_window_on_its_own_monitor() {
        let work = [rect(0, 0, 1920, 1040), rect(1920, 0, 2560, 1400)];
        assert_eq!(
            clamp_to_work_area(rect(1800, 100, 800, 600), &work),
            rect(1920, 100, 800, 600)
        );
        assert_eq!(
            clamp_to_work_area(rect(1920 + 6000, 100, 800, 600), &work),
            rect(3680, 100, 800, 600)
        );
    }

    #[test]
    fn stacked_monitors_with_negative_origin_are_supported() {
        let work = [rect(0, -1080, 1920, 1080), rect(0, 0, 1920, 1040)];
        assert_eq!(
            clamp_to_work_area(rect(100, -1000, 800, 600), &work),
            rect(100, -1000, 800, 600)
        );
        assert_eq!(
            clamp_to_work_area(rect(100, -3000, 800, 600), &work),
            rect(100, -1080, 800, 600)
        );
        assert_eq!(
            clamp_to_work_area(rect(0, -200, 100, 100), &work),
            rect(0, -200, 100, 100)
        );
    }

    #[test]
    fn equally_near_work_areas_are_tie_broken_by_enumeration_order() {
        let work = [rect(0, 0, 100, 100), rect(300, 0, 100, 100)];
        assert_eq!(
            clamp_to_work_area(rect(150, 10, 100, 10), &work),
            rect(0, 10, 100, 10)
        );
    }

    #[test]
    fn no_work_areas_is_a_conservative_no_op() {
        let target = rect(5000, -3000, 800, 600);
        assert_eq!(clamp_to_work_area(target, &[]), target);
    }

    #[test]
    fn the_gap_distance_is_zero_for_intersecting_rects() {
        assert_eq!(
            gap_distance_squared(rect(0, 0, 10, 10), rect(5, 5, 10, 10)),
            0
        );
        assert_eq!(
            gap_distance_squared(rect(0, 0, 10, 10), rect(13, 14, 10, 10)),
            3 * 3 + 4 * 4
        );
        assert_eq!(
            gap_distance_squared(rect(0, 0, 10, 10), rect(-17, 10, 10, 10)),
            7 * 7
        );
    }

    #[test]
    fn client_limits_resolve_the_public_min_max_contract() {
        let default = ClientLimits::from_options(&WindowOptions::default());
        assert_eq!(default.min, LogicalSize::new(px(320.0), px(200.0)));
        assert_eq!(default.max, None);

        let options = WindowOptions {
            min_size: Some(LogicalSize::new(px(900.0), px(700.0))),
            max_size: Some(LogicalSize::new(px(600.0), px(500.0))),
            ..WindowOptions::default()
        };
        let limits = ClientLimits::from_options(&options);
        assert_eq!(
            limits.min,
            LogicalSize::new(px(600.0), px(500.0)),
            "min > max 时按 max 钳制（公共契约 min = max）"
        );
        assert_eq!(limits.max, Some(LogicalSize::new(px(600.0), px(500.0))));
        assert_eq!(
            limits.clamp(LogicalSize::new(px(100.0), px(100.0))),
            LogicalSize::new(px(600.0), px(500.0)),
            "小于 min 的请求被抬到 min"
        );
        assert_eq!(
            limits.clamp(LogicalSize::new(px(2000.0), px(2000.0))),
            LogicalSize::new(px(600.0), px(500.0)),
            "大于 max 的请求被压到 max"
        );

        let options = WindowOptions {
            max_size: Some(LogicalSize::new(px(200.0), px(100.0))),
            ..WindowOptions::default()
        };
        let limits = ClientLimits::from_options(&options);
        assert_eq!(limits.min, LogicalSize::new(px(200.0), px(100.0)));
    }
}
