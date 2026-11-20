//! 初始窗口落点（B3）：把 `WindowOptions.position` / `min_size` / `max_size` 解析成平台几何。
//!
//! ## 为什么需要 clamp 到工作区
//!
//! placement 的持久化归**应用层**（公共契约见 [`crate::window::WindowOptions::position`]）：
//! 应用从 `Event::Moved` / `Event::Resized` 记录，下次启动填回 `WindowOptions`。显示器拔掉、
//! 分辨率变小、多屏改布局之后，旧位置可能落在任何显示器之外 —— 不 clamp 就把窗口恢复到用户
//! 够不着的地方（Chrome 的 `WindowPlacement`、Qt 的 `QWidget::restoreGeometry` 都会把窗口
//! 移回可见区域）。
//!
//! ## 纯逻辑与薄壳
//!
//! - [`clamp_to_work_area`]：纯函数（输入是物理矩形列表），全部边界可在 Windows 单测覆盖
//!   （负坐标、全屏外、恰好贴边、左右/上下双屏、无显示器回退）；
//! - [`work_areas`]：显示器枚举薄壳（`EnumDisplayMonitors` + `GetMonitorInfoW`），只做系统
//!   调用，不做几何决策 —— 决策全在纯函数里。
//!
//! 坐标全部是**物理像素、虚拟桌面左上角为原点**（与 `GetWindowRect` / `MONITORINFO.rcWork`
//! 同域）。

use std::mem::size_of;

use windows::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
};

use crate::{
    math::{PhysicalRect, Size},
    platform::windows::dpi,
    window::WindowOptions,
};

/// 全部显示器的工作区（物理像素，虚拟桌面坐标；`EnumDisplayMonitors` 顺序）。
///
/// 枚举失败 / 没有显示器（会话 0 服务、远程会话的某些阶段）⇒ 空 `Vec`；
/// [`clamp_to_work_area`] 对空切片保守回退为「不 clamp」，不会把窗口堆到未定义的 (0, 0)。
pub(crate) fn work_areas() -> Vec<PhysicalRect> {
    let mut areas: Vec<PhysicalRect> = Vec::with_capacity(2);
    // SAFETY: 回调把 `&mut Vec` 经 `LPARAM` 传回；`EnumDisplayMonitors` **同步**枚举并在返回
    // 前不再回调，借用不跨越函数返回。`areas` 在枚举期间保持存活。
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_work_area),
            windows::Win32::Foundation::LPARAM((&mut areas as *mut Vec<PhysicalRect>) as isize),
        );
    }
    areas
}

/// `EnumDisplayMonitors` 回调：把一个显示器的工作区追加进 `LPARAM` 指向的 `Vec`。
///
/// 返回 `TRUE` = 继续枚举。`GetMonitorInfoW` 失败（显示器刚被拔掉）⇒ 跳过该显示器，不中断。
unsafe extern "system" fn collect_work_area(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    // SAFETY: `data` 是 `work_areas` 里 `&mut Vec<PhysicalRect>` 的指针；枚举同步进行，
    // 该引用在 `EnumDisplayMonitors` 返回前一直有效（见调用点注释）。回调不跨线程。
    let areas = unsafe { &mut *(data.0 as *mut Vec<PhysicalRect>) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `monitor` 由系统在枚举时给出；`info` 是栈上结构且已填 `cbSize`。
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        areas.push(PhysicalRect::new(
            info.rcWork.left,
            info.rcWork.top,
            info.rcWork.right,
            info.rcWork.bottom,
        ));
    }
    windows::core::BOOL::from(true)
}

/// 把窗口外框矩形移到最近的显示器工作区内（纯函数，可单测）。
///
/// 策略（单测与实现逐条对应）：
///
/// 1. **选择工作区**：与 `target` 交集面积最大的工作区；所有工作区都不相交时，取
///    「窗口与工作区之间的空隙欧氏距离」最近的一个。距离相同取枚举顺序在前者（确定性）。
///    主显示器标记不参与选择 —— 多屏布局里用户上次所在的屏通常就是交集最大的那个。
/// 2. **只移动、不缩放**：窗口能完整放进工作区 ⇒ 把整个窗口移进工作区（标题栏与四边都
///    可见）；窗口比工作区大 ⇒ 左上角对齐工作区左上角（**标题栏可见**，仍然可拖回）。
/// 3. `work_areas` 为空（枚举失败 / 无显示器）⇒ 保守回退：**原样返回**，不做任何猜测。
///
/// 返回值保持输入尺寸（`width` / `height` 不变）。
pub(crate) fn clamp_to_work_area(
    target: PhysicalRect,
    work_areas: &[PhysicalRect],
) -> PhysicalRect {
    let Some(area) = nearest_work_area(target, work_areas) else {
        return target;
    };
    let width = target.width();
    let height = target.height();
    // 能完整放下 ⇒ 夹进 `[工作区左上, 工作区右下 - 窗口尺寸]`；放不下 ⇒ 左上对齐。
    let x = if width <= area.width() {
        target.x0.clamp(area.x0, area.x1 - width)
    } else {
        area.x0
    };
    let y = if height <= area.height() {
        target.y0.clamp(area.y0, area.y1 - height)
    } else {
        area.y0
    };
    PhysicalRect::new(x, y, x.saturating_add(width), y.saturating_add(height))
}

