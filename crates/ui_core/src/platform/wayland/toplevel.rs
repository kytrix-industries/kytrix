//! `xdg_toplevel` / `xdg_surface` 的协议无关状态机：装饰模式（R10-4）、可见性（R10-7）、
//! window geometry（R10-10）、configure 状态位与 `wm_capabilities` / `configure_bounds`
//! 的解析（R10-15）。
//!
//! 这些判据的共同点是「输入是协议里的整数数组或布尔状态，输出是一个决定」：
//!
//! - **装饰模式**：`zxdg_toplevel_decoration_v1.configure` 的 `mode` 原文要求 *"The specified
//!   mode must be obeyed by the client."* —— compositor 说 `server_side` 时客户端必须停止
//!   自绘标题栏，否则出现**双标题栏**；
//! - **可见性**：`wl_surface.frame` 原文允许服务端在 surface 不可见时**不**回 callback，
//!   因此「不可见」必须由客户端自己判定并停止请求帧（否则 200 ms 活性兜底退化成永久 5 Hz）；
//! - **window geometry**：`xdg_surface.set_window_geometry` 定义「哪个矩形是窗口」，compositor
//!   用它做放置/动画/阴影，不设置时等于整个 surface（含 CSD 装饰与将来的阴影边距）。

use crate::math::Size;

/// `zxdg_toplevel_decoration_v1.configure` 的有效装饰模式（协议值 1 / 2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecorationMode {
    /// 客户端自绘标题栏（引擎的默认，协议值 1）。
    ClientSide,
    /// compositor 画标题栏（协议值 2）：客户端**必须**停止自绘，否则双标题栏。
    ServerSide,
}

impl DecorationMode {
    /// 协议值 → 模式（未知值 = 未来新增的枚举，按「没有信息」处理）。
    #[must_use]
    pub(crate) fn from_protocol(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::ClientSide),
            2 => Some(Self::ServerSide),
            _ => None,
        }
    }

    /// 是否需要引擎自绘标题栏。
    #[must_use]
    pub(crate) fn draws_client_decoration(self) -> bool {
        matches!(self, Self::ClientSide)
    }
}

/// `xdg_toplevel.configure` 的 `states` 数组里引擎关心的位。
///
/// 注意协议里这些是**枚举值列表**（maximized=1 … suspended=9），不是位掩码：同一个数组里
/// 每个 u32 表示一个状态存在。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToplevelStates {
    /// 最大化。
    pub maximized: bool,
    /// 全屏。
    pub fullscreen: bool,
    /// 正在被交互式 resize（可用于抑制 resize 期的重建，R10-19）。
    pub resizing: bool,
    /// 已激活（键盘焦点由 compositor 视角给出，可作为 a11y 的焦点信号）。
    pub activated: bool,
    /// 不可见：compositor 明确要求停止绘制（xdg-shell v6）。
    pub suspended: bool,
    /// 左侧被平铺。
    pub tiled_left: bool,
    /// 右侧被平铺。
    pub tiled_right: bool,
}

impl ToplevelStates {
    /// 解析协议数组（native-endian 的 u32 序列；未知值与不足 4 字节的尾巴被忽略）。
    #[must_use]
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        let mut states = Self::default();
        for chunk in bytes.as_chunks::<4>().0 {
            match u32::from_ne_bytes(*chunk) {
                1 => states.maximized = true,
                2 => states.fullscreen = true,
                3 => states.resizing = true,
                4 => states.activated = true,
                5 => states.tiled_left = true,
                6 => states.tiled_right = true,
                9 => states.suspended = true,
                // 7/8 是 tiled_top/bottom，与 left/right 合并成「被平铺」即可；
                // 其余是未来版本的枚举值，忽略。
                7 | 8 => {}
                _ => {}
            }
        }
        states
    }

    /// 是否被平铺（任意方向）。平铺状态下不应自绘圆角/阴影（compositor 会把窗口贴边）。
    #[must_use]
    pub(crate) fn tiled(self) -> bool {
        self.tiled_left || self.tiled_right
    }
}

/// 可见性判据的输入（全部来自协议状态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Visibility {
    /// 已收到首个 `xdg_toplevel.configure`。
    pub configured: bool,
    /// 最近一次 configure 的 `Suspended` 状态。
    pub suspended: bool,
    /// 是否收到过至少一个 `wl_surface.enter`（一旦为真就保持）。
    pub entered_once: bool,
    /// 当前 `wl_surface.enter` 过、还没 `leave` 的 output 数量。
    pub outputs: usize,
}

