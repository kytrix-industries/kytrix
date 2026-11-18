//! 自持窗口缩放（P-A）：命中码 → 会话 → 目标外框的纯数据与纯函数。
//!
//! 为什么 8 个 resize 命中码**不再**交 `DefWindowProcW`：系统的 size 模态循环会**占住 UI 线程
//! 的消息泵**，拖动期间 `App::frame` 不运行 —— 布局不重排、内容不按新尺寸重绘、DirectComposition
//! 背景层也停在上一个客户区（新区域没有 visual ⇒ 露出桌面）。而产品前提是「不用定时器驱动刷新」
//! 且「resize 必须真实重排 + 重绘，不能拉伸旧帧」，两条合起来只允许一种实现：**缩放由引擎自己
//! 按消息驱动**，泵每轮跑一次真实帧。
//!
//! 分工：
//! - `WndProc` 只**记账**（[`super::WndProcState::begin_resize`] /
//!   [`super::WndProcState::note_resize_cursor`]），不做任何会重入 `WndProc` 的调用
//!   （`wndproc.rs` 文件头纪律 1）；
//! - 泵在 `collect_events` **之前**统一 `SetWindowPos` 一次（`pump::apply_pending_resizes`），
//!   同步产生的 `WM_SIZE` 走现有事件链路：`set_size` 标 `LAYOUT` + `set_surface` 标全量 damage；
//! - 出帧合并只靠「消息队列天然成批 + `pending_resize` 最新赢 + 上一帧未上屏就跳过」
//!   （`pump::ResizePacing`，**没有**任何定时器）。
//!
//! `HTCAPTION` / 系统按钮**仍然交系统**：Aero Snap / Snap Layouts / 双击最大化 / 右键系统菜单
//! 只存在于系统的 move 模态循环里（见 `wndproc.rs` 文件头纪律 2）。
//!
//! ## 与系统 size 模态循环的已知差异（有意，不补）
//!
//! 会话只吞 `WM_MOUSEMOVE` / `WM_LBUTTONUP` / `WM_KEYDOWN(VK_ESCAPE)` / 失去捕获 / `WM_CANCELMODE`，
//! 其余消息照常走生产路径。因此与系统循环相比有三处差异：
//! - 缩放期间应用仍会收到键盘事件（系统循环吞掉全部输入）：补一层输入闸门需要额外的会话状态，
//!   而「拖边框的同时打字」不是真实路径；
//! - Esc 取消会话后，它的 `WM_KEYUP` 仍会作为应用的按键事件投递（系统循环会一起吞掉）；
//! - 边框双击后的第二次 `WM_LBUTTONUP` 会作为一次**孤立** `PointerUp` 投递（没有配对的
//!   `PointerDown`）：交互层对孤立抬键是 no-op。

use windows::Win32::UI::WindowsAndMessaging::{
    HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT,
};

/// 缩放会话拖动的边 / 角。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeEdge {
    /// 命中码 → 边 / 角；非缩放命中码（`HTCAPTION` / 系统按钮 / 客户区…）返回 `None`。
    pub(crate) fn from_hit(hit: u32) -> Option<Self> {
        match hit {
            HTLEFT => Some(Self::Left),
            HTRIGHT => Some(Self::Right),
            HTTOP => Some(Self::Top),
            HTBOTTOM => Some(Self::Bottom),
            HTTOPLEFT => Some(Self::TopLeft),
            HTTOPRIGHT => Some(Self::TopRight),
            HTBOTTOMLEFT => Some(Self::BottomLeft),
            HTBOTTOMRIGHT => Some(Self::BottomRight),
            _ => None,
        }
    }

    /// 该边 / 角会拖动外框的左边。
    const fn moves_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    /// 该边 / 角会拖动外框的右边。
    const fn moves_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    /// 该边 / 角会拖动外框的上边。
    const fn moves_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    /// 该边 / 角会拖动外框的下边。
    const fn moves_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
}

