use windows::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{
        DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DwmSetWindowAttribute,
    },
    UI::WindowsAndMessaging::{
        HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT,
    },
};

use crate::window::WindowResizeEdge;

#[must_use]
pub(crate) fn resize_edge_at(
    point: (i32, i32),
    client: (i32, i32),
    border: (i32, i32),
) -> Option<WindowResizeEdge> {
    let (x, y) = point;
    let (width, height) = client;
    if width <= 0 || height <= 0 || x < 0 || y < 0 || x >= width || y >= height {
        return None;
    }
    let border_x = border.0.clamp(0, width / 2);
    let border_y = border.1.clamp(0, height / 2);
    let left = x < border_x;
    let right = x >= width - border_x;
    let top = y < border_y;
    let bottom = y >= height - border_y;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(WindowResizeEdge::TopLeft),
        (_, true, true, _) => Some(WindowResizeEdge::TopRight),
        (true, _, _, true) => Some(WindowResizeEdge::BottomLeft),
        (_, true, _, true) => Some(WindowResizeEdge::BottomRight),
        (true, false, false, false) => Some(WindowResizeEdge::Left),
        (false, true, false, false) => Some(WindowResizeEdge::Right),
        (false, false, true, false) => Some(WindowResizeEdge::Top),
        (false, false, false, true) => Some(WindowResizeEdge::Bottom),
        _ => None,
    }
}

#[must_use]
pub(crate) const fn hit_test_code(edge: WindowResizeEdge) -> u32 {
    match edge {
        WindowResizeEdge::Top => HTTOP,
        WindowResizeEdge::Bottom => HTBOTTOM,
        WindowResizeEdge::Left => HTLEFT,
        WindowResizeEdge::Right => HTRIGHT,
        WindowResizeEdge::TopLeft => HTTOPLEFT,
        WindowResizeEdge::TopRight => HTTOPRIGHT,
        WindowResizeEdge::BottomLeft => HTBOTTOMLEFT,
        WindowResizeEdge::BottomRight => HTBOTTOMRIGHT,
    }
}

#[must_use]
pub(crate) fn inset_for_maximized(
    window: (i32, i32, i32, i32),
    border: (i32, i32),
) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = window;
    let left = left.saturating_add(border.0);
    let top = top.saturating_add(border.1);
    let right = right.saturating_sub(border.0).max(left);
    let bottom = bottom.saturating_sub(border.1).max(top);
    (left, top, right, bottom)
}

pub(crate) fn apply_frameless_dwm(hwnd: HWND) -> bool {
    let preference = DWMWCP_ROUND;
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const preference).cast(),
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };
    match result {
        Ok(()) => true,
        Err(error) => {
            if !DWM_FAILURE_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "kytrix: DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE) failed: \
                     {error:?}; the frameless window keeps square corners (window remains usable)"
                );
            }
            false
        }
    }
}

