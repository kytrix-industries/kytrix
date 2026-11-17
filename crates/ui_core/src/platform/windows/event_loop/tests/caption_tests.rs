//! 自绘标题栏拖动 / 边框缩放的纯函数单测。

use std::time::Duration;

use windows::Win32::{
    Foundation::{POINT, RECT},
    UI::WindowsAndMessaging::{HTCAPTION, HTSYSMENU},
};

use super::super::caption::{
    caption_position, is_caption_hit, is_self_timed_double_click, min_visible_edge,
    restored_drag_origin,
};
use crate::math::Scale;

fn scale_one() -> Scale {
    Scale::ONE
}

fn scale(factor: f32) -> Scale {
    Scale::new(factor).expect("scale")
}

fn work() -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    }
}

fn window() -> RECT {
    RECT {
        left: 100,
        top: 100,
        right: 500,
        bottom: 400,
    }
}

#[test]
fn caption_areas_are_the_title_bar_and_the_menu_icon() {
    assert!(is_caption_hit(HTCAPTION));
    assert!(is_caption_hit(HTSYSMENU));
    // `HTNOWHERE` 以及最小化/最大化/关闭按钮、边框都不归拖动。
    assert!(!is_caption_hit(0));
}

// ── MIN_VISIBLE_EDGE：按 DPI 缩放（win-2 P3） ──────────────────────────────

#[test]
fn min_visible_edge_scales_with_the_dpi() {
    assert_eq!(min_visible_edge(scale_one()), 64);
    assert_eq!(
        min_visible_edge(scale(1.25)),
        80,
        "125% DPI 下 64 逻辑像素 = 80 物理像素"
    );
    assert_eq!(min_visible_edge(scale(1.5)), 96);
    assert_eq!(min_visible_edge(scale(2.0)), 128);
    // 极端小比例也要至少留 1 物理像素（不能钳成 0，更不能为负）。
    assert_eq!(min_visible_edge(scale(0.001)), 1);
}

#[test]
fn caption_drag_follows_the_cursor_delta() {
    let (left, top) = caption_position(window(), (50, 30), work(), scale_one());

    assert_eq!((left, top), (150, 130));
}

#[test]
fn caption_drag_keeps_the_title_bar_below_the_work_area_top() {
    // 往上拖：系统原生 move 同样不允许把标题栏拖到工作区顶边之上。
    let (_, top) = caption_position(window(), (0, -500), work(), scale_one());

    assert_eq!(top, 0);
}

#[test]
fn caption_drag_leaves_a_visible_sliver_at_the_bottom() {
    let (_, top) = caption_position(window(), (0, 2000), work(), scale_one());

    assert_eq!(top, work().bottom - min_visible_edge(scale_one()));
}

#[test]
fn caption_drag_keeps_more_physical_pixels_visible_at_high_dpi() {
    // 同一逻辑目标（标题栏贴底）在 200% DPI 下必须保留 128 物理像素：固定 64 物理像素
    // 在高 DPI 屏幕上只剩 32 逻辑像素，窗口会被拖得几乎不可见（win-2 P3）。
    let (_, at_100) = caption_position(window(), (0, 2000), work(), scale_one());
    let (_, at_200) = caption_position(window(), (0, 2000), work(), scale(2.0));

    assert_eq!(work().bottom - at_100, 64);
    assert_eq!(work().bottom - at_200, 128);
}

#[test]
fn caption_drag_clamps_horizontally_to_the_work_area() {
    let (far_left, _) = caption_position(window(), (-2000, 0), work(), scale_one());
    let (far_right, _) = caption_position(window(), (3000, 0), work(), scale_one());

    // 至少留 min_visible_edge 宽在屏幕里（窗口宽 400）。
    let edge = min_visible_edge(scale_one());
    assert_eq!(far_left, work().left - 400 + edge);
    assert_eq!(far_right, work().right - edge);
}

#[test]
fn caption_drag_uses_the_cursor_monitor_work_area() {
    // 主屏右侧的第二台显示器：虚拟坐标从 1920 起，工作区含任务栏（bottom 小 40）。
    let secondary = RECT {
        left: 1920,
        top: 0,
        right: 3840,
        bottom: 1000,
    };
    let moved = RECT {
        left: 1900,
        top: 100,
        right: 2300,
        bottom: 400,
    };

    let (left, top) = caption_position(moved, (100, -200), secondary, scale_one());

    assert_eq!(left, 2000, "跨屏拖动按光标所在显示器平移");
    assert_eq!(top, 0, "垂直仍钳制到第二台显示器的工作区顶边");
}

#[test]
fn caption_drag_does_not_panic_on_a_tiny_work_area() {
    // 退化工作区（多显示器配置异常 / 远程桌面极小分辨率）：钳制区间必须自洽，不能
    // 触发 `clamp(min > max)` panic。工作区比 min_visible_edge 还窄时退化为「靠右边界
    // 减去可见边」，窗口 400 宽 ⇒ left = 10 - 64 = -54。
    let tiny = RECT {
        left: 0,
        top: 0,
        right: 10,
        bottom: 10,
    };

    let (left, top) = caption_position(window(), (1, 1), tiny, scale_one());

    assert_eq!((left, top), (10 - min_visible_edge(scale_one()), 0));
}

// ── 最大化还原摆位：水平比例 + 竖直偏移（win-2 P3） ──────────────────────