/// 选择目标工作区：见 [`clamp_to_work_area`] 的策略 1。
fn nearest_work_area(target: PhysicalRect, work_areas: &[PhysicalRect]) -> Option<PhysicalRect> {
    let mut best: Option<(PhysicalRect, i64, i64)> = None;
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

/// 两个矩形之间的空隙距离平方（相交为 0；`i64` 避免 i32 端点相减溢出）。
fn gap_distance_squared(a: PhysicalRect, b: PhysicalRect) -> i64 {
    let dx = (i64::from(b.x0) - i64::from(a.x1))
        .max(i64::from(a.x0) - i64::from(b.x1))
        .max(0);
    let dy = (i64::from(b.y0) - i64::from(a.y1))
        .max(i64::from(a.y0) - i64::from(b.y1))
        .max(0);
    dx * dx + dy * dy
}

/// 解析后的窗口客户区尺寸约束（逻辑像素）。
///
/// 来源是 [`WindowOptions::min_size`] / [`WindowOptions::max_size`]；公共契约规定
/// `min > max` 时按 `max` 钳制（[`WindowOptions`] 的文档「引擎先 clamp 成 min = max」）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClientLimits {
    /// 最小客户区逻辑尺寸（`min > max` 时已按 `max` 钳制）。
    min: Size,
    /// 最大客户区逻辑尺寸；`None` = 不额外限制（`WM_GETMINMAXINFO` 保留系统默认）。
    max: Option<Size>,
}

impl ClientLimits {
    /// 从建窗参数解析：`min_size = None` 用引擎默认最小尺寸
    /// （[`dpi::MIN_SIZE_LOGICAL`]；P2-1 的既有行为），`min > max` 时按 `max` 钳制。
    pub(crate) fn from_options(options: &WindowOptions) -> Self {
        let min = options.min_size.unwrap_or(dpi::MIN_SIZE_LOGICAL);
        let max = options.max_size;
        let min = match max {
            Some(max) => Size::new(min.width.min(max.width), min.height.min(max.height)),
            None => min,
        };
        Self { min, max }
    }

    /// 把请求的客户区尺寸钳进约束（逐维度）。
    pub(crate) fn clamp(self, size: Size) -> Size {
        let mut width = size.width.max(self.min.width);
        let mut height = size.height.max(self.min.height);
        if let Some(max) = self.max {
            width = width.min(max.width);
            height = height.min(max.height);
        }
        Size::new(width, height)
    }

