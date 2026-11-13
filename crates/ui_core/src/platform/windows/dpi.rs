//! DPI 查询与「客户区 ↔ 窗口尺寸」换算（`design/phase-09-win32-demo.md` §6.4）。
//!
//! probe 实测（本机 120 dpi）：client 480×320 → window 496×359（96 dpi）/ 498×367（120 dpi）。
//! 非 96 dpi 下**必须**用 `AdjustWindowRectExForDpi`，否则窗口客户区会被系统按 96 dpi 的边框算错。

use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::{
        HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow},
        WindowsAndMessaging::{WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW},
    },
};

use crate::math::Scale;

/// DPI 基准（96 = 100%）。
pub(crate) const BASE_DPI: u32 = 96;

/// DPI → [`Scale`]；`0` 或非法值返回 `None`（调用方转 `EngineError::InvalidDpiScale`）。
pub(crate) fn scale_from_dpi(dpi: u32) -> Option<Scale> {
    Scale::new(dpi as f32 / BASE_DPI as f32)
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

/// 由客户区尺寸（物理像素）算出窗口尺寸（含系统标题栏与边框）。
pub(crate) fn window_size_for_client(client: (u32, u32), dpi: u32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client.0.max(1) as i32,
        bottom: client.1.max(1) as i32,
    };
    // SAFETY: `rect` 是栈上值；`AdjustWindowRectExForDpi` 只读写它。
    let adjusted = unsafe {
        AdjustWindowRectExForDpi(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
    };
    match adjusted {
        // 成功：rect 变成窗口矩形（左上可能为负，尺寸取差值）。
        Ok(()) => (rect.right - rect.left, rect.bottom - rect.top),
        // 失败：至少保证客户区不小于请求值（不静默缩小）。
        Err(_) => (client.0.max(1) as i32, client.1.max(1) as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::{BASE_DPI, scale_from_dpi, system_dpi, window_size_for_client};

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
        let at_96 = window_size_for_client((480, 320), 96);
        let at_120 = window_size_for_client((480, 320), 120);

        assert!(at_96.0 >= 480 && at_96.1 >= 320, "{at_96:?}");
        assert!(at_120.0 >= 480 && at_120.1 >= 320, "{at_120:?}");
        // 更高 DPI 下边框/标题栏按物理像素放大：窗口只会更大（或相等）。
        assert!(at_120.0 >= at_96.0 && at_120.1 >= at_96.1);
    }

    #[test]
    fn system_dpi_is_positive() {
        assert!(system_dpi() > 0);
    }
}
