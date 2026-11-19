//! macOS 窗口摆放的纯逻辑（B3）：引擎逻辑坐标 ⇄ AppKit 屏幕坐标、工作区 clamp、
//! 内容 min/max 钳制、`position = None` 时的级联默认位置。
//!
//! # 为什么在 `platform/` 根而不是 `platform/macos/` 下
//!
//! `platform::macos` 整个模块只在 macOS 目标编译（objc2 依赖也是 macOS-only），而这里
//! 全是与 AppKit 类型无关的纯数据 / 纯函数，必须能在开发机（Windows）单测 —— 与
//! [`crate::platform::menu`]（macOS 主菜单模型 + test 构建）同一模式，不走 `#[path]`。
//! 真机侧只负责把 `NSScreen` / `NSView` 的矩形喂进来、把返回值写回 `NSWindow`。
//!
//! # 坐标语义（三端统一契约见 `WindowOptions`）
//!
//! - **引擎**（`WindowOptions::position` / `Event::Moved`）：原点 = 虚拟桌面内容区
//!   **左上角**，x 向右、y 向下、逻辑像素。macOS 的 AppKit 全局坐标本身就是 point
//!   （逻辑单位），backingScaleFactor 不参与换算，因此这里没有 DPI 乘法（与 Windows 的
//!   physical / scale 不同）。
//! - **AppKit**：全局屏幕坐标，原点在主屏**左下角**，y 向上；各屏幕 `frame` 可以带负
//!   坐标（主屏左侧/下方的屏幕）。虚拟桌面 = 全部屏幕 `frame` 的并集包围盒。
//!
//! 换算因此只有「平移 + y 翻转」：
//! `appkit = (desktop.min_x + engine.x, desktop.max_y - engine.y)`。
//!
//! # clamp 契约
//!
//! 初始位置与级联默认位置都 clamp 进**目标屏幕的 `visibleFrame`**（已扣掉菜单栏 /
//! Dock / 刘海的可用工作区）：放得下时整窗落在工作区内；放不下时左上角对齐工作区左上角
//! （标题栏那条边始终可见，满足 `WindowOptions::position` 的「至少保留标题栏/一条边」
//! 契约）。`Event::Moved` 报告的是窗口**真实**位置，不再 clamp。

use crate::math::{Point, Size};

/// AppKit 全局坐标下的矩形（**point**；左下原点、y 向上）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Area {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Area {
    pub(crate) const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 左边界（AppKit 的 x）。真机侧（`platform::macos::window`）读，测试构建下的纯逻辑
    /// 单测不消费它，因此按 macOS 门控避免 Windows 测试构建出现死代码。
    #[cfg(target_os = "macos")]
    pub(crate) const fn x(self) -> f64 {
        self.x
    }

    /// 右边界 / 上边界（AppKit 是 y 向上，所以 `max_y` 是屏幕顶部）。
    pub(crate) const fn max_x(self) -> f64 {
        self.x + self.width
    }

    pub(crate) const fn max_y(self) -> f64 {
        self.y + self.height
    }

    /// 两块矩形相交面积（无交集为 0）。
    fn intersection_area(self, other: Self) -> f64 {
        let width = (self.max_x().min(other.max_x()) - self.x.max(other.x)).max(0.0);
        let height = (self.max_y().min(other.max_y()) - self.y.max(other.y)).max(0.0);
        width * height
    }

    /// 两矩形之间的最短距离平方（无交集时用于挑最近的屏幕）。
    fn distance_sq_to(self, other: Self) -> f64 {
        let dx = (other.x - self.max_x())
            .max(self.x - other.max_x())
            .max(0.0);
        let dy = (other.y - self.max_y())
            .max(self.y - other.max_y())
            .max(0.0);
        dx * dx + dy * dy
    }
}

/// 一块屏幕：`frame` 决定虚拟桌面包围盒，`visible`（`NSScreen.visibleFrame`，已扣掉
/// 菜单栏 / Dock / 刘海）决定窗口 clamp 的工作区。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenArea {
    frame: Area,
    visible: Area,
}

impl ScreenArea {
    /// 真机侧构造（`platform::macos::window::screen_areas`）。测试构建的纯逻辑单测与被测
    /// 函数同模块，直接写字面量即可，因此这里按 macOS 门控避免 Windows 测试构建死代码。
    #[cfg(target_os = "macos")]
    pub(crate) const fn new(frame: Area, visible: Area) -> Self {
        Self { frame, visible }
    }