/// 该 surface 现在是否应当驱动帧（R10-7）。
///
/// 判据（`wl_surface.enter`/`leave` 是**权威**的可见性信号，`Suspended` 是 compositor 的显式
/// 通知）：
///
/// - 未 configure ⇒ 不可见（还没有 swapchain，也没有可画的东西）；
/// - `Suspended` ⇒ 不可见（最小化 / 切 workspace）；
/// - **收到过 enter 之后** entered 列表为空 ⇒ 不在任何 output 上 ⇒ 不可见；
/// - 首帧之前（`entered_once == false`，enter 要等首次 attach/commit 才到）⇒ 视为可见，
///   否则首帧永远不会被绘制（鸡生蛋）。
#[must_use]
pub(crate) fn is_visible(visibility: Visibility) -> bool {
    if !visibility.configured || visibility.suspended {
        return false;
    }
    visibility.outputs > 0 || !visibility.entered_once
}

/// 一个整数逻辑像素矩形（`xdg_surface.set_window_geometry` 的参数）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowGeometry {
    /// 左上角 x（surface-local 逻辑像素）。
    pub x: i32,
    /// 左上角 y。
    pub y: i32,
    /// 宽（≥1）。
    pub width: i32,
    /// 高（≥1）。
    pub height: i32,
}

/// 窗口几何：逻辑尺寸扣除 `shadow` 边距（每边），并夹到至少 1×1。
///
/// `shadow` 是 CSD 阴影/边框在 window geometry **之外**占的宽度（本仓目前不画阴影 ⇒ 0，
/// 此时 window geometry 就是整个 surface，与「未设置」等价但语义明确）。将来加投影时只需把
/// 边距传进来：阴影落在 geometry 之外，最大化时不会被裁、平铺时不留白（R10-10 的诉求）。
#[must_use]
pub(crate) fn window_geometry(size: Size, shadow: f32) -> WindowGeometry {
    let margin = if shadow.is_finite() {
        shadow.max(0.0).round() as i32
    } else {
        0
    };
    let width = (size.width.round() as i32)
        .saturating_sub(margin * 2)
        .max(1);
    let height = (size.height.round() as i32)
        .saturating_sub(margin * 2)
        .max(1);
    WindowGeometry {
        x: margin,
        y: margin,
        width,
        height,
    }
}

/// `xdg_toplevel.set_min_size` 的参数：最小**客户区** + 阴影边距（window geometry 坐标）。
///
/// 不加上边距时 `set_min_size(320, 200)` 约束的是 window geometry，而 client 侧真正想保住的
/// 是客户区（自绘标题栏 + 内容）：加阴影后必须把边距补回去，否则窗口能被缩到内容不可用。
#[must_use]
pub(crate) fn min_size_with_shadow(minimum: (f32, f32), shadow: f32) -> (i32, i32) {
    let margin = if shadow.is_finite() {
        (shadow.max(0.0).round() as i32) * 2
    } else {
        0
    };
    (
        (minimum.0.round() as i32).saturating_add(margin).max(1),
        (minimum.1.round() as i32).saturating_add(margin).max(1),
    )
}

/// `xdg_toplevel.wm_capabilities`（xdg-shell v5）解析结果。
///
/// 协议：*"If a capability isn't supported, clients should hide or disable the UI elements that
/// expose this functionality."* 事件体是 native-endian u32 数组（枚举值列表，不是位掩码）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WmCapabilities {
    /// `show_window_menu` 可用。
    pub window_menu: bool,
    /// `set_maximized` / `unset_maximized` 可用。
    pub maximize: bool,
    /// `set_fullscreen` / `unset_fullscreen` 可用。
    pub fullscreen: bool,
    /// `set_minimized` 可用。
    pub minimize: bool,
}

impl WmCapabilities {
    /// 未收到事件时的默认值：**全部支持**。
    ///
    /// `wm_capabilities` 只在 xdg-shell v5+ 存在；绑定到 v6 但与老 compositor 通信时永远收不到。
    /// 那时按「全部支持」处理与旧行为一致（请求被忽略，不会出错），不会凭空禁用按钮。
    pub(crate) const ASSUMED: Self = Self {
        window_menu: true,
        maximize: true,
        fullscreen: true,
        minimize: true,
    };