#[test]
fn restored_drag_origin_keeps_the_cursor_ratio_on_the_title_bar() {
    // 最大化窗口 1920 宽，还原尺寸 800×600：光标在最大化标题栏 50% 处 ⇒ 还原窗口
    // 标题栏的 50% 处也要落在光标下（left = 960 - 800/2），顶边与最大化顶边一致。
    let maximized = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    let restored = RECT {
        left: 200,
        top: 100,
        right: 1000,
        bottom: 700,
    };
    let press = POINT { x: 960, y: 30 };

    let (left, top) = restored_drag_origin(maximized, restored, press, press, work(), scale_one());

    assert_eq!((left, top), (560, 0));
}

#[test]
fn restored_drag_origin_preserves_the_vertical_caption_offset() {
    // 按下后光标往下甩了 40 px（超过系统拖拽阈值）⇒ 还原窗口顶边也跟着下移 40 px，
    // 光标相对标题栏的竖直偏移保持不变。旧实现固定 `top = maximized.top`，光标会掉到
    // 标题栏下方的客户区里（win-2 P3）。
    let maximized = RECT {
        left: 0,
        top: -8,
        right: 1920,
        bottom: 1040,
    };
    let restored = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let press = POINT { x: 500, y: 12 };
    let cursor = POINT { x: 520, y: 52 };

    let (left, top) = restored_drag_origin(maximized, restored, press, cursor, work(), scale_one());

    // top = cursor.y - (press.y - maximized.top) = 52 - (12 + 8) = 32。
    assert_eq!(top, 32, "竖直方向必须保留按下时在标题栏内的偏移");
    // 水平：光标在最大化标题栏 520/1920 比例处 ⇒ 还原标题栏同一比例处。
    assert_eq!(left, 520 - ((520.0 / 1920.0) * 800.0) as i32);
}

#[test]
fn restored_drag_origin_uses_the_cursor_monitor() {
    // 第二台显示器（1920..3840）上的最大化窗口：光标在它左侧 1/4 处 ⇒ 还原窗口顶边
    // 留在该显示器工作区内，水平按比例摆位。
    let maximized = RECT {
        left: 1920,
        top: 0,
        right: 3840,
        bottom: 1000,
    };
    let restored = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let secondary = RECT {
        left: 1920,
        top: 0,
        right: 3840,
        bottom: 1000,
    };
    let press = POINT { x: 2400, y: 20 };

    let (left, top) =
        restored_drag_origin(maximized, restored, press, press, secondary, scale_one());

    assert_eq!((left, top), (2200, 0));
}

#[test]
fn restored_drag_origin_clamps_to_a_negative_coordinate_monitor() {
    // 主显示器上方的第二台显示器：虚拟坐标 top 为负。还原摆位仍要钳在该显示器工作区内，
    // 负坐标不能触发任何越界/panic。
    let above = RECT {
        left: 0,
        top: -1080,
        right: 1920,
        bottom: 0,
    };
    let restored = RECT {
        left: 0,
        top: -500,
        right: 800,
        bottom: 100,
    };
    let press = POINT { x: 300, y: -1050 };
    let cursor = POINT { x: 320, y: -1020 };

    let (left, top) = restored_drag_origin(above, restored, press, cursor, above, scale_one());

    assert_eq!(top, -1050, "还原顶边 = 最大化顶边 + 光标竖直位移");
    assert_eq!(left, 320 - 133, "负坐标工作区内的水平比例摆位");
}

#[test]
fn restored_drag_origin_handles_extreme_drag_deltas() {
    // 还原触发前光标已经被甩出工作区（极端拖拽方向 / 卡顿后的积压移动）：竖直方向必须
    // 钳在 [工作区顶边, 工作区底边 - 可见边]，不能 panic，也不能让标题栏出屏。
    let maximized = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    let restored = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let press = POINT { x: 960, y: 5 };

    let (left, top) = restored_drag_origin(
        maximized,
        restored,
        press,
        POINT { x: 960, y: 50_000 },
        work(),
        scale_one(),
    );
    assert_eq!(top, work().bottom - 64);
    assert_eq!(left, 960 - 400);

    let (_, top) = restored_drag_origin(
        maximized,
        restored,
        press,
        POINT { x: 960, y: -50_000 },
        work(),
        scale_one(),
    );
    assert_eq!(top, 0);

    // 200% DPI 下底部可见边按物理像素放大（128 px）。
    let (_, top_high_dpi) = restored_drag_origin(
        maximized,
        restored,
        press,
        POINT { x: 960, y: 50_000 },
        work(),
        scale(2.0),
    );
    assert_eq!(top_high_dpi, work().bottom - 128);
}

// ── 自计时双击兜底（win-2 P3） ──────────────────────────────────────────

#[test]
fn self_timed_double_click_requires_time_and_distance() {
    let time = Duration::from_millis(500);
    let tolerance = (2, 2);

    assert!(is_self_timed_double_click(
        Duration::from_millis(499),
        (1, -2),
        time,
        tolerance
    ));
    assert!(
        !is_self_timed_double_click(Duration::from_millis(500), (0, 0), time, tolerance),
        "时间必须严格小于双击时间（与系统「在指定时间内」一致）"
    );
    assert!(
        !is_self_timed_double_click(Duration::from_millis(100), (3, 0), time, tolerance),
        "水平超出双击矩形（半宽 2）"
    );
    assert!(
        !is_self_timed_double_click(Duration::from_millis(100), (0, -3), time, tolerance),
        "竖直超出双击矩形（半高 2）"
    );
    // 极端值：`i32::MIN` 距离不能触发 `abs` 溢出（debug 下会 panic）。
    assert!(!is_self_timed_double_click(
        Duration::from_millis(1),
        (i32::MIN, i32::MAX),
        time,
        tolerance
    ));
}