/// 窗口**外框**矩形（屏幕物理像素，含系统标题栏与边框）。
///
/// 缩放会话里的边界都是外框边界：`WM_NCLBUTTONDOWN` 的命中发生在非客户区，
/// `SetWindowPos` 的 `(cx, cy)` 也是外框尺寸；客户区由系统按边框算，不在本模块出现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OuterRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl OuterRect {
    /// 由四条边构造（右 / 下为**排他**边界，与 Win32 `RECT` 同义）。
    pub(crate) const fn from_ltrb(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// `SetWindowPos` 的 `(x, y, cx, cy)`。
    ///
    /// 尺寸用 `saturating_sub`：极端光标 + 极端最小尺寸（测试里的 `u32::MAX`）可以让外框
    /// 跨满整个 `i32` 值域，普通减法会溢出；真实窗口的坐标远小于该范围。
    pub(crate) const fn as_window_rect(self) -> (i32, i32, i32, i32) {
        (
            self.left,
            self.top,
            self.right.saturating_sub(self.left),
            self.bottom.saturating_sub(self.top),
        )
    }
}

/// 一次自持缩放会话：起始外框 + 起始光标 + 拖动的边 / 角。
///
/// 起始光标与起始外框都取**按下那一刻**的值（屏幕物理像素），目标外框只按光标位移计算 ⇒
/// 抓取点相对边框的偏移保持不变（与系统模态循环的手感一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResizeSession {
    start_outer: OuterRect,
    start_cursor: (i32, i32),
    edge: ResizeEdge,
}

impl ResizeSession {
    pub(crate) const fn new(
        start_outer: OuterRect,
        start_cursor: (i32, i32),
        edge: ResizeEdge,
    ) -> Self {
        Self {
            start_outer,
            start_cursor,
            edge,
        }
    }

    /// 会话起始外框（Esc 取消时恢复它）。
    pub(crate) const fn start_outer(self) -> OuterRect {
        self.start_outer
    }

