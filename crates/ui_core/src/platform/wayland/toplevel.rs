//! `xdg_toplevel` / `xdg_surface` 的协议无关状态机：装饰模式（R10-4）、可见性（R10-7）、
//! window geometry（R10-10）、尺寸约束（`set_min_size` / `set_max_size` + `configure_bounds`，
//! B3）、configure 状态位与 `wm_capabilities` / `configure_bounds` 的解析（R10-15）。
//!
//! 这些判据的共同点是「输入是协议里的整数数组或布尔状态，输出是一个决定」：
//!
//! - **装饰模式**：`zxdg_toplevel_decoration_v1.configure` 的 `mode` 原文要求 *"The specified
//!   mode must be obeyed by the client."* —— compositor 说 `server_side` 时客户端必须停止
//!   自绘标题栏，否则出现**双标题栏**；
//! - **可见性**：`wl_surface.frame` 原文允许服务端在 surface 不可见时**不**回 callback，
//!   因此「不可见」必须由客户端自己判定并停止请求帧（否则 200 ms 活性兜底退化成永久 5 Hz）；
//! - **window geometry**：`xdg_surface.set_window_geometry` 定义「哪个矩形是窗口」，compositor
//!   用它做放置/动画/阴影，不设置时等于整个 surface（含 CSD 装饰与将来的阴影边距）；
//! - **尺寸约束**：应用 `min_size/max_size` 是客户区逻辑尺寸，`configure_bounds` 是 compositor
//!   建议的 window geometry 上限；两者逐维取最小后加阴影边距才是 `set_min_size` /
//!   `set_max_size` 的参数（[`SizeConstraintState`]）。

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

/// `xdg_toplevel.configure` 的 `states` 数组里引擎**实际消费**的三个位。
///
/// 注意协议里这些是**枚举值列表**（maximized=1 … suspended=9），不是位掩码：同一个数组里
/// 每个 u32 表示一个状态存在。
///
/// 只解析被消费的状态（AGENTS §4：不再使用的字段直接删除）。`resizing` / `activated` /
/// `tiled_*` 目前没有消费者：resize 期重建的抑制属于 `render/linux/**`（R10-19，wave-4 写域），
/// 平铺态的圆角/阴影也还没有实现；等有消费者时再加回来，而不是先存着不用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToplevelStates {
    /// 最大化（自绘标题栏的切换按钮据此选择 set/unset）。
    pub maximized: bool,
    /// 全屏（`set_fullscreen` / `unset_fullscreen` 的判据）。
    pub fullscreen: bool,
    /// 不可见：compositor 明确要求停止绘制（xdg-shell v6）。
    pub suspended: bool,
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
                9 => states.suspended = true,
                // 3=resizing, 4=activated, 5..=8=tiled_*：当前没有消费者，见类型文档。
                // 其余是未来版本的枚举值，同样忽略。
                _ => {}
            }
        }
        states
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

/// CSD 阴影 / 边框在 window geometry **之外**占的整数逻辑像素（每边）。
///
/// 非有限值按 0 处理（`f32::NAN.round() as i32` 是 0，但显式分支让调用方意图可读）。
fn shadow_margin(shadow: f32) -> i32 {
    if shadow.is_finite() {
        shadow.max(0.0).round() as i32
    } else {
        0
    }
}

/// 窗口几何：逻辑尺寸扣除 `shadow` 边距（每边），并夹到至少 1×1。
///
/// `shadow` 是 CSD 阴影/边框在 window geometry **之外**占的宽度（本仓目前不画阴影 ⇒ 0，
/// 此时 window geometry 就是整个 surface，与「未设置」等价但语义明确）。将来加投影时只需把
/// 边距传进来：阴影落在 geometry 之外，最大化时不会被裁、平铺时不留白（R10-10 的诉求）。
#[must_use]
pub(crate) fn window_geometry(size: Size, shadow: f32) -> WindowGeometry {
    let margin = shadow_margin(shadow);
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

/// `xdg_toplevel.set_min_size` / `set_max_size` 的最终参数（window geometry 坐标）。
///
/// 两个参数都是 **window geometry 坐标**（= 客户区 + 每边阴影边距），单位整数逻辑像素。
/// `max == None` 表示没有上限：调用方用协议的 `set_max_size(0, 0)` 表达「无约束」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SizeLimits {
    /// `set_min_size` 参数（每维 ≥1）。
    pub min: (i32, i32),
    /// `set_max_size` 参数；`None` = 无上限。
    pub max: Option<(i32, i32)>,
}

