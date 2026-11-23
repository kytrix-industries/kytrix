use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::{
        HiDpi::{
            AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi,
        },
        WindowsAndMessaging::{
            SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYSIZEFRAME, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
        },
    },
};

use crate::{
    math::{Point, Rect, Scale, Size},
    window::Decorations,
};

pub(crate) const BASE_DPI: u32 = 96;

pub(crate) const MIN_SIZE_LOGICAL: Size = Size::new(320.0, 200.0);

pub(crate) fn track_size(client: Size, dpi: u32, decorations: Decorations) -> (u32, u32) {
    let scale = scale_from_dpi(dpi).unwrap_or(Scale::ONE);
    let physical = scale_client(client, scale);
    let window = window_size_for_client(physical, dpi, decorations);
    (window.0.max(1) as u32, window.1.max(1) as u32)
}

pub(crate) fn scale_from_dpi(dpi: u32) -> Option<Scale> {
    Scale::new(dpi as f32 / BASE_DPI as f32)
}

pub(crate) fn physical_origin(origin: Point, scale: Scale) -> (i32, i32) {
    let rect = Rect::new(origin.x, origin.y, 0.0, 0.0).to_physical_rounded(scale);
    (rect.x0, rect.y0)
}

pub(crate) fn system_dpi() -> u32 {
    unsafe { GetDpiForSystem() }.max(1)
}

pub(crate) fn window_dpi(hwnd: HWND) -> u32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { system_dpi() } else { dpi }
}

pub(crate) fn window_size_for_client(
    client: (u32, u32),
    dpi: u32,
    decorations: Decorations,
) -> (i32, i32) {
    match adjusted_window_rect(client, dpi, decorations) {
        Some((_, _, width, height)) => (width, height),
        None => (client.0.max(1) as i32, client.1.max(1) as i32),
    }
}

pub(crate) fn window_origin_for_client(
    client_origin: (i32, i32),
    client_size: (u32, u32),
    dpi: u32,
    decorations: Decorations,
) -> (i32, i32) {
    match adjusted_window_rect(client_size, dpi, decorations) {
        Some((left, top, _, _)) => (client_origin.0 + left, client_origin.1 + top),
        None => client_origin,
    }
}

fn adjusted_window_rect(
    client: (u32, u32),
    dpi: u32,
    decorations: Decorations,
) -> Option<(i32, i32, i32, i32)> {
    let width = client.0.max(1) as i32;
    let height = client.1.max(1) as i32;
    if decorations == Decorations::None {
        return Some((0, 0, width, height));
    }
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    unsafe {
        AdjustWindowRectExForDpi(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
    }
    .ok()?;
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

pub(crate) fn resize_border(dpi: u32) -> (i32, i32) {
    unsafe {
        let frame_x = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi).max(1);
        let frame_y = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi).max(1);
        let padded = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi).max(0);
        (frame_x + padded, frame_y + padded)
    }
}