    /// 工作区矩形（`visibleFrame`，已扣掉菜单栏 / Dock / 刘海）。
    #[cfg(target_os = "macos")]
    pub(crate) const fn visible(self) -> Area {
        self.visible
    }
}

/// 引擎逻辑坐标 ⇄ AppKit 全局坐标的平移（虚拟桌面**内容区左上角**在 AppKit 空间中的位置）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Desktop {
    /// 最左屏幕的左边界（多屏时可为负）。
    min_x: f64,
    /// 最高屏幕的上边界。
    max_y: f64,
}

impl Desktop {
    /// 全部屏幕 `frame` 的并集包围盒；`None` = 没有屏幕（无显示器 / headless）。
    pub(crate) fn from_screens(screens: &[ScreenArea]) -> Option<Self> {
        let first = screens.first()?;
        Some(Self {
            min_x: screens
                .iter()
                .fold(first.frame.x, |min, screen| min.min(screen.frame.x)),
            max_y: screens.iter().fold(first.frame.max_y(), |max, screen| {
                max.max(screen.frame.max_y())
            }),
        })
    }

    fn to_appkit(self, point: Point) -> (f64, f64) {
        (
            self.min_x + f64::from(point.x),
            self.max_y - f64::from(point.y),
        )
    }

    fn to_engine(self, x: f64, top: f64) -> Point {
        Point::new((x - self.min_x) as f32, (self.max_y - top) as f32)
    }
}

/// 引擎位置（内容区左上角，逻辑点）→ AppKit **内容区原点**（左下角，point），并 clamp 进
/// 最近屏幕的工作区。返回值直接喂 `NSWindow::initWithContentRect:styleMask:backing:defer:`。
///
/// 没有屏幕时（headless）只做 y 翻转、不 clamp：窗口自由建在给定位置。
pub(crate) fn content_origin_for_position(
    position: Point,
    size: Size,
    screens: &[ScreenArea],
) -> (f64, f64) {
    let width = f64::from(size.width.max(1.0));
    let height = f64::from(size.height.max(1.0));
    let Some(desktop) = Desktop::from_screens(screens) else {
        return (f64::from(position.x), -f64::from(position.y) - height);
    };
    let (left, top) = desktop.to_appkit(position);
    clamp_origin(Area::new(left, top - height, width, height), screens)
}

/// AppKit 内容区矩形 → 引擎位置（内容区左上角，逻辑点）。**不** clamp：`Event::Moved`
/// 报告窗口的真实位置（允许部分离屏，例如用户把窗口拖出屏幕边缘）。
pub(crate) fn position_from_content_rect(content: Area, screens: &[ScreenArea]) -> Point {
    match Desktop::from_screens(screens) {
        Some(desktop) => desktop.to_engine(content.x, content.max_y()),
        // 与 [`content_origin_for_position`] 的无屏分支互逆。
        None => Point::new(content.x as f32, (-content.max_y()) as f32),
    }
}

/// `position = None` 时平台决定的初始位置（引擎逻辑坐标）。
///
/// AppKit 没有「自动摆放」的默认行为（`initWithContentRect:` 严格按给定矩形创建），因此
/// 平台层自己在主屏工作区左上角按 26 point 阶梯级联（与 `NSWindow.cascadeTopLeftFromPoint:`
/// 的步长一致；Qt / winit 的默认摆放同款），8 个窗口后回卷 —— 多窗口不再完全重叠，
/// 兑现 `WindowOptions::position = None` 文档里的「macOS 由平台级联摆放」。
pub(crate) fn cascade_position(desktop: Desktop, main_visible: Area, index: usize) -> Point {
    let step = CASCADE_STEP * (index % CASCADE_WRAP) as f64;
    let x = main_visible.x - desktop.min_x + CASCADE_MARGIN + step;
    let y = desktop.max_y - main_visible.max_y() + CASCADE_MARGIN + step;
    Point::new(x as f32, y as f32)
}

/// 级联步长（point，AppKit 系统摆放的默认阶梯）。
const CASCADE_STEP: f64 = 26.0;
/// 级联回卷窗口数（避免第 20 个窗口被推到工作区右下角之外）。
const CASCADE_WRAP: usize = 8;
/// 级联起点距主屏工作区左上角的内缩（point）。
const CASCADE_MARGIN: f64 = 80.0;

