//! DPI 查询与「客户区 ↔ 窗口尺寸/位置」换算。
//!
//! probe 实测（本机 120 dpi）：client 480×320 → window 496×359（96 dpi）/ 498×367（120 dpi）。
//! 非 96 dpi 下**必须**用 `AdjustWindowRectExForDpi`，否则窗口客户区会被系统按 96 dpi 的边框算错。

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

/// DPI 基准（96 = 100%）。
pub(crate) const BASE_DPI: u32 = 96;

/// 引擎默认的最小客户区（逻辑像素；B3 起 `WindowOptions::min_size = None` 时用它，
/// [`track_size`] 换算成 `WM_GETMINMAXINFO` 的外框尺寸）。
pub(crate) const MIN_SIZE_LOGICAL: Size = Size::new(320.0, 200.0);

/// 客户区逻辑尺寸 → `WM_GETMINMAXINFO` 的跟踪尺寸（`ptMinTrackSize` / `ptMaxTrackSize`，
/// **窗口外框**，物理像素）。
///
/// `ptMinTrackSize` 是外框尺寸，而应用给的是**客户区**最小尺寸（P2-1）：直接透传会让窗口
/// 最小尺寸比预期小一个边框 + 标题栏（150% DPI 下尤其明显），于是能缩到客户区只有几十像素。
/// `WndProcState::note_dpi` 与 `NativeWindow::apply_dpi_and_size` 都走这里，避免两处换算漂移。
///
/// `decorations`：frameless 的窗口外框 = 客户区（`WM_NCCALCSIZE` 返回 0），因此跟踪尺寸不能
/// 再加系统边框 / 标题栏 —— 否则 frameless 窗口的最小尺寸会凭空大一圈。
pub(crate) fn track_size(client: Size, dpi: u32, decorations: Decorations) -> (u32, u32) {
    let scale = scale_from_dpi(dpi).unwrap_or(Scale::ONE);
    let physical = scale_client(client, scale);
    let window = window_size_for_client(physical, dpi, decorations);
    // 外框尺寸恒为正（`window_size_for_client` 只加边框）。
    (window.0.max(1) as u32, window.1.max(1) as u32)
}

/// DPI → [`Scale`]；`0` 或非法值返回 `None`（调用方转 `EngineError::InvalidDpiScale`）。
pub(crate) fn scale_from_dpi(dpi: u32) -> Option<Scale> {
    Scale::new(dpi as f32 / BASE_DPI as f32)
}

/// 逻辑原点 → 物理像素原点（`round`，误差 ≤0.5 px）：窗口落点坐标换算的唯一入口。
pub(crate) fn physical_origin(origin: Point, scale: Scale) -> (i32, i32) {
    // `to_physical_rounded` 对原点逐坐标 round、对尺寸整体 round；零尺寸矩形只剩原点。
    let rect = Rect::new(origin.x, origin.y, 0.0, 0.0).to_physical_rounded(scale);
    (rect.x0, rect.y0)
}

/// 系统 DPI（`SetProcessDpiAwarenessContext` 之后才准）。
pub(crate) fn system_dpi() -> u32 {
    // SAFETY: 无参数、无指针的查询；任意线程可调。
    unsafe { GetDpiForSystem() }.max(1)
}

/// 窗口 DPI；窗口尚未绑定显示器时返回系统 DPI。
pub(crate) fn window_dpi(hwnd: HWND) -> u32 {
    // SAFETY: `hwnd` 由 `NativeWindow` 持有且存活。
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { system_dpi() } else { dpi }
}

/// 由客户区尺寸（物理像素）算出窗口尺寸（含系统标题栏与边框；frameless 时窗口 = 客户区）。
pub(crate) fn window_size_for_client(
    client: (u32, u32),
    dpi: u32,
    decorations: Decorations,
) -> (i32, i32) {
    match adjusted_window_rect(client, dpi, decorations) {
        // 成功：rect 变成窗口矩形（左上为负的边框/标题栏，尺寸取差值）。
        Some((_, _, width, height)) => (width, height),
        // 失败：至少保证客户区不小于请求值（不静默缩小）。
        None => (client.0.max(1) as i32, client.1.max(1) as i32),
    }
}

/// 客户区左上角（物理屏幕坐标）→ 窗口外框左上角（物理屏幕坐标）。
///
/// `AdjustWindowRectExForDpi` 给出的 `left/top` 是负的非客户区宽度/高度（边框 + 标题栏），
/// 窗口原点 = 客户区原点 + 它们。窗口落点的「客户区左上角语义」由这里保证。
///
/// frameless 时没有非客户区（`WM_NCCALCSIZE` 返回 0），窗口原点 = 客户区原点。
pub(crate) fn window_origin_for_client(
    client_origin: (i32, i32),
    client_size: (u32, u32),
    dpi: u32,
    decorations: Decorations,
) -> (i32, i32) {
    match adjusted_window_rect(client_size, dpi, decorations) {
        Some((left, top, _, _)) => (client_origin.0 + left, client_origin.1 + top),
        // 非法 DPI / 系统调用失败：位置语义至少不退化成 (0, 0)，按客户区原点处理。
        None => client_origin,
    }
}

