//! 自绘标题栏**拖动区**（X2）的命中判定：Linux/Wayland 用 `xdg_toplevel.move` 把拖动交给
//! compositor。
//!
//! # 为什么放在这一层
//!
//! 拖动的判定只有两个纯问题：「区域表 + 逻辑坐标点 → 是否命中」与「哪个输入事件算拖动开始」。
//! 真正的协议调用只有一句 `xdg_toplevel._move(seat, serial)`，留在
//! `platform/linux/event_loop.rs` 的泵里；判定放在那里的话，开发机（Windows）的 `cargo test`
//! 一次也覆盖不到（与 B7 的按钮码、B8 的滚轮累积同一教训，见同目录的 `input.rs` / `scroll.rs`）。
//!
//! # 语义（三端统一；Chrome / Electron 的 `-webkit-app-region: drag` 模型）
//!
//! - 区域是**逻辑窗口坐标**（客户区左上角为原点，x 向右、y 向下），可以有多个，取**并集**；
//!   空列表 = 没有拖动区（默认）。
//! - 边界是**半开区间** `[min, max)`（[`Rect::contains`]；与 Chrome `gfx::Rect::Contains`、
//!   Windows 平台层对拖动区的 `WM_NCHITTEST` 判定同款）：相邻区域不重复命中，右下边界外的
//!   像素属于内容。
//! - 只有**主键按下**（[`PointerButton::Primary`]）触发：拖动区是窗口 chrome，命中后该次按下
//!   **不投递**给应用（不产生 click / drag 会话），后续 motion / up 由 compositor 的隐式抓取
//!   接管；区域之外行为完全不变，`drag_regions` 为空时与没有本功能完全一致。
//!
//! # 不提供的手势（与 Windows 的差异，明确口径）
//!
//! 双击最大化/还原、Aero Snap 之类的系统手势是 **compositor / 系统**语义：`xdg_toplevel.move`
//! 只请求「交互移动」，Wayland 没有把「哪块是我的拖动区」告诉 compositor 的协议。因此拖动区内
//! 的双击**不会**最大化（Windows 的 `HTCAPTION` 会）：命中区域后应用连这次按下都收不到。应用
//! 若需要双击最大化，应在**非拖动区**的窗口 chrome 上自行处理并调用
//! `WindowControl::ToggleMaximize`（Linux 构建专有 API）。
//!
//! # 坐标
//!
//! `wl_pointer.enter/motion/button` 给的是 **surface-local** 坐标，而拖动区是客户区坐标。
//! 当前二者重合：`SHADOW_MARGIN == 0`（`platform/linux/window.rs` 有编译期断言禁止直接改大）
//! ⇒ window geometry 就是整个 surface，客户区原点 = surface 原点。将来引入 CSD 阴影边距时那条
//! 断言会构建失败，强制实现者同时处理输入区与本处的坐标平移（surface 坐标减去边距）。
//!
//! # 降级（用户裁决 X2：不得静默忽略）
//!
//! 没有 `xdg_toplevel` / compositor 不实现交互移动时，`xdg_toplevel.move` 请求是无副作用的
//! no-op（协议没有「不支持」错误，compositor 可以忽略）：窗口不动，应用仍可用
//! `WindowControl::BeginMove` 的显式路径。本模块只做判定、不接触协议，降级语义由调用方注释写清。

use crate::{
    event::{Event, InputEvent},
    math::{Point, Rect},
};

/// 这次平台事件是否是「拖动区的主键按下」；是 ⇒ 返回按下点（逻辑客户区坐标）。
///
/// 只认 **`PointerDown` + 主键**：
///
/// - 次键 / 中键 / 侧键在拖动区上仍按普通内容处理（右键菜单、中键粘贴等不得被 chrome 吞掉）；
/// - 抬起 / 移动 / 进入不触发 —— 拖动只能由一次真实的按下开始（`xdg_toplevel.move` 的 serial
///   也必须来自这次按下）。
#[must_use]
pub(crate) fn drag_press_position(event: &Event) -> Option<Point> {
    match event {
        Event::Input(InputEvent::PointerDown {
            position, button, ..
        }) if button.is_primary() => Some(*position),
        _ => None,
    }
}

/// 逻辑坐标 `point` 是否落在任一拖动区内（并集，半开区间）。
///
/// `false` = 没有拖动区 / 点不在任何区域内 / 点非有限（NaN / ±∞ 的防御入口：
/// [`Rect::contains`] 对 NaN 恒 `false`，这里把意图写明）。
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

    /// 一条自绘标题栏条带：客户区顶部 40 逻辑像素、横向 800。
    fn titlebar() -> Rect {
        Rect::new(0.0, 0.0, 800.0, 40.0)
    }

    /// X2 基本命中：条带内（含左上角）命中；条带外的上下左右都不命中。
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

    /// 边界语义钉死为**半开区间** `[min, max)`（与 `Rect::contains` / Chrome 一致）：
    /// 左上边界算命中，右下边界算区域外 —— 相邻区域不重复命中、边界外像素属于内容。
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

    /// 多个区域取**并集**：条带 + 侧栏把手；两个区域之间的空隙不命中，任一区域内命中。
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

    /// 空区域列表 = 没有拖动区：任何点都不命中（默认行为，等价于未声明 X2）。
    #[test]
    fn no_regions_never_hit() {
        assert!(!hit(&[], Point::new(0.0, 0.0)));
        assert!(!hit(&[], Point::new(400.0, 20.0)));
    }

    /// 负坐标区域（应用把把手放在客户区左/上边界之外，或窗口内容上移）：命中判定不得因负坐标
    /// 失效或 panic。
    #[test]
    fn negative_origin_regions_work() {
        let region = Rect::new(-20.0, -10.0, 40.0, 30.0);
        assert!(hit(&[region], Point::new(-20.0, -10.0)), "负原点本身命中");
        assert!(hit(&[region], Point::new(0.0, 0.0)));
        assert!(!hit(&[region], Point::new(20.0, -10.0)), "右边界外");
        assert!(!hit(&[region], Point::new(-20.5, 0.0)), "左边界外");
    }

    /// 退化区域（宽或高为 0 / 负数）一律不命中：应用传了坏矩形时不制造一条 1 像素的假拖动带。
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

    /// 超大区域与退化点：巨大矩形覆盖任意有限客户区坐标；NaN / ±∞ 点恒不命中（防御，
    /// 不 panic、也不把非有限坐标当命中）。
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

    /// 只有**主键**按下算拖动开始；其它键与其它指针事件都不触发（抬起/移动不能自己开始拖动，
    /// 次键/中键在拖动区上仍是普通内容）。
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