impl SizeLimits {
    /// 客户区逻辑尺寸 + 阴影边距 → 协议参数。
    ///
    /// - 阴影边距对 min / max 一视同仁：协议约束的是 window geometry，而应用表达的是客户区
    ///   （自绘标题栏 + 内容）；不加边距时窗口能被缩到内容不可用。
    /// - 非有限 / 退化尺寸夹到至少 1×1（`set_min_size` 的 0 表示「无约束」，不能把非法输入
    ///   原样下发）。
    /// - `min > max` 的**维度**钳成 `min = max`（上限优先）：与 `WindowOptions` 文档的
    ///   「min>max 先钳成 min=max」一致；上限优先是因为 `configure_bounds` 是合成器的硬性
    ///   工作区提示、应用 `max_size` 是显式契约，而最小尺寸含引擎默认值（不该突破前者）。
    fn from_logical(min: Size, max: Option<Size>, shadow: f32) -> Self {
        let margin = shadow_margin(shadow).saturating_mul(2);
        let units = |value: f32| (value.round() as i32).saturating_add(margin).max(1);
        let mut min_width = units(min.width);
        let mut min_height = units(min.height);
        let max = max.map(|max| {
            let max_width = units(max.width);
            let max_height = units(max.height);
            min_width = min_width.min(max_width);
            min_height = min_height.min(max_height);
            (max_width, max_height)
        });
        Self {
            min: (min_width, min_height),
            max,
        }
    }
}

/// 一个窗口的尺寸约束状态：应用 [`crate::window::WindowOptions::min_size`] /
/// [`crate::window::WindowOptions::max_size`] + compositor 的 `xdg_toplevel.configure_bounds`
/// → 下发给 `xdg_toplevel` 的 [`SizeLimits`]。
///
/// **为什么需要状态机**（而不是建窗时算一次）：`configure_bounds`（xdg-shell v4）在窗口生命
/// 周期内会变化（面板 / 工作区改变），它是**建议上限**，与应用 `max_size` 必须**逐维取最小**
/// （审计 §3：旧实现直接拿 bounds 覆盖 `set_max_size`，应用显式上限会被静默放大；bounds=0
/// 表示「无建议上限」，应回到应用上限 / 不设）。同值不重复下发（与 window geometry 同一纪律）。
pub(crate) struct SizeConstraintState {
    /// 应用客户区最小逻辑尺寸（已解析平台默认，见 `platform/linux/window.rs` 的
    /// `MIN_SIZE_LOGICAL`）。
    min: Size,
    /// 应用客户区最大逻辑尺寸（`None` = 无应用上限）。
    max: Option<Size>,
    /// compositor 最近一次 `configure_bounds`（`None` = 未收到 / 0×0 = 无建议上限）。
    bounds: Option<Size>,
    /// 最近一次**已下发**的协议参数（`None` = 还没下发过）。
    applied: Option<SizeLimits>,
}

impl SizeConstraintState {
    /// 建窗：`min` 是已解析的应用 / 平台默认最小客户区尺寸。
    pub(crate) fn new(min: Size, max: Option<Size>) -> Self {
        Self {
            min,
            max,
            bounds: None,
            applied: None,
        }
    }

    /// `xdg_toplevel.configure_bounds`：更新 compositor 建议上限。
    ///
    /// 是否真的变化由 [`Self::take_pending`] 的同值比较负责，这里无条件写入（调用方随后调用
    /// `take_pending`，无变化时不会发请求）。
    pub(crate) fn set_bounds(&mut self, bounds: Option<Size>) {
        self.bounds = bounds;
    }

    /// 取「还没下发」的最新限制；`None` = 与上次下发相同（同值 no-op）。
    ///
    /// `max` 的有效值 = 应用 `max_size` 与 `configure_bounds` 的**逐维最小值**（任一为
    /// `None` 则取另一个；都为 `None` 则无上限）。
    pub(crate) fn take_pending(&mut self, shadow: f32) -> Option<SizeLimits> {
        let effective_max = match (self.max, self.bounds) {
            (Some(app), Some(bounds)) => Some(Size::new(
                app.width.min(bounds.width),
                app.height.min(bounds.height),
            )),
            (Some(app), None) => Some(app),
            (None, Some(bounds)) => Some(bounds),
            (None, None) => None,
        };
        let limits = SizeLimits::from_logical(self.min, effective_max, shadow);
        if self.applied == Some(limits) {
            return None;
        }
        self.applied = Some(limits);
        Some(limits)
    }
}