/// 内容区 min/max 尺寸（B3）。
///
/// - `min = None` 用平台默认最小尺寸（macOS 与 Win32 / Wayland 同源的 320×200 逻辑点）；
/// - `max = None` 不设上限（调用方不调用 `setContentMaxSize:`）；
/// - `min > max` 时按轴钳成 `min = max`（与 `WindowOptions` 文档一致，等价 Qt 的
///   `setMinimumSize` / `setMaximumSize` 互相钳制）；
/// - 两个结果都不小于 1 point：AppKit 的 min/max 尺寸不接受 0 / 负值。
pub(crate) fn content_size_bounds(
    min: Option<Size>,
    max: Option<Size>,
    platform_min: Size,
) -> (Size, Option<Size>) {
    let max = max.map(|size| Size::new(size.width.max(1.0), size.height.max(1.0)));
    let min = min.map_or(platform_min, |size| {
        Size::new(size.width.max(1.0), size.height.max(1.0))
    });
    let min = max.map_or(min, |max| {
        Size::new(min.width.min(max.width), min.height.min(max.height))
    });
    (min, max)
}

/// 把内容区矩形 clamp 进工作区，返回 AppKit 原点（左下角）。
fn clamp_origin(rect: Area, screens: &[ScreenArea]) -> (f64, f64) {
    let Some(visible) = target_work_area(rect, screens) else {
        return (rect.x, rect.y);
    };
    // 放得下：完整 clamp 进工作区（整窗可见）；放不下：左上角对齐工作区左上角
    // （标题栏那条边始终在工作区内）。
    let x = if rect.width >= visible.width {
        visible.x
    } else {
        rect.x.clamp(visible.x, visible.max_x() - rect.width)
    };
    let y = if rect.height >= visible.height {
        visible.max_y() - rect.height
    } else {
        rect.y.clamp(visible.y, visible.max_y() - rect.height)
    };
    (x, y)
}

/// 目标工作区：与矩形相交面积最大的屏幕；全都不相交时取距离最近的屏幕。
fn target_work_area(rect: Area, screens: &[ScreenArea]) -> Option<Area> {
    let mut best: Option<(f64, Area)> = None;
    for screen in screens {
        let area = rect.intersection_area(screen.visible);
        if area > 0.0 && best.is_none_or(|(best_area, _)| area > best_area) {
            best = Some((area, screen.visible));
        }
    }
    if let Some((_, visible)) = best {
        return Some(visible);
    }
    // 与所有工作区都不相交（显示器拔掉 / 位置来自另一套屏幕布局的恢复路径）：
    // 取距离最近的工作区，保证窗口至少落在某块屏幕附近而不是被丢到屏幕外。
    screens
        .iter()
        .map(|screen| screen.visible)
        .min_by(|a, b| rect.distance_sq_to(*a).total_cmp(&rect.distance_sq_to(*b)))
}

#[cfg(test)]
mod tests {
    use super::{
        Area, Desktop, ScreenArea, cascade_position, content_origin_for_position,
        content_size_bounds, position_from_content_rect,
    };
    use crate::math::{Point, Size};

    /// 主屏：1920×1080，菜单栏占顶部 25 point，Dock 占底部 60 point。
    fn main_screen() -> ScreenArea {
        ScreenArea {
            frame: Area::new(0.0, 0.0, 1920.0, 1080.0),
            visible: Area::new(0.0, 60.0, 1920.0, 995.0),
        }
    }

    /// 主屏左侧的副屏：1280×1024（AppKit 全局坐标 x 为负）。
    fn left_screen() -> ScreenArea {
        ScreenArea {
            frame: Area::new(-1280.0, 0.0, 1280.0, 1024.0),
            visible: Area::new(-1280.0, 0.0, 1280.0, 1024.0),
        }
    }

    /// 主屏上方的副屏（更高，决定虚拟桌面 `max_y`）。
    fn upper_screen() -> ScreenArea {
        ScreenArea {
            frame: Area::new(0.0, 1080.0, 1920.0, 1200.0),
            visible: Area::new(0.0, 1080.0, 1920.0, 1200.0),
        }
    }

    fn desktop_of(screens: &[ScreenArea]) -> Desktop {
        Desktop::from_screens(screens).expect("至少一块屏幕")
    }