/// 客户区物理尺寸 → 窗口外框 `(left, top, width, height)`。
///
/// `Decorations::Native`：`AdjustWindowRectExForDpi`（系统会补标题栏 + 边框）；
/// `Decorations::None`：窗口外框 = 客户区（`(0, 0, width, height)`）—— `WM_NCCALCSIZE` 返回 0
/// 让客户区铺满整个窗口，窗口矩形若再大一圈边框，客户区就会比请求值大（frameless 下非客户区
/// 在窗口矩形**内部**，只是命中带）。
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
    // SAFETY: `rect` 是栈上值；`AdjustWindowRectExForDpi` 只读写它。
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

/// 自绘标题栏（frameless）的 resize 命中带厚度（物理像素，按窗口 DPI 换算）。
///
/// `SM_CXSIZEFRAME` / `SM_CYSIZEFRAME` + `SM_CXPADDEDBORDER`：Win11 96 dpi 下是 4 + 4 = 8 px
/// （可见边框 1 px + 8 px 命中带里的其余部分由 DWM 的 padding 提供）。`WM_NCHITTEST` 的命中带与
/// `WM_NCCALCSIZE` 的最大化补偿**必须**用同一个值，否则缩放带与客户区边缘错位。
pub(crate) fn resize_border(dpi: u32) -> (i32, i32) {
    // SAFETY: `GetSystemMetricsForDpi` 是无副作用的线程局部查询；`dpi` 来自 `GetDpiForWindow`
    // （0 已由 `window_dpi` 兜底成系统 DPI）。
    unsafe {
        let frame_x = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi).max(1);
        let frame_y = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi).max(1);
        // 系统只有 `SM_CXPADDEDBORDER`（窗口 padding 四边同宽）。
        let padded = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi).max(0);
        (frame_x + padded, frame_y + padded)
    }
}

/// 逻辑客户区尺寸 → 物理像素（`round`，至少 1 px；负尺寸钳到 0 再取整）。
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
        // 更高 DPI 下边框/标题栏按物理像素放大：窗口只会更大（或相等）。
        assert!(at_120.0 >= at_96.0 && at_120.1 >= at_96.1);
    }

    /// E6 frameless：窗口外框 = 客户区（`WM_NCCALCSIZE` 返回 0），不加标题栏/边框；
    /// 落点也无非客户区偏移。与 `Native` 的差必须真实存在（防「两个分支其实一样」）。
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

    /// B3：客户区原点换算成窗口原点时，窗口外框必须**包含**客户区矩形（左/上各有边框与
    /// 标题栏，右/下补齐同样的非客户区宽度）—— 位置换算与尺寸换算必须来自同一次
    /// `AdjustWindowRectExForDpi`，否则窗口会整体偏几个像素。
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

    /// B3：逻辑点 → 物理原点是 `round`（误差 ≤0.5 px），负坐标同样正确。
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

    /// `ptMinTrackSize` 是**外框**尺寸：必须比应用给的客户区最小尺寸大（多出边框 + 标题栏），
    /// 且随 DPI 单调增长。直接把客户区尺寸当外框会让窗口能缩到客户区只剩几十像素（P2-1）。
    /// frameless 时外框 = 客户区（否则最小尺寸会凭空大一圈）。
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

        // 150%：客户区 480×300，外框只可能更大。
        let at_144 = track_size(Size::new(480.0, 300.0), 144, Decorations::Native);
        assert!(
            at_144.0 >= (480.0f32 * 1.5) as u32 && at_144.1 >= (300.0f32 * 1.5) as u32,
            "{at_144:?}"
        );
        assert!(
            at_144.0 >= at_96.0 && at_144.1 >= at_96.1,
            "更高 DPI 的最小外框不得比 96 dpi 更小"
        );

        // frameless：跟踪尺寸就是客户区物理尺寸（96 dpi 下 = 逻辑尺寸）。
        assert_eq!(
            track_size(MIN_SIZE_LOGICAL, 96, Decorations::None),
            (320, 200)
        );
        assert_eq!(
            track_size(Size::new(480.0, 300.0), 144, Decorations::None),
            (720, 450)
        );
    }

    /// 参数化（B3）：同一 DPI 下最小/最大跟踪尺寸只由传入的客户区逻辑尺寸决定，
    /// 与 P2-1 的默认最小尺寸无关。
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

    /// frameless 的 `WM_NCHITTEST` 命中带 / `WM_NCCALCSIZE` 最大化补偿共用同一个厚度：
    /// 必须是正的、随 DPI 单调不减（96 dpi 实测 4 + 4 = 8）。
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