    /// 按最新光标算目标外框：起始的**对边固定**、拖动边跟着光标位移走。
    ///
    /// `min_outer` 是最小外框（`WndProcState::min_size`，`WM_GETMINMAXINFO` 用的同一份换算）：
    /// 拖动边最多压到「对边 − 最小尺寸」，因此反向拖动 / 越过对边都不会产生 0 或负尺寸，也不会
    /// 左右 / 上下翻转。位移用 `saturating_add` 吸收异常大的坐标，不溢出。
    pub(crate) fn target_outer(self, cursor: (i32, i32), min_outer: (u32, u32)) -> OuterRect {
        let dx = cursor.0.saturating_sub(self.start_cursor.0);
        let dy = cursor.1.saturating_sub(self.start_cursor.1);
        let mut rect = self.start_outer;
        if self.edge.moves_left() {
            rect.left = self.start_outer.left.saturating_add(dx);
        }
        if self.edge.moves_right() {
            rect.right = self.start_outer.right.saturating_add(dx);
        }
        if self.edge.moves_top() {
            rect.top = self.start_outer.top.saturating_add(dy);
        }
        if self.edge.moves_bottom() {
            rect.bottom = self.start_outer.bottom.saturating_add(dy);
        }
        let min_width = i32::try_from(min_outer.0).unwrap_or(i32::MAX).max(1);
        let min_height = i32::try_from(min_outer.1).unwrap_or(i32::MAX).max(1);
        if self.edge.moves_left() {
            rect.left = rect.left.min(rect.right.saturating_sub(min_width));
        }
        if self.edge.moves_right() {
            rect.right = rect.right.max(rect.left.saturating_add(min_width));
        }
        if self.edge.moves_top() {
            rect.top = rect.top.min(rect.bottom.saturating_sub(min_height));
        }
        if self.edge.moves_bottom() {
            rect.bottom = rect.bottom.max(rect.top.saturating_add(min_height));
        }
        // 未参与拖动的轴保持起始尺寸；起始外框本身退化时也保证正尺寸（`SetWindowPos` 的
        // 0 宽 / 0 高窗口不可用）。
        if rect.right <= rect.left {
            rect.right = rect.left.saturating_add(1);
        }
        if rect.bottom <= rect.top {
            rect.bottom = rect.top.saturating_add(1);
        }
        rect
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{
        HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON,
        HTMINBUTTON, HTNOWHERE, HTRIGHT, HTSYSMENU, HTTOP, HTTOPLEFT, HTTOPRIGHT,
    };

    use super::{OuterRect, ResizeEdge, ResizeSession};

    /// 320×240 客户区在 96 dpi 下的最小外框（`dpi::min_track_size` 的量级；只用于几何断言）。
    const MIN: (u32, u32) = (336, 239);

    fn session(start: OuterRect, cursor: (i32, i32), edge: ResizeEdge) -> ResizeSession {
        ResizeSession::new(start, cursor, edge)
    }

    fn start() -> OuterRect {
        OuterRect::from_ltrb(100, 100, 500, 400)
    }

    /// 只有 8 个 HT 缩放码是缩放命中；标题栏 / 系统按钮 / 客户区必须返回 `None`
    /// （它们要继续交给系统的 move 模态循环：Aero Snap / Snap Layouts / 双击最大化）。
    #[test]
    fn only_the_eight_resize_hit_codes_start_a_resize() {
        let cases = [
            (HTLEFT, ResizeEdge::Left),
            (HTRIGHT, ResizeEdge::Right),
            (HTTOP, ResizeEdge::Top),
            (HTBOTTOM, ResizeEdge::Bottom),
            (HTTOPLEFT, ResizeEdge::TopLeft),
            (HTTOPRIGHT, ResizeEdge::TopRight),
            (HTBOTTOMLEFT, ResizeEdge::BottomLeft),
            (HTBOTTOMRIGHT, ResizeEdge::BottomRight),
        ];
        for (hit, edge) in cases {
            assert_eq!(
                ResizeEdge::from_hit(hit),
                Some(edge),
                "命中码 {hit} 必须是 {edge:?}"
            );
        }
        for hit in [
            HTCAPTION,
            HTSYSMENU,
            HTCLOSE,
            HTMINBUTTON,
            HTMAXBUTTON,
            HTCLIENT,
            HTNOWHERE,
        ] {
            assert_eq!(
                ResizeEdge::from_hit(hit),
                None,
                "命中码 {hit} 不是缩放码：必须继续交系统，否则 Snap / 双击最大化会失效"
            );
        }
    }

    /// 单边拖动：只有该边跟着光标走，其余三边保持起始值。
    #[test]
    fn a_single_edge_follows_the_cursor_and_keeps_the_opposite_edge() {
        let s = start();
        assert_eq!(
            session(s, (160, 220), ResizeEdge::Left).target_outer((140, 220), MIN),
            OuterRect::from_ltrb(80, 100, 500, 400),
            "左边：left 跟 -20，其余不变"
        );
        assert_eq!(
            session(s, (160, 220), ResizeEdge::Right).target_outer((190, 220), MIN),
            OuterRect::from_ltrb(100, 100, 530, 400),
            "右边：right 跟 +30"
        );
        assert_eq!(
            session(s, (160, 220), ResizeEdge::Top).target_outer((160, 200), MIN),
            OuterRect::from_ltrb(100, 80, 500, 400),
            "上边：top 跟 -20"
        );
        assert_eq!(
            session(s, (160, 220), ResizeEdge::Bottom).target_outer((160, 250), MIN),
            OuterRect::from_ltrb(100, 100, 500, 430),
            "下边：bottom 跟 +30"
        );
    }

    /// 角拖动：两条边同时跟光标走。
    #[test]
    fn a_corner_moves_both_axes() {
        let s = start();
        assert_eq!(
            session(s, (160, 220), ResizeEdge::TopLeft).target_outer((140, 210), MIN),
            OuterRect::from_ltrb(80, 90, 500, 400)
        );
        assert_eq!(
            session(s, (160, 220), ResizeEdge::TopRight).target_outer((190, 210), MIN),
            OuterRect::from_ltrb(100, 90, 530, 400)
        );
        assert_eq!(
            session(s, (160, 220), ResizeEdge::BottomLeft).target_outer((140, 250), MIN),
            OuterRect::from_ltrb(80, 100, 500, 430)
        );
        assert_eq!(
            session(s, (160, 220), ResizeEdge::BottomRight).target_outer((190, 250), MIN),
            OuterRect::from_ltrb(100, 100, 530, 430)
        );
    }

    /// 反向拖动（越过对边）停在最小外框：不产生 0 / 负尺寸，也不翻转。
    #[test]
    fn dragging_past_the_opposite_edge_stops_at_the_minimum() {
        let s = start();
        // 右边拖到起点左侧很远：宽度 = 最小外框宽，右边界贴着左边界。
        let rect = session(s, (160, 220), ResizeEdge::Right).target_outer((0, 220), MIN);
        assert_eq!(rect.as_window_rect(), (100, 100, MIN.0 as i32, 400 - 100));
        // 左边拖到起点右侧很远：left = right − 最小宽。
        let rect = session(s, (160, 220), ResizeEdge::Left).target_outer((9_000, 220), MIN);
        assert_eq!(
            rect.as_window_rect(),
            (500 - MIN.0 as i32, 100, MIN.0 as i32, 300)
        );
        // 上边拖到下边以下：高度 = 最小高。
        let rect = session(s, (160, 220), ResizeEdge::Top).target_outer((160, 9_000), MIN);
        assert_eq!(
            rect.as_window_rect(),
            (100, 400 - MIN.1 as i32, 400, MIN.1 as i32)
        );
        // 下边拖到上边以上：bottom = top + 最小高。
        let rect = session(s, (160, 220), ResizeEdge::Bottom).target_outer((160, 0), MIN);
        assert_eq!(rect.as_window_rect(), (100, 100, 400, MIN.1 as i32));
        // 角：两个轴各自钳住。
        let rect = session(s, (160, 220), ResizeEdge::BottomRight).target_outer((0, 0), MIN);
        assert_eq!(
            rect.as_window_rect(),
            (100, 100, MIN.0 as i32, MIN.1 as i32),
            "右下角反向拖到左上：两轴都停在最小尺寸"
        );
    }

    /// 退化输入：最小外框为 0 时仍至少 1 px；极端坐标不溢出。
    #[test]
    fn degenerate_minimum_and_extreme_cursors_keep_a_positive_size() {
        let s = start();
        let rect = session(s, (0, 0), ResizeEdge::Right).target_outer((-9_000, 0), (0, 0));
        let (_, _, width, height) = rect.as_window_rect();
        assert_eq!(width, 1, "最小外框为 0 时宽度至少 1 px");
        assert_eq!(height, 300, "未拖动的轴保持起始尺寸");
        let rect = session(s, (0, 0), ResizeEdge::BottomLeft)
            .target_outer((i32::MIN, i32::MAX), (u32::MAX, u32::MAX));
        let (_, _, width, height) = rect.as_window_rect();
        assert!(
            width > 0 && height > 0,
            "极端坐标下尺寸仍为正：{width}x{height}"
        );
    }

    /// 目标外框来回转换 `SetWindowPos` 参数不丢信息（泵直接用 `as_window_rect`）。
    #[test]
    fn outer_rect_round_trips_to_set_window_pos_arguments() {
        let rect = OuterRect::from_ltrb(-40, 12, 960, 720);
        assert_eq!(rect.as_window_rect(), (-40, 12, 1000, 708));
    }
}