/// `xdg_toplevel.wm_capabilities`（xdg-shell v5）解析结果 —— 只保留**有消费者**的三项。
///
/// 协议：*"If a capability isn't supported, clients should hide or disable the UI elements that
/// expose this functionality."* 事件体是 native-endian u32 数组（枚举值列表，不是位掩码）。
///
/// `window_menu` 不在其中：本仓没有实现 `show_window_menu`（`WindowControl` 没有该请求），
/// 存了也没有消费者（AGENTS §4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WmCapabilities {
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
        maximize: true,
        fullscreen: true,
        minimize: true,
    };

    /// 解析协议数组（未知值与不足 4 字节的尾巴被忽略）。
    #[must_use]
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        let mut capabilities = Self {
            maximize: false,
            fullscreen: false,
            minimize: false,
        };
        for chunk in bytes.as_chunks::<4>().0 {
            match u32::from_ne_bytes(*chunk) {
                2 => capabilities.maximize = true,
                3 => capabilities.fullscreen = true,
                4 => capabilities.minimize = true,
                // 1 = window_menu：本仓不实现 show_window_menu。
                _ => {}
            }
        }
        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DecorationMode, SizeConstraintState, SizeLimits, ToplevelStates, Visibility,
        WmCapabilities, is_visible, window_geometry,
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
        assert!(parsed.maximized);
        assert!(!parsed.fullscreen && !parsed.suspended);

        let parsed = ToplevelStates::parse(&states(&[2, 9]));
        assert!(parsed.fullscreen);
        assert!(parsed.suspended, "xdg-shell v6 的 Suspended");

        // 没有消费者的状态（resizing / activated / tiled_*）被忽略，不影响已知状态。
        assert_eq!(
            ToplevelStates::parse(&states(&[3, 4, 5, 6, 7, 8])),
            ToplevelStates::default()
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

    /// `set_min_size` / `set_max_size` 的坐标是 window geometry，必须把阴影边距加回去，
    /// 否则客户区被压扁；退化尺寸夹到至少 1×1（协议里 0 表示「无约束」，不能原样下发）。
    #[test]
    fn size_limits_include_the_shadow_margin_on_both_bounds() {
        let mut state =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1280.0, 720.0)));
        assert_eq!(
            state.take_pending(0.0),
            Some(SizeLimits {
                min: (320, 200),
                max: Some((1280, 720)),
            })
        );
        assert_eq!(state.take_pending(0.0), None, "同值不得重复下发");

        let mut with_shadow =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1280.0, 720.0)));
        assert_eq!(
            with_shadow.take_pending(8.0),
            Some(SizeLimits {
                min: (336, 216),
                max: Some((1296, 736)),
            }),
            "阴影边距对 min 与 max 一视同仁（协议约束的是 window geometry）"
        );

        let mut degenerate = SizeConstraintState::new(Size::new(0.0, -5.0), None);
        assert_eq!(
            degenerate.take_pending(0.0),
            Some(SizeLimits {
                min: (1, 1),
                max: None,
            }),
            "0 / 负数不是合法的 set_min_size 参数（等价于「无约束」），必须夹到 1"
        );
    }

    /// B3 核心：compositor 的 `configure_bounds` 是建议上限，与应用 `max_size` **逐维取最小**。
    ///
    /// 旧实现（当前分支修复前）在 `configure_bounds` 里直接 `set_max_size(bounds)`，会静默放大
    /// 应用显式设置的上限（`WindowOptions::max_size` 是契约）。
    ///
    /// 变异检查：让 `take_pending` 忽略 bounds（恒取应用 max）⇒ 本测试红。
    #[test]
    fn configure_bounds_are_intersected_with_the_app_maximum() {
        let mut state =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1000.0, 800.0)));
        let initial = state.take_pending(0.0).expect("建窗时要下发一次约束");
        assert_eq!(initial.max, Some((1000, 800)));

        // bounds 比应用上限小的一维受 bounds 约束，另一维仍取应用上限。
        state.set_bounds(Some(Size::new(900.0, 900.0)));
        assert_eq!(
            state.take_pending(0.0).expect("bounds 变化必须重新下发"),
            SizeLimits {
                min: (320, 200),
                max: Some((900, 800)),
            }
        );
        assert_eq!(state.take_pending(0.0), None, "同值 bounds 不重复下发");

        // bounds 变大：应用上限重新生效（逐维最小）。
        state.set_bounds(Some(Size::new(1600.0, 1200.0)));
        assert_eq!(
            state
                .take_pending(0.0)
                .expect("bounds 变化必须重新下发")
                .max,
            Some((1000, 800))
        );
    }

    /// 应用没设 max 时，`configure_bounds` 就是唯一上限（不能忽略）。
    #[test]
    fn configure_bounds_without_an_app_maximum_become_the_limit() {
        let mut state = SizeConstraintState::new(Size::new(320.0, 200.0), None);
        assert_eq!(state.take_pending(0.0).expect("建窗下发").max, None);

        state.set_bounds(Some(Size::new(1280.0, 720.0)));
        assert_eq!(
            state.take_pending(0.0).expect("bounds 必须成为上限").max,
            Some((1280, 720))
        );
    }

    /// bounds 撤销（`configure_bounds(0, 0)`）⇒ 回到应用上限；应用也没有上限 ⇒ 返回「无上限」，
    /// 调用方据此发 `set_max_size(0, 0)` 解除之前设置的 bounds。
    #[test]
    fn removing_the_bounds_restores_or_clears_the_maximum() {
        let mut with_app_max =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1000.0, 800.0)));
        assert!(with_app_max.take_pending(0.0).is_some());
        with_app_max.set_bounds(Some(Size::new(640.0, 480.0)));
        assert_eq!(
            with_app_max.take_pending(0.0).expect("bounds 生效").max,
            Some((640, 480))
        );
        with_app_max.set_bounds(None);
        assert_eq!(
            with_app_max
                .take_pending(0.0)
                .expect("bounds 撤销后必须恢复应用上限")
                .max,
            Some((1000, 800))
        );

        let mut without_app_max = SizeConstraintState::new(Size::new(320.0, 200.0), None);
        assert!(without_app_max.take_pending(0.0).is_some());
        without_app_max.set_bounds(Some(Size::new(640.0, 480.0)));
        assert!(without_app_max.take_pending(0.0).is_some());
        without_app_max.set_bounds(None);
        assert_eq!(
            without_app_max
                .take_pending(0.0)
                .expect("撤销 bounds 必须重新下发（清除 max）")
                .max,
            None,
            "无应用上限 ⇒ 调用方发 set_max_size(0, 0) 解除之前的 bounds"
        );
    }

    /// 公共面契约：`min > max` 时先钳成 `min = max`（逐维、上限优先）。这里覆盖「引擎默认
    /// 最小尺寸（320×200）大于应用 max」的典型场景。
    #[test]
    fn a_contradictory_minimum_is_clamped_to_the_maximum() {
        let mut state =
            SizeConstraintState::new(Size::new(800.0, 600.0), Some(Size::new(400.0, 300.0)));
        assert_eq!(
            state.take_pending(0.0),
            Some(SizeLimits {
                min: (400, 300),
                max: Some((400, 300)),
            }),
            "min > max 先钳成 min = max（与 WindowOptions 公共文档一致）"
        );

        // 只有一维冲突：该维钳成相等，另一维保持原值。
        let mut mixed =
            SizeConstraintState::new(Size::new(800.0, 600.0), Some(Size::new(1000.0, 500.0)));
        assert_eq!(
            mixed.take_pending(0.0),
            Some(SizeLimits {
                min: (800, 500),
                max: Some((1000, 500)),
            })
        );
    }

    /// 默认最小尺寸 + 应用 max：没有冲突时两者都下发（Linux 侧 `None` 的 min 用平台默认）。
    #[test]
    fn the_platform_default_minimum_is_used_when_the_app_does_not_set_one() {
        let mut state =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1024.0, 768.0)));
        let limits = state.take_pending(0.0).expect("建窗下发");
        assert_eq!(limits.min, (320, 200), "平台默认最小客户区");
        assert_eq!(limits.max, Some((1024, 768)));
    }

    /// R10-15：`wm_capabilities` 是枚举值列表（不是位掩码），未收到时假定全部支持。
    #[test]
    fn wm_capabilities_are_parsed_as_a_value_list() {
        let none = WmCapabilities {
            maximize: false,
            fullscreen: false,
            minimize: false,
        };
        assert_eq!(WmCapabilities::parse(&[]), none);
        let parsed = WmCapabilities::parse(&states(&[2, 3]));
        assert!(parsed.maximize && parsed.fullscreen);
        assert!(!parsed.minimize);
        assert_eq!(
            WmCapabilities::parse(&states(&[1, 2, 3, 4])),
            WmCapabilities::ASSUMED,
            "1 是 window_menu（本仓不消费），其余三项与默认值一致"
        );
        assert_eq!(WmCapabilities::parse(&states(&[9])), none, "未知能力值忽略");
    }
}