static DWM_FAILURE_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{
        HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT,
        HTTOPRIGHT,
    };

    use super::{hit_test_code, inset_for_maximized, resize_edge_at};
    use crate::window::WindowResizeEdge;

    #[test]
    fn the_eight_resize_zones_cover_the_expected_pixels() {
        let client = (800, 600);
        let border = (8, 8);
        let cases = [
            ((3, 3), Some(WindowResizeEdge::TopLeft)),
            ((796, 3), Some(WindowResizeEdge::TopRight)),
            ((3, 596), Some(WindowResizeEdge::BottomLeft)),
            ((796, 596), Some(WindowResizeEdge::BottomRight)),
            ((400, 3), Some(WindowResizeEdge::Top)),
            ((400, 596), Some(WindowResizeEdge::Bottom)),
            ((3, 300), Some(WindowResizeEdge::Left)),
            ((796, 300), Some(WindowResizeEdge::Right)),
            ((400, 300), None),
            ((8, 8), None),
            ((7, 8), Some(WindowResizeEdge::Left)),
            ((8, 7), Some(WindowResizeEdge::Top)),
            ((792, 300), Some(WindowResizeEdge::Right)),
            ((791, 300), None),
        ];
        for (point, expected) in cases {
            assert_eq!(
                resize_edge_at(point, client, border),
                expected,
                "point {point:?}"
            );
        }
    }

    #[test]
    fn the_resize_border_scales_with_dpi() {
        let client = (800, 600);
        assert_eq!(
            resize_edge_at((7, 300), client, (8, 8)),
            Some(WindowResizeEdge::Left)
        );
        assert_eq!(resize_edge_at((8, 300), client, (8, 8)), None);
        assert_eq!(
            resize_edge_at((11, 300), client, (12, 12)),
            Some(WindowResizeEdge::Left)
        );
        assert_eq!(resize_edge_at((12, 300), client, (12, 12)), None);
        assert_eq!(
            resize_edge_at((15, 300), client, (16, 16)),
            Some(WindowResizeEdge::Left)
        );
        assert_eq!(resize_edge_at((16, 300), client, (16, 16)), None);
    }

    #[test]
    fn tiny_windows_clamp_the_border_to_half_the_client_size() {
        assert_eq!(
            resize_edge_at((0, 4), (10, 10), (8, 8)),
            Some(WindowResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge_at((9, 4), (10, 10), (8, 8)),
            Some(WindowResizeEdge::TopRight)
        );
        assert_eq!(
            resize_edge_at((0, 5), (10, 10), (8, 8)),
            Some(WindowResizeEdge::BottomLeft),
            "上下命中带也被钳到一半 ⇒ 角优先于边"
        );
        assert_eq!(resize_edge_at((0, 0), (10, 10), (0, 0)), None);
        assert_eq!(resize_edge_at((-1, 5), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((5, -1), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((10, 5), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((5, 10), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((0, 0), (0, 0), (8, 8)), None);
    }

    #[test]
    fn every_edge_maps_to_its_system_hit_test_code() {
        for (edge, code) in [
            (WindowResizeEdge::Top, HTTOP),
            (WindowResizeEdge::Bottom, HTBOTTOM),
            (WindowResizeEdge::Left, HTLEFT),
            (WindowResizeEdge::Right, HTRIGHT),
            (WindowResizeEdge::TopLeft, HTTOPLEFT),
            (WindowResizeEdge::TopRight, HTTOPRIGHT),
            (WindowResizeEdge::BottomLeft, HTBOTTOMLEFT),
            (WindowResizeEdge::BottomRight, HTBOTTOMRIGHT),
        ] {
            assert_eq!(hit_test_code(edge), code, "{edge:?}");
        }
        assert_ne!(hit_test_code(WindowResizeEdge::Top), HTCLIENT);
    }

    #[test]
    fn maximized_client_area_is_inset_by_the_frame_thickness() {
        let window = (-8, -8, 1928, 1088);
        assert_eq!(
            inset_for_maximized(window, (8, 8)),
            (0, 0, 1920, 1080),
            "最大化后客户区必须正好是工作区（否则屏幕四边各被裁 8 px）"
        );
        assert_eq!(
            inset_for_maximized((0, 0, 100, 100), (0, 0)),
            (0, 0, 100, 100),
            "零边框（非 Win11 / 未缩放）不变"
        );
        assert_eq!(inset_for_maximized((0, 0, 4, 4), (8, 8)), (8, 8, 8, 8));
    }

    #[test]
    fn maximized_inset_matches_the_work_area_at_every_dpi() {
        let work_area = (0, 0, 1920, 1080);
        for dpi in [96, 120, 144, 192] {
            let border = crate::platform::windows::dpi::resize_border(dpi);
            assert!(
                border.0 > 0 && border.1 > 0,
                "{dpi} dpi 的 resize 命中带必须为正：{border:?}"
            );
            let maximized_window = (
                work_area.0 - border.0,
                work_area.1 - border.1,
                work_area.2 + border.0,
                work_area.3 + border.1,
            );
            assert_eq!(
                inset_for_maximized(maximized_window, border),
                work_area,
                "{dpi} dpi：最大化客户区必须逐像素等于工作区（补偿 = 同一个 resize_border）"
            );
        }
    }
}