    /// `WM_GETMINMAXINFO` 的两个跟踪尺寸：`(ptMinTrackSize, ptMaxTrackSize)`（**窗口外框**、
    /// 物理像素，随 DPI 换算；`max = None` 时不覆盖系统默认）。
    ///
    /// `decorations` 决定是否把系统标题栏 / 边框算进外框：frameless 的窗口外框 = 客户区
    /// （`WM_NCCALCSIZE` 返回 0），多算一圈会让最小跟踪尺寸凭空变大。
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

/// 引擎默认约束（`min_size = None` / `max_size = None`）：平台单测建窗口时用。
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
        math::{PhysicalRect, Size},
        window::WindowOptions,
    };

    fn rect(x0: i32, y0: i32, width: i32, height: i32) -> PhysicalRect {
        PhysicalRect::new(x0, y0, x0 + width, y0 + height)
    }

    /// 单屏工作区（1920×1040，任务栏占底部 40）：请求完全在屏幕外 ⇒ 整个窗口放进工作区。
    #[test]
    fn an_off_screen_window_is_moved_back_inside_the_work_area() {
        let work = [rect(0, 0, 1920, 1040)];
        // 完全在右侧之外。
        assert_eq!(
            clamp_to_work_area(rect(2500, 100, 800, 600), &work),
            rect(1120, 100, 800, 600)
        );
        // 完全在左侧（负坐标）之外。
        assert_eq!(
            clamp_to_work_area(rect(-2000, 100, 800, 600), &work),
            rect(0, 100, 800, 600)
        );
        // 完全在下方之外：底边贴工作区底边。
        assert_eq!(
            clamp_to_work_area(rect(100, 2000, 800, 600), &work),
            rect(100, 440, 800, 600)
        );
        // 完全在上方之外。
        assert_eq!(
            clamp_to_work_area(rect(100, -900, 800, 600), &work),
            rect(100, 0, 800, 600)
        );
    }

    /// 窗口比工作区大：只对齐左上角（保证标题栏可见），不缩放。
    #[test]
    fn an_oversized_window_is_aligned_to_the_work_area_top_left() {
        let work = [rect(0, 0, 1280, 720)];
        let placed = clamp_to_work_area(rect(-50, -50, 2000, 1500), &work);
        assert_eq!(placed, rect(0, 0, 2000, 1500), "只移动、不缩放");
    }

    /// 恰好贴边 / 一部分在外面但仍在工作区内：可见矩形不变（不移动）。
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

    /// 双屏左右并排：窗口在右屏时不因左屏工作区更大而被搬走（按交集面积选择）。
    #[test]
    fn side_by_side_monitors_keep_the_window_on_its_own_monitor() {
        let work = [rect(0, 0, 1920, 1040), rect(1920, 0, 2560, 1400)];
        // 主屏右侧、跨过屏缝：交集在右屏更大 ⇒ 落在右屏。
        assert_eq!(
            clamp_to_work_area(rect(1800, 100, 800, 600), &work),
            rect(1920, 100, 800, 600)
        );
        // 完全在左屏之外、但两屏都无交集（右侧留空）：取距离最近的右屏。
        assert_eq!(
            clamp_to_work_area(rect(1920 + 6000, 100, 800, 600), &work),
            rect(3680, 100, 800, 600)
        );
    }

    /// 双屏上下叠放（负 y 的上屏 + 正 y 的下屏）：按交集选屏，全空时按距离选。
    #[test]
    fn stacked_monitors_with_negative_origin_are_supported() {
        let work = [rect(0, -1080, 1920, 1080), rect(0, 0, 1920, 1040)];
        // 上屏窗口（负 y）不应被搬到下屏。
        assert_eq!(
            clamp_to_work_area(rect(100, -1000, 800, 600), &work),
            rect(100, -1000, 800, 600)
        );
        // 上屏顶部之外 ⇒ 夹回上屏。
        assert_eq!(
            clamp_to_work_area(rect(100, -3000, 800, 600), &work),
            rect(100, -1080, 800, 600)
        );
        // 上屏内部（负坐标）本来就完整可见：不移动。
        assert_eq!(
            clamp_to_work_area(rect(0, -200, 100, 100), &work),
            rect(0, -200, 100, 100)
        );
    }

    /// 两个工作区与目标空隙距离完全相等 ⇒ 取枚举顺序在前者（确定性，不随机）。
    #[test]
    fn equally_near_work_areas_are_tie_broken_by_enumeration_order() {
        let work = [rect(0, 0, 100, 100), rect(300, 0, 100, 100)];
        // 目标在两屏正中间：到两侧空隙各 50。
        assert_eq!(
            clamp_to_work_area(rect(150, 10, 100, 10), &work),
            rect(0, 10, 100, 10)
        );
    }

    /// 无显示器（枚举失败 / 会话 0）⇒ 保守回退：原样返回，不把窗口堆到 (0, 0)。
    #[test]
    fn no_work_areas_is_a_conservative_no_op() {
        let target = rect(5000, -3000, 800, 600);
        assert_eq!(clamp_to_work_area(target, &[]), target);
    }

    /// 距离函数本身：相交为 0，否则是空隙的平方距离（含负坐标）。
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

    /// 最小/最大尺寸解析：缺省最小值是引擎默认 320×200；`min > max` 按 `max` 钳制。
    #[test]
    fn client_limits_resolve_the_public_min_max_contract() {
        let default = ClientLimits::from_options(&WindowOptions::default());
        assert_eq!(default.min, Size::new(320.0, 200.0));
        assert_eq!(default.max, None);

        let options = WindowOptions {
            min_size: Some(Size::new(900.0, 700.0)),
            max_size: Some(Size::new(600.0, 500.0)),
            ..WindowOptions::default()
        };
        let limits = ClientLimits::from_options(&options);
        assert_eq!(
            limits.min,
            Size::new(600.0, 500.0),
            "min > max 时按 max 钳制（公共契约 min = max）"
        );
        assert_eq!(limits.max, Some(Size::new(600.0, 500.0)));
        assert_eq!(
            limits.clamp(Size::new(100.0, 100.0)),
            Size::new(600.0, 500.0),
            "小于 min 的请求被抬到 min"
        );
        assert_eq!(
            limits.clamp(Size::new(2000.0, 2000.0)),
            Size::new(600.0, 500.0),
            "大于 max 的请求被压到 max"
        );

        // 只给 max：默认最小尺寸也要服从 max（不能出现 min > max）。
        let options = WindowOptions {
            max_size: Some(Size::new(200.0, 100.0)),
            ..WindowOptions::default()
        };
        let limits = ClientLimits::from_options(&options);
        assert_eq!(limits.min, Size::new(200.0, 100.0));
    }
}