    #[test]
    fn engine_position_is_flipped_into_appkit_content_origin() {
        // 引擎 (100, 50) = 内容区左上角；高 600 ⇒ AppKit 左下角 y = (1080-50)-600 = 430。
        let screens = [main_screen()];
        assert_eq!(
            content_origin_for_position(Point::new(100.0, 50.0), Size::new(800.0, 600.0), &screens),
            (100.0, 430.0)
        );
        // 原点 (0,0) = 主屏内容区顶部；但 AppKit 顶部（1080）高于 visibleFrame 顶部
        // （菜单栏下沿 1055）⇒ 被 clamp 到 1055，左下角 = 1055-600 = 455。
        assert_eq!(
            content_origin_for_position(Point::new(0.0, 0.0), Size::new(800.0, 600.0), &screens),
            (0.0, 455.0)
        );
    }

    #[test]
    fn roundtrip_between_engine_position_and_appkit_content_rect() {
        // 位置与读取必须互逆（对不需要 clamp 的位置）：否则应用持久化的位置与
        // `Event::Moved` 会互相打架。
        let screens = [main_screen()];
        for (x, y, w, h) in [
            (100.0_f32, 50.0_f32, 800.0_f32, 600.0_f32),
            (200.0, 200.0, 320.0, 200.0),
            (1500.0, 300.0, 400.0, 640.0),
        ] {
            let (left, bottom) =
                content_origin_for_position(Point::new(x, y), Size::new(w, h), &screens);
            let content = Area::new(left, bottom, f64::from(w), f64::from(h));
            assert_eq!(
                position_from_content_rect(content, &screens),
                Point::new(x, y)
            );
        }
    }

    #[test]
    fn positions_are_clamped_into_the_visible_frame() {
        let screens = [main_screen()];
        // 右侧越界：x 被 clamp 到 visible.max_x - width = 1920 - 800 = 1120。
        assert_eq!(
            content_origin_for_position(
                Point::new(5000.0, 50.0),
                Size::new(800.0, 600.0),
                &screens
            ),
            (1120.0, 430.0)
        );
        // 左侧/上方负坐标：clamp 到 visible 的左边与上边（菜单栏下沿 max_y = 1055）。
        assert_eq!(
            content_origin_for_position(
                Point::new(-50.0, -20.0),
                Size::new(800.0, 600.0),
                &screens
            ),
            (0.0, 455.0)
        );
        // 下方越界：AppKit 左下角被 clamp 到 Dock 上沿 visible.y = 60。
        assert_eq!(
            content_origin_for_position(
                Point::new(100.0, 900.0),
                Size::new(800.0, 600.0),
                &screens
            ),
            (100.0, 60.0)
        );
    }

    #[test]
    fn a_window_larger_than_the_work_area_keeps_its_top_left_visible() {
        // 2500×1500 的窗口放不进 1920×995：左上角对齐工作区左上角
        // （x = visible.x；AppKit 顶部 = visible.max_y，因此左下角 y = 1055 - 1500）。
        let screens = [main_screen()];
        assert_eq!(
            content_origin_for_position(
                Point::new(100.0, 100.0),
                Size::new(2500.0, 1500.0),
                &screens
            ),
            (0.0, -445.0)
        );
    }

    #[test]
    fn multi_screen_negative_origin_and_upper_screen_flip() {
        // 左屏在负 x：引擎 (0,100) 落在最左屏的内容区（y=100 在左屏 1024 高度内）。
        let screens = [main_screen(), left_screen()];
        let desktop = desktop_of(&screens);
        assert_eq!(desktop.min_x, -1280.0);
        assert_eq!(desktop.max_y, 1080.0);
        assert_eq!(
            content_origin_for_position(Point::new(0.0, 100.0), Size::new(800.0, 600.0), &screens),
            (-1280.0, 380.0)
        );

        // 上方副屏决定 max_y = 2280：引擎 y = 50 落在上方屏内容区。
        let screens = [main_screen(), upper_screen()];
        let desktop = desktop_of(&screens);
        assert_eq!(desktop.max_y, 2280.0);
        assert_eq!(
            content_origin_for_position(Point::new(100.0, 50.0), Size::new(800.0, 600.0), &screens),
            (100.0, 1630.0)
        );
    }