    /// 解析协议数组（未知值与不足 4 字节的尾巴被忽略）。
    #[must_use]
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        let mut capabilities = Self {
            window_menu: false,
            maximize: false,
            fullscreen: false,
            minimize: false,
        };
        for chunk in bytes.as_chunks::<4>().0 {
            match u32::from_ne_bytes(*chunk) {
                1 => capabilities.window_menu = true,
                2 => capabilities.maximize = true,
                3 => capabilities.fullscreen = true,
                4 => capabilities.minimize = true,
                _ => {}
            }
        }
        capabilities
    }
}

/// 首帧尺寸：`xdg_toplevel.set_default_size` 的参数（R10-15）。
///
/// `0×0` 表示「让 compositor 决定」，本仓总是给出应用的请求尺寸（取整、至少 1 px）：
/// 平铺/居中类 compositor 会优先用 `default_size` 决定首帧，比「只依赖 configure」更可控。
#[must_use]
pub(crate) fn default_size(size: Size) -> (i32, i32) {
    (
        size.width.round().max(1.0) as i32,
        size.height.round().max(1.0) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DecorationMode, ToplevelStates, Visibility, WmCapabilities, default_size, is_visible,
        min_size_with_shadow, window_geometry,
    };
    use crate::math::Size;

    fn states(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect()
    }

    /// R10-4：compositor 说的 `server_side` 必须被遵从（否则双标题栏）。
    #[test]
    fn decoration_modes_map_from_the_protocol_values() {
        assert_eq!(
            DecorationMode::from_protocol(1),
            Some(DecorationMode::ClientSide)
        );
        assert_eq!(
            DecorationMode::from_protocol(2),
            Some(DecorationMode::ServerSide)
        );
        assert_eq!(DecorationMode::from_protocol(0), None);
        assert_eq!(DecorationMode::from_protocol(3), None, "未来新增的枚举值");
        assert!(DecorationMode::ClientSide.draws_client_decoration());
        assert!(
            !DecorationMode::ServerSide.draws_client_decoration(),
            "SSD 时引擎不得再画标题栏"
        );
    }

    #[test]
    fn toplevel_states_are_parsed_from_the_protocol_array() {
        assert_eq!(ToplevelStates::parse(&[]), ToplevelStates::default());
        let parsed = ToplevelStates::parse(&states(&[1, 4]));
        assert!(parsed.maximized && parsed.activated);
        assert!(!parsed.fullscreen && !parsed.suspended);

        let parsed = ToplevelStates::parse(&states(&[2, 9]));
        assert!(parsed.fullscreen);
        assert!(parsed.suspended, "xdg-shell v6 的 Suspended");

        let parsed = ToplevelStates::parse(&states(&[5]));
        assert!(parsed.tiled() && parsed.tiled_left);
        let parsed = ToplevelStates::parse(&states(&[6]));
        assert!(parsed.tiled() && parsed.tiled_right);
        assert!(
            !ToplevelStates::parse(&states(&[7])).tiled(),
            "上下平铺不算"
        );
    }

    /// 未知状态值 / 长度不是 4 的倍数的尾巴必须被忽略而不是 panic。
    #[test]
    fn unknown_and_truncated_state_entries_are_ignored() {
        let mut bytes = states(&[1]);
        bytes.extend_from_slice(&[0xff, 0xff, 0xff]);
        let parsed = ToplevelStates::parse(&bytes);
        assert!(parsed.maximized);
        assert_eq!(
            ToplevelStates::parse(&states(&[99, 0])),
            ToplevelStates::default(),
            "未知枚举值不得被当成已知状态"
        );
    }

    /// R10-7：不可见判定必须放过「首帧之前还没有 enter」这一鸡生蛋状态。
    #[test]
    fn visibility_requires_a_configure_and_an_output_after_the_first_enter() {
        let unconfigured = Visibility {
            configured: false,
            suspended: false,
            entered_once: false,
            outputs: 0,
        };
        assert!(!is_visible(unconfigured), "未 configure 没有 swapchain");

        let before_first_enter = Visibility {
            configured: true,
            suspended: false,
            entered_once: false,
            outputs: 0,
        };
        assert!(
            is_visible(before_first_enter),
            "首个 enter 要等首次 commit 才到：必须放行首帧"
        );

        let mapped = Visibility {
            configured: true,
            suspended: false,
            entered_once: true,
            outputs: 1,
        };
        assert!(is_visible(mapped));

        let left_all_outputs = Visibility {
            outputs: 0,
            ..mapped
        };
        assert!(
            !is_visible(left_all_outputs),
            "收到过 enter 之后列表为空 ⇒ 不可见（最小化 / 切 workspace）"
        );

        let suspended = Visibility {
            suspended: true,
            outputs: 1,
            ..mapped
        };
        assert!(
            !is_visible(suspended),
            "compositor 的 Suspended 是显式「停止绘制」"
        );
    }

    /// R10-10：无阴影时 window geometry 就是整个 surface（含 CSD 标题栏）。
    #[test]
    fn window_geometry_without_shadow_covers_the_whole_surface() {
        let geometry = window_geometry(Size::new(800.0, 600.0), 0.0);
        assert_eq!((geometry.x, geometry.y), (0, 0));
        assert_eq!((geometry.width, geometry.height), (800, 600));
    }

    /// 有阴影时 geometry 收缩到可见窗口（阴影落在外面，最大化不被裁、平铺不留白）。
    #[test]
    fn window_geometry_subtracts_the_shadow_margin_on_every_side() {
        let geometry = window_geometry(Size::new(800.0, 600.0), 8.0);
        assert_eq!((geometry.x, geometry.y), (8, 8));
        assert_eq!((geometry.width, geometry.height), (784, 584));
    }

    /// 退化尺寸：收缩后仍然至少 1×1（`set_window_geometry` 的 0 宽高是非法输入）。
    #[test]
    fn window_geometry_never_collapses_to_zero() {
        let geometry = window_geometry(Size::new(4.0, 4.0), 8.0);
        assert_eq!((geometry.width, geometry.height), (1, 1));
        let geometry = window_geometry(Size::new(f32::NAN, f32::INFINITY), 2.0);
        assert!(geometry.width >= 1 && geometry.height >= 1);
        assert_eq!((geometry.x, geometry.y), (2, 2));
        let geometry = window_geometry(Size::new(100.0, 100.0), f32::NAN);
        assert_eq!((geometry.x, geometry.y), (0, 0), "非法边距按 0 处理");
    }

    /// `set_min_size` 的坐标是 window geometry，必须把阴影边距加回去，否则客户区被压扁。
    #[test]
    fn min_size_includes_the_shadow_margin() {
        assert_eq!(min_size_with_shadow((320.0, 200.0), 0.0), (320, 200));
        assert_eq!(min_size_with_shadow((320.0, 200.0), 8.0), (336, 216));
        assert_eq!(
            min_size_with_shadow((0.0, 0.0), 0.0),
            (1, 1),
            "0 不是合法的 set_min_size 参数（等价于「无约束」）"
        );
    }

    /// R10-15：`wm_capabilities` 是枚举值列表（不是位掩码），未收到时假定全部支持。
    #[test]
    fn wm_capabilities_are_parsed_as_a_value_list() {
        assert_eq!(
            WmCapabilities::parse(&[]),
            WmCapabilities {
                window_menu: false,
                maximize: false,
                fullscreen: false,
                minimize: false
            }
        );
        let parsed = WmCapabilities::parse(&states(&[2, 3]));
        assert!(parsed.maximize && parsed.fullscreen);
        assert!(!parsed.minimize && !parsed.window_menu);
        assert_eq!(
            WmCapabilities::parse(&states(&[1, 2, 3, 4])),
            WmCapabilities::ASSUMED,
            "全部支持时与默认值一致"
        );
        assert_eq!(
            WmCapabilities::parse(&states(&[9])),
            WmCapabilities {
                window_menu: false,
                maximize: false,
                fullscreen: false,
                minimize: false
            },
            "未知能力值忽略"
        );
    }

    /// 首帧尺寸必须取整且 ≥1（0 会变成「让 compositor 决定」）。
    #[test]
    fn default_size_is_rounded_and_never_zero() {
        assert_eq!(default_size(Size::new(800.0, 600.0)), (800, 600));
        assert_eq!(default_size(Size::new(800.6, 600.4)), (801, 600));
        assert_eq!(default_size(Size::new(0.0, 0.0)), (1, 1));
        assert_eq!(default_size(Size::new(f32::NAN, -3.0)), (1, 1));
    }
}