fn scale_client(client: Size, scale: Scale) -> (u32, u32) {
    let width = (client.width.max(0.0) * scale.factor()).round();
    let height = (client.height.max(0.0) * scale.factor()).round();
    (width.max(0.0) as u32, height.max(0.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::{
        BASE_DPI, MIN_SIZE_LOGICAL, physical_origin, resize_border, scale_from_dpi, system_dpi,
        track_size, window_origin_for_client, window_size_for_client,
    };
    use crate::{
        math::{Point, Scale, Size},
        window::Decorations,
    };

    #[test]
    fn scale_table_matches_the_dpi_ratio() {
        assert_eq!(scale_from_dpi(96).map(|scale| scale.factor()), Some(1.0));
        assert_eq!(scale_from_dpi(120).map(|scale| scale.factor()), Some(1.25));
        assert_eq!(scale_from_dpi(144).map(|scale| scale.factor()), Some(1.5));
        assert_eq!(scale_from_dpi(192).map(|scale| scale.factor()), Some(2.0));
        assert!(scale_from_dpi(0).is_none(), "0 dpi 不是合法比例");
        assert_eq!(BASE_DPI, 96);
    }

    #[test]
    fn window_size_is_never_smaller_than_the_client_area() {
        let at_96 = window_size_for_client((480, 320), 96, Decorations::Native);
        let at_120 = window_size_for_client((480, 320), 120, Decorations::Native);

        assert!(at_96.0 >= 480 && at_96.1 >= 320, "{at_96:?}");
        assert!(at_120.0 >= 480 && at_120.1 >= 320, "{at_120:?}");
        assert!(at_120.0 >= at_96.0 && at_120.1 >= at_96.1);
    }

    #[test]
    fn frameless_windows_are_exactly_their_client_area() {
        for dpi in [96, 120, 144, 192] {
            assert_eq!(
                window_size_for_client((480, 320), dpi, Decorations::None),
                (480, 320),
                "{dpi} dpi frameless：窗口矩形 = 客户区"
            );
            assert_eq!(
                window_origin_for_client((500, 400), (480, 320), dpi, Decorations::None),
                (500, 400),
                "{dpi} dpi frameless：客户区原点 = 窗口原点"
            );
            let native = window_size_for_client((480, 320), dpi, Decorations::Native);
            assert!(
                native.0 > 480 && native.1 > 320,
                "{dpi} dpi 的 Native 必须带非客户区（否则本用例没有区分力）：{native:?}"
            );
        }
    }

    #[test]
    fn system_dpi_is_positive() {
        assert!(system_dpi() > 0);
    }

    #[test]
    fn the_window_origin_is_shifted_by_the_non_client_area() {
        for dpi in [96, 120, 144, 192] {
            let client = (480, 320);
            let client_origin = (500, 400);
            let (left, top) =
                window_origin_for_client(client_origin, client, dpi, Decorations::Native);
            let (width, height) = window_size_for_client(client, dpi, Decorations::Native);
            let window = crate::math::PhysicalRect::new(left, top, left + width, top + height);
            assert!(
                window.x0 <= client_origin.0
                    && window.y0 <= client_origin.1
                    && window.x1 >= client_origin.0 + client.0 as i32
                    && window.y1 >= client_origin.1 + client.1 as i32,
                "窗口外框必须包含客户区矩形（{dpi} dpi，window={window:?}）"
            );
            assert!(
                window.width() > client.0 as i32 && window.height() > client.1 as i32,
                "非客户区只会让窗口更大（{dpi} dpi）"
            );
        }
    }

    #[test]
    fn logical_origins_are_rounded_to_physical_pixels() {
        assert_eq!(
            physical_origin(Point::new(150.0, 120.0), Scale::ONE),
            (150, 120)
        );
        let scale = Scale::new(1.25).expect("scale");
        assert_eq!(
            physical_origin(Point::new(150.0, 120.0), scale),
            (188, 150),
            "150×1.25=187.5 round 到 188；120×1.25=150"
        );
        assert_eq!(
            physical_origin(Point::new(-150.0, -120.0), scale),
            (-188, -150),
            "负坐标按 half away from zero 取整"
        );
    }

    #[test]
    fn the_track_size_is_a_window_rect_for_the_logical_client_size() {
        let at_96 = track_size(MIN_SIZE_LOGICAL, 96, Decorations::Native);
        let client_96 = (
            MIN_SIZE_LOGICAL.width as u32,
            MIN_SIZE_LOGICAL.height as u32,
        );
        assert!(
            at_96.0 >= client_96.0 && at_96.1 >= client_96.1,
            "外框不得小于客户区最小值：{at_96:?}"
        );
        assert!(
            at_96.0 > client_96.0 && at_96.1 > client_96.1,
            "96 dpi 下也必须有边框/标题栏：{at_96:?}"
        );

        let at_144 = track_size(Size::new(480.0, 300.0), 144, Decorations::Native);
        assert!(
            at_144.0 >= (480.0f32 * 1.5) as u32 && at_144.1 >= (300.0f32 * 1.5) as u32,
            "{at_144:?}"
        );
        assert!(
            at_144.0 >= at_96.0 && at_144.1 >= at_96.1,
            "更高 DPI 的最小外框不得比 96 dpi 更小"
        );

        assert_eq!(
            track_size(MIN_SIZE_LOGICAL, 96, Decorations::None),
            (320, 200)
        );
        assert_eq!(
            track_size(Size::new(480.0, 300.0), 144, Decorations::None),
            (720, 450)
        );
    }

    #[test]
    fn the_track_size_follows_the_requested_client_size() {
        let small = track_size(Size::new(100.0, 80.0), 96, Decorations::Native);
        let large = track_size(Size::new(1000.0, 800.0), 96, Decorations::Native);
        assert!(
            small.0 < large.0 && small.1 < large.1,
            "更小的客户区必须得到更小的外框：{small:?} vs {large:?}"
        );
        assert!(
            small.0 >= 100 && small.1 >= 80,
            "外框至少覆盖客户区：{small:?}"
        );
    }

    #[test]
    fn the_frameless_resize_border_is_dpi_scaled_and_positive() {
        let at_96 = resize_border(96);
        assert!(
            at_96.0 >= 8 && at_96.1 >= 8,
            "96 dpi 的命中带至少 8 px（SM_CXSIZEFRAME + SM_CXPADDEDBORDER）：{at_96:?}"
        );
        for dpi in [120, 144, 192] {
            let border = resize_border(dpi);
            assert!(
                border.0 >= at_96.0 && border.1 >= at_96.1,
                "{dpi} dpi: {border:?}"
            );
        }
    }
}