    #[test]
    fn clamp_picks_the_screen_with_the_largest_intersection() {
        // 窗口横跨两屏（左屏 600 point、主屏 200 point）：clamp 基准必须是相交更多的左屏。
        let screens = [main_screen(), left_screen()];
        let (left, bottom) = content_origin_for_position(
            Point::new(680.0, 100.0),
            Size::new(800.0, 600.0),
            &screens,
        );
        // 引擎 x=680 ⇒ AppKit 左边界 = -600，横跨 [-600, 200]；左屏更宽 ⇒ 目标 = 左屏，
        // x 被 clamp 进 [-1280, -800] ⇒ -800；y=380 在左屏可见范围内不变。
        assert_eq!((left, bottom), (-800.0, 380.0));
    }

    #[test]
    fn far_off_screen_position_snaps_to_the_nearest_screen() {
        // 屏幕布局变化后的恢复：位置远超所有屏幕 ⇒ 落到最近工作区（左屏）里。
        let screens = [main_screen(), left_screen()];
        let (left, bottom) = content_origin_for_position(
            Point::new(-9000.0, 300.0),
            Size::new(800.0, 600.0),
            &screens,
        );
        assert_eq!((left, bottom), (-1280.0, 180.0));
    }

    #[test]
    fn menu_bar_and_dock_shrink_the_work_area() {
        // visibleFrame 已扣掉菜单栏（顶部 25）与 Dock（底部 60）：整窗必须落在
        // [visible.min_y, visible.max_y] 内，不能覆盖菜单栏 / Dock。
        let screens = [main_screen()];
        let visible = screens[0].visible;
        for engine_y in [0.0_f32, 500.0, 1000.0] {
            let (_, bottom) = content_origin_for_position(
                Point::new(0.0, engine_y),
                Size::new(400.0, 300.0),
                &screens,
            );
            assert!(
                bottom >= visible.y - f64::EPSILON
                    && bottom + 300.0 <= visible.max_y() + f64::EPSILON,
                "engine_y={engine_y} 的窗口必须完整落在 visibleFrame 内：bottom={bottom}"
            );
        }
    }

    #[test]
    fn cascade_defaults_step_down_right_and_wrap() {
        let screens = [main_screen()];
        let desktop = desktop_of(&screens);
        let visible = screens[0].visible;
        let first = cascade_position(desktop, visible, 0);
        assert_eq!(first, Point::new(80.0, 105.0));
        let second = cascade_position(desktop, visible, 1);
        assert_eq!(second, Point::new(106.0, 131.0));
        // 8 个窗口后回卷：第 9 个与第 1 个重合，不会把窗口永远推向工作区外。
        assert_eq!(cascade_position(desktop, visible, 8), first);
    }

    #[test]
    fn size_bounds_use_platform_minimum_and_clamp_min_into_max() {
        let platform_min = Size::new(320.0, 200.0);
        // 缺省：平台最小尺寸 + 无上限。
        assert_eq!(
            content_size_bounds(None, None, platform_min),
            (platform_min, None)
        );
        // 只给 max：min 被钳到 max（min = max），不会出现 min > max 的平台状态。
        assert_eq!(
            content_size_bounds(None, Some(Size::new(200.0, 100.0)), platform_min),
            (Size::new(200.0, 100.0), Some(Size::new(200.0, 100.0)))
        );
        // min > max：按轴钳成 min = max。
        assert_eq!(
            content_size_bounds(
                Some(Size::new(900.0, 700.0)),
                Some(Size::new(800.0, 600.0)),
                platform_min
            ),
            (Size::new(800.0, 600.0), Some(Size::new(800.0, 600.0)))
        );
        // 只给 min（大于平台默认）：应用自己接管最小尺寸。
        assert_eq!(
            content_size_bounds(Some(Size::new(640.0, 480.0)), None, platform_min),
            (Size::new(640.0, 480.0), None)
        );
    }

    #[test]
    fn without_screens_the_mapping_only_flips_the_y_axis() {
        // headless（无显示器）：不 clamp、不做虚拟桌面平移。
        assert_eq!(
            content_origin_for_position(Point::new(10.0, 20.0), Size::new(100.0, 50.0), &[]),
            (10.0, -70.0)
        );
        assert_eq!(
            position_from_content_rect(Area::new(10.0, -70.0, 100.0, 50.0), &[]),
            Point::new(10.0, 20.0)
        );
    }
}
