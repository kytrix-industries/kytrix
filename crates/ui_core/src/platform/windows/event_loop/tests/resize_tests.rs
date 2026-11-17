//! 自绘边框缩放的纯函数单测。

use windows::Win32::{
    Foundation::{POINT, RECT},
    UI::WindowsAndMessaging::{HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT, HTSYSMENU, HTTOPLEFT},
};

use super::super::{
    ResizeSession,
    caption::{is_resize_hit, resized_rect},
};

/// 缩放起点与抓取点必须一致：`HTLEFT`/`HTTOPLEFT`/`HTBOTTOMLEFT` 的按下点在**左边缘**，
/// 右键/上边/下边同理。抓取点由调用方显式给出（旧的固定 `(500, 300)` 让左边缘的 `dx`
/// 从窗口右边缘起算，算术上不可能触发最小宽度钳制）。
fn session(hit: u32, cursor: POINT) -> ResizeSession {
    ResizeSession {
        hit,
        cursor,
        window: RECT {
            left: 100,
            top: 100,
            right: 500,
            bottom: 400,
        },
        latest: cursor,
    }
}

#[test]
fn border_hits_are_the_eight_resize_edges() {
    assert!(is_resize_hit(HTLEFT));
    assert!(is_resize_hit(HTBOTTOMRIGHT));
    assert!(
        !is_resize_hit(HTCAPTION),
        "标题栏由自绘拖动分支接管，不是缩放"
    );
    assert!(
        !is_resize_hit(HTSYSMENU),
        "标题栏左上角图标区也归自绘拖动（避免系统菜单模态循环）"
    );
}

#[test]
fn dragging_the_right_edge_changes_only_the_width() {
    let rect = resized_rect(
        session(HTRIGHT, POINT { x: 500, y: 300 }),
        POINT { x: 540, y: 280 },
        (320, 200),
    );

    assert_eq!(rect.left, 100);
    assert_eq!(rect.right, 540);
    assert_eq!(rect.top, 100);
    assert_eq!(rect.bottom, 400);
}

#[test]
fn dragging_the_top_left_corner_moves_the_origin() {
    let rect = resized_rect(
        session(HTTOPLEFT, POINT { x: 100, y: 100 }),
        POINT { x: 80, y: 80 },
        (320, 200),
    );

    assert_eq!(rect.left, 80);
    assert_eq!(rect.top, 80);
    assert_eq!(rect.right, 500);
    assert_eq!(rect.bottom, 400);
}

#[test]
fn minimum_size_clamps_the_dragged_edge() {
    // 左边缘抓取点在 (100, 300)：拖动到 (490, 300) 时 dx = 390，左边越过右边界，
    // 钳制到 right - min_width = 500 - 320 = 180。
    let rect = resized_rect(
        session(HTLEFT, POINT { x: 100, y: 300 }),
        POINT { x: 490, y: 300 },
        (320, 200),
    );
    assert_eq!(
        rect.right - rect.left,
        320,
        "左边不能把窗口压到最小宽度以下"
    );
    assert_eq!(rect.right, 500);
    assert_eq!(rect.left, 180);
}
