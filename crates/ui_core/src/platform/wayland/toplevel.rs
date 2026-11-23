use crate::math::Size;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecorationMode {
    ClientSide,
    ServerSide,
}

impl DecorationMode {
    #[must_use]
    pub(crate) fn from_protocol(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::ClientSide),
            2 => Some(Self::ServerSide),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn draws_client_decoration(self) -> bool {
        matches!(self, Self::ClientSide)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToplevelStates {
    pub maximized: bool,
    pub fullscreen: bool,
    pub suspended: bool,
}

impl ToplevelStates {
    #[must_use]
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        let mut states = Self::default();
        for chunk in bytes.as_chunks::<4>().0 {
            match u32::from_ne_bytes(*chunk) {
                1 => states.maximized = true,
                2 => states.fullscreen = true,
                9 => states.suspended = true,
                _ => {}
            }
        }
        states
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Visibility {
    pub configured: bool,
    pub suspended: bool,
    pub entered_once: bool,
    pub outputs: usize,
}

#[must_use]
pub(crate) fn is_visible(visibility: Visibility) -> bool {
    if !visibility.configured || visibility.suspended {
        return false;
    }
    visibility.outputs > 0 || !visibility.entered_once
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

fn shadow_margin(shadow: f32) -> i32 {
    if shadow.is_finite() {
        shadow.max(0.0).round() as i32
    } else {
        0
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SizeLimits {
    pub min: (i32, i32),
    pub max: Option<(i32, i32)>,
}

impl SizeLimits {
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

pub(crate) struct SizeConstraintState {
    min: Size,
    max: Option<Size>,
    bounds: Option<Size>,
    applied: Option<SizeLimits>,
}

impl SizeConstraintState {
    pub(crate) fn new(min: Size, max: Option<Size>) -> Self {
        Self {
            min,
            max,
            bounds: None,
            applied: None,
        }
    }

    pub(crate) fn set_bounds(&mut self, bounds: Option<Size>) {
        self.bounds = bounds;
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WmCapabilities {
    pub maximize: bool,
    pub fullscreen: bool,
    pub minimize: bool,
}

impl WmCapabilities {
    pub(crate) const ASSUMED: Self = Self {
        maximize: true,
        fullscreen: true,
        minimize: true,
    };

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

        assert_eq!(
            ToplevelStates::parse(&states(&[3, 4, 5, 6, 7, 8])),
            ToplevelStates::default()
        );
    }

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

    #[test]
    fn window_geometry_without_shadow_covers_the_whole_surface() {
        let geometry = window_geometry(Size::new(800.0, 600.0), 0.0);
        assert_eq!((geometry.x, geometry.y), (0, 0));
        assert_eq!((geometry.width, geometry.height), (800, 600));
    }

    #[test]
    fn window_geometry_subtracts_the_shadow_margin_on_every_side() {
        let geometry = window_geometry(Size::new(800.0, 600.0), 8.0);
        assert_eq!((geometry.x, geometry.y), (8, 8));
        assert_eq!((geometry.width, geometry.height), (784, 584));
    }

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

    #[test]
    fn configure_bounds_are_intersected_with_the_app_maximum() {
        let mut state =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1000.0, 800.0)));
        let initial = state.take_pending(0.0).expect("建窗时要下发一次约束");
        assert_eq!(initial.max, Some((1000, 800)));

        state.set_bounds(Some(Size::new(900.0, 900.0)));
        assert_eq!(
            state.take_pending(0.0).expect("bounds 变化必须重新下发"),
            SizeLimits {
                min: (320, 200),
                max: Some((900, 800)),
            }
        );
        assert_eq!(state.take_pending(0.0), None, "同值 bounds 不重复下发");

        state.set_bounds(Some(Size::new(1600.0, 1200.0)));
        assert_eq!(
            state
                .take_pending(0.0)
                .expect("bounds 变化必须重新下发")
                .max,
            Some((1000, 800))
        );
    }

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

    #[test]
    fn the_platform_default_minimum_is_used_when_the_app_does_not_set_one() {
        let mut state =
            SizeConstraintState::new(Size::new(320.0, 200.0), Some(Size::new(1024.0, 768.0)));
        let limits = state.take_pending(0.0).expect("建窗下发");
        assert_eq!(limits.min, (320, 200), "平台默认最小客户区");
        assert_eq!(limits.max, Some((1024, 768)));
    }

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
