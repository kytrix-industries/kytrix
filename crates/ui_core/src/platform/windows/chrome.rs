//! frameless（`Decorations::None`）的窗口 chrome：命中带计算、最大化客户区补偿、DWM 保留。
//!
//! ## 结构：系统仍拥有交互，引擎只改「客户区在哪 / 命中码是哪个」
//!
//! E6 不重写 caption 的 move 模态循环，也不重写 size 模态循环 —— 那样会丢掉 Aero Snap /
//! Win11 Snap Layouts / 双击最大化 / `Alt+Space`（§8.6.2 用户裁决）。frameless 只做两件事：
//!
//! 1. **`WM_NCCALCSIZE`（wparam == TRUE）返回 0**：客户区 = 整个窗口矩形（去掉系统标题栏与
//!    非客户区绘画）。窗口样式仍是 `WS_OVERLAPPEDWINDOW`（含 `WS_THICKFRAME` / `WS_CAPTION`），
//!    因此 **DWM 阴影、圆角、最大化动画、Snap 命中全部保留** —— 这是不换成 `WS_POPUP` 的
//!    唯一理由，也是本模块存在的边界。
//! 2. **`WM_NCHITTEST` 自算八向 resize 命中**（[`resize_edge_at`]）：返回 `HTLEFT`… /
//!    `HTBOTTOMRIGHT` 走系统 size 模态循环；应用声明的拖动区返回 `HTCAPTION` 走系统 move
//!    模态循环；其余 `HTCLIENT`。
//!
//! 最大化时窗口矩形会比显示器工作区**大一圈**边框（系统为装饰窗口预留的 frame 厚度）：
//! `WM_NCCALCSIZE` 必须向内补偿 [`inset_for_maximized`]，否则最大化后内容在屏幕四边被裁掉
//! 1–8 px（§11.6 的实现要点）。
//!
//! ## 取捨：不调 `DwmExtendFrameIntoClientArea`
//!
//! DWM 阴影由 `WS_THICKFRAME` 自动提供（`WS_EX_NOREDIRECTIONBITMAP` 不改变这一点）。把 frame
//! 扩展进客户区（`MARGINS{1,1,1,1}`）是「客户区需要 DWM 画边框」的路线，而本窗口的每一个像素
//! 都由 DirectComposition 提供（客户区没有重定向表面）—— 扩展 frame 会让 DWM 在内容上叠一层
//! 边框像素，与 DComp 输出冲突。因此只显式设置圆角偏好（[`apply_frameless_dwm`]）；失败时窗口
//! 回落成方角，仍完全可用（degrade, never panic，与 `dark_mode` 同口径）。

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

/// frameless 的八向 resize 命中（`WM_NCHITTEST` 的纯逻辑部分）。
///
/// 输入都是**物理像素、客户区局部坐标**（`WM_NCHITTEST` 的屏幕坐标先经 `ScreenToClient`）；
/// `client` 是当前客户区尺寸，`border` 是 [`crate::platform::windows::dpi::resize_border`] 给出的
/// DPI 缩放后的命中带厚度。返回 `None` = 客户区内部（调用方继续判定拖动区 / 返回 `HTCLIENT`）。
///
/// 边界语义（与系统 decorated 窗口一致）：
/// - 半开区间：`x < border_x` 是左、`x >= width - border_x` 是右；
/// - 角优先于边：`(x < bx, y < by)` ⇒ `TopLeft`；
/// - 极小窗口（`width < 2 * border_x`）把命中带钳到一半宽，左右/上下不重叠（不会出现「整个窗口
///   都是 resize 带」导致点不到内容）；
/// - 客户区外的点（拖动中系统会问略超出窗口的坐标）返回 `None`。
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

/// 命中边 → Win32 `WM_NCHITTEST` 命中码（走系统 size 模态循环的入场券）。
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

/// 最大化时的客户区补偿：把 `WM_NCCALCSIZE` 提议的窗口矩形向内缩 `border`。
///
/// 返回 `(left, top, right, bottom)`（屏幕物理坐标）。非最大化时调用方不做任何调整（客户区 =
/// 整个窗口矩形）。纯函数，DPI 缩放由调用方给出的 `border` 承担；退化窗口（小于 2×border）钳成
/// 零尺寸而不是反向矩形。
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

/// frameless 的 DWM 属性：圆角偏好（Win11）。
///
/// 默认值（`DWMWCP_DEFAULT`）在带非客户区的窗口上就是圆角，但**客户区铺满整个窗口**之后圆角
/// 不会自动出现（DWM 没有可裁的 frame），显式请求后才与系统窗口观感一致。失败只影响观感：
/// 返回 `false`，窗口继续可用（与 `dark_mode` 的降级口径一致，不 panic）。
///
/// 消费者：`NativeWindow::create`（frameless 路径，显示之前；`DwmSetWindowAttribute` 不派发
/// 窗口消息，可以在任何借用状态调用）。
pub(crate) fn apply_frameless_dwm(hwnd: HWND) -> bool {
    let preference = DWMWCP_ROUND;
    // SAFETY: `hwnd` 由调用方保证存活；属性值是指向栈上枚举的只读指针，长度 = 4 字节。
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
            // 不 panic：DWM 不可用（远程会话 / 合成器变体）时窗口仍必须可用，只是方角。
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

/// [`apply_frameless_dwm`] 失败诊断的一次性开关（不刷屏；同 `dark_mode` 口径）。
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

    /// 96 dpi 的窗口 800×600、命中带 8 px：八个方向 + 内部 + 边界点。
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
            // 半开区间的边界：8 是第一个「内部」像素，7 仍是命中带。
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

    /// DPI 缩放：命中带按物理像素放大（150% ⇒ 12 px；200% ⇒ 16 px），逻辑命中带不变。
    #[test]
    fn the_resize_border_scales_with_dpi() {
        let client = (800, 600);
        // 96 dpi：8 px 命中带；(7,300) 命中左边，(8,300) 起是内容。
        assert_eq!(
            resize_edge_at((7, 300), client, (8, 8)),
            Some(WindowResizeEdge::Left)
        );
        assert_eq!(resize_edge_at((8, 300), client, (8, 8)), None);
        // 150%：12 px（物理）= 8 逻辑 px。
        assert_eq!(
            resize_edge_at((11, 300), client, (12, 12)),
            Some(WindowResizeEdge::Left)
        );
        assert_eq!(resize_edge_at((12, 300), client, (12, 12)), None);
        // 200%：16 px（物理）= 8 逻辑 px。
        assert_eq!(
            resize_edge_at((15, 300), client, (16, 16)),
            Some(WindowResizeEdge::Left)
        );
        assert_eq!(resize_edge_at((16, 300), client, (16, 16)), None);
    }

    /// 极小窗口：命中带钳到一半，左右/上下不重叠（不会整窗都是 resize 带导致点不到内容）。
    #[test]
    fn tiny_windows_clamp_the_border_to_half_the_client_size() {
        // 窗口 10×10、命中带 8 ⇒ 每边钳到 5：整个窗口都是角/边命中带（内容不可点）。
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
        // 零命中带：整个客户区都是内部。
        assert_eq!(resize_edge_at((0, 0), (10, 10), (0, 0)), None);
        // 客户区外的点（拖动中系统会问）：不返回 resize 码。
        assert_eq!(resize_edge_at((-1, 5), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((5, -1), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((10, 5), (10, 10), (8, 8)), None);
        assert_eq!(resize_edge_at((5, 10), (10, 10), (8, 8)), None);
        // 非法客户区尺寸：没有可命中的像素。
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
        // 命中码不能与「客户区」撞车（否则 resize 带会被当成内容点）。
        assert_ne!(hit_test_code(WindowResizeEdge::Top), HTCLIENT);
    }

    /// 最大化补偿：四边各缩 `border`；非最大化的调用方不调用它（客户区 = 整个窗口）。
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
        // 退化输入不产生反向矩形（钳成零尺寸）。
        assert_eq!(inset_for_maximized((0, 0, 4, 4), (8, 8)), (8, 8, 8, 8));
    }
}
