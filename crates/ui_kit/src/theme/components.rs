use std::time::Duration;

use ui_core::{
    math::{LogicalPx, px},
    paint::color::Color,
};

use super::{
    color::{black_with_alpha, over, with_alpha},
    roles::RoleColors,
};

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum InteractiveVariant {
    #[default]
    Filled,
    Tonal,
    Outline,
    Ghost,
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum InteractiveState {
    #[default]
    Normal,
    Hover,
    Active,
    Disabled,
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ControlSize {
    Small,
    #[default]
    Medium,
    Large,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct SizeTokens {
    pub height: LogicalPx,
    pub gap: LogicalPx,
    pub padding_x: LogicalPx,
    pub font_size: LogicalPx,
    pub icon_size: LogicalPx,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ShadowTokens {
    pub dialog_alpha: f32,
    pub menu_alpha: f32,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ScrollbarTokens {
    pub resting_alpha: f32,
    pub active_alpha: f32,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ComponentTokens {
    pub small: SizeTokens,
    pub medium: SizeTokens,
    pub large: SizeTokens,
    pub border_width: LogicalPx,
    pub animation_tick: Duration,
    pub shadow: ShadowTokens,
    pub scrollbar: ScrollbarTokens,
}

impl ComponentTokens {
    pub fn standard() -> Self {
        let size = |height, gap, padding_x, font_size, icon_size| SizeTokens {
            height: px(height),
            gap: px(gap),
            padding_x: px(padding_x),
            font_size: px(font_size),
            icon_size: px(icon_size),
        };

        Self {
            small: size(20.0, 4.0, 4.0, 12.0, 8.0),
            medium: size(30.0, 6.0, 6.0, 14.0, 12.0),
            large: size(40.0, 8.0, 10.0, 18.0, 16.0),
            border_width: px(1.0),
            animation_tick: Duration::from_millis(150),
            shadow: ShadowTokens {
                dialog_alpha: 0.25,
                menu_alpha: 0.12,
            },
            scrollbar: ScrollbarTokens {
                resting_alpha: 0.22,
                active_alpha: 0.40,
            },
        }
    }

    pub fn for_size(&self, size: ControlSize) -> SizeTokens {
        match size {
            ControlSize::Small => self.small,
            ControlSize::Medium => self.medium,
            ControlSize::Large => self.large,
        }
    }
}

impl Default for ComponentTokens {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ComponentColors {
    pub interactive: InteractiveVariantColors,
    pub input: InputColors,
    pub menu: MenuColors,
    pub dialog: DialogColors,
    pub toggle: ToggleStateColors,
    pub progress: ProgressColors,
    pub scrollbar: ScrollbarColors,
    pub tooltip: TooltipColors,
    pub chart: ChartColors,
}

impl ComponentColors {
    pub(crate) fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        Self {
            interactive: InteractiveVariantColors::resolve(roles),
            input: InputColors::resolve(roles),
            menu: MenuColors::resolve(roles, tokens),
            dialog: DialogColors::resolve(roles, tokens),
            toggle: ToggleStateColors::resolve(roles),
            progress: ProgressColors::resolve(roles),
            scrollbar: ScrollbarColors::resolve(roles, tokens),
            tooltip: TooltipColors::resolve(roles),
            chart: ChartColors::resolve(roles),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InteractiveVariantColors {
    pub filled: InteractiveStateColors,
    pub tonal: InteractiveStateColors,
    pub outline: InteractiveStateColors,
    pub ghost: InteractiveStateColors,
}

impl InteractiveVariantColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            filled: InteractiveStateColors::resolve(roles, InteractiveVariant::Filled),
            tonal: InteractiveStateColors::resolve(roles, InteractiveVariant::Tonal),
            outline: InteractiveStateColors::resolve(roles, InteractiveVariant::Outline),
            ghost: InteractiveStateColors::resolve(roles, InteractiveVariant::Ghost),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InteractiveStateColors {
    pub normal: InteractiveElementColors,
    pub hover: InteractiveElementColors,
    pub active: InteractiveElementColors,
    pub disabled: InteractiveElementColors,
}

impl InteractiveStateColors {
    fn resolve(roles: &RoleColors, variant: InteractiveVariant) -> Self {
        Self {
            normal: InteractiveElementColors::resolve(roles, variant, InteractiveState::Normal),
            hover: InteractiveElementColors::resolve(roles, variant, InteractiveState::Hover),
            active: InteractiveElementColors::resolve(roles, variant, InteractiveState::Active),
            disabled: InteractiveElementColors::resolve(roles, variant, InteractiveState::Disabled),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InteractiveElementColors {
    pub background: Color,
    pub foreground: Color,
    pub border: Color,
}

impl InteractiveElementColors {
    fn resolve(roles: &RoleColors, variant: InteractiveVariant, state: InteractiveState) -> Self {
        let colors = |background: Color, foreground: Color, border: Color| Self {
            background,
            foreground,
            border,
        };
        let transparent = Color::TRANSPARENT;
        let role = |role| roles.get(role);

        match variant {
            InteractiveVariant::Filled => match state {
                InteractiveState::Normal => colors(
                    role(super::role::Role::BrandBase),
                    role(super::role::Role::FgOnBrand),
                    role(super::role::Role::BrandBase),
                ),
                InteractiveState::Hover => colors(
                    role(super::role::Role::BrandHover),
                    role(super::role::Role::FgOnBrand),
                    role(super::role::Role::BrandHover),
                ),
                InteractiveState::Active => colors(
                    role(super::role::Role::BrandActive),
                    role(super::role::Role::FgOnBrand),
                    role(super::role::Role::BrandActive),
                ),
                InteractiveState::Disabled => colors(
                    role(super::role::Role::SurfaceControl),
                    role(super::role::Role::FgDisabled),
                    role(super::role::Role::SurfaceControl),
                ),
            },
            InteractiveVariant::Tonal => {
                let base = role(super::role::Role::SurfaceControl);

                match state {
                    InteractiveState::Normal => colors(
                        base,
                        role(super::role::Role::FgDefault),
                        role(super::role::Role::LineBorder),
                    ),
                    InteractiveState::Hover => colors(
                        over(base, role(super::role::Role::VeilHover)),
                        role(super::role::Role::FgDefault),
                        role(super::role::Role::LineStrong),
                    ),
                    InteractiveState::Active => colors(
                        over(base, role(super::role::Role::VeilActive)),
                        role(super::role::Role::FgDefault),
                        role(super::role::Role::LineStrong),
                    ),
                    InteractiveState::Disabled => colors(
                        base,
                        role(super::role::Role::FgDisabled),
                        role(super::role::Role::LineDivider),
                    ),
                }
            }
            InteractiveVariant::Outline => match state {
                InteractiveState::Normal => colors(
                    transparent,
                    role(super::role::Role::FgDefault),
                    role(super::role::Role::LineBorder),
                ),
                InteractiveState::Hover => colors(
                    role(super::role::Role::VeilHover),
                    role(super::role::Role::FgDefault),
                    role(super::role::Role::LineStrong),
                ),
                InteractiveState::Active => colors(
                    role(super::role::Role::VeilActive),
                    role(super::role::Role::FgDefault),
                    role(super::role::Role::LineStrong),
                ),
                InteractiveState::Disabled => colors(
                    transparent,
                    role(super::role::Role::FgDisabled),
                    role(super::role::Role::LineDivider),
                ),
            },
            InteractiveVariant::Ghost => match state {
                InteractiveState::Normal => {
                    colors(transparent, role(super::role::Role::FgDefault), transparent)
                }
                InteractiveState::Hover => colors(
                    role(super::role::Role::VeilHover),
                    role(super::role::Role::FgDefault),
                    transparent,
                ),
                InteractiveState::Active => colors(
                    role(super::role::Role::VeilActive),
                    role(super::role::Role::FgDefault),
                    transparent,
                ),
                InteractiveState::Disabled => colors(
                    transparent,
                    role(super::role::Role::FgDisabled),
                    transparent,
                ),
            },
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputColors {
    pub background: InputBackgroundColors,
    pub foreground: InputForegroundColors,
    pub border: InputBorderColors,
    pub placeholder: Color,
    pub selection: Color,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputBackgroundColors {
    pub enabled: Color,
    pub disabled: Color,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputForegroundColors {
    pub enabled: Color,
    pub disabled: Color,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputBorderColors {
    pub normal: Color,
    pub focused: Color,
    pub disabled: Color,
}

impl InputColors {
    fn resolve(roles: &RoleColors) -> Self {
        let role = |role| roles.get(role);

        Self {
            background: InputBackgroundColors {
                enabled: role(super::role::Role::SurfaceControl),
                disabled: role(super::role::Role::SurfaceSunken),
            },
            foreground: InputForegroundColors {
                enabled: role(super::role::Role::FgDefault),
                disabled: role(super::role::Role::FgDisabled),
            },
            border: InputBorderColors {
                normal: role(super::role::Role::LineBorder),
                focused: role(super::role::Role::BrandBase),
                disabled: role(super::role::Role::LineDivider),
            },
            placeholder: role(super::role::Role::FgSubtle),
            selection: role(super::role::Role::Selection),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct MenuColors {
    pub background: Color,
    pub group_foreground: Color,
    pub shadow: Color,
}

impl MenuColors {
    fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        Self {
            background: roles.get(super::role::Role::SurfaceOverlay),
            group_foreground: roles.get(super::role::Role::FgSubtle),
            shadow: black_with_alpha(tokens.shadow.menu_alpha),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct DialogColors {
    pub background: Color,
    pub backdrop: Color,
    pub shadow: Color,
}

impl DialogColors {
    fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        Self {
            background: roles.get(super::role::Role::SurfaceOverlay),
            backdrop: roles.get(super::role::Role::Scrim),
            shadow: black_with_alpha(tokens.shadow.dialog_alpha),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ToggleStateColors {
    pub enabled: ToggleColors,
    pub disabled: ToggleColors,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ToggleColors {
    pub checked: Color,
    pub unchecked: Color,
    pub foreground: Color,
}

impl ToggleStateColors {
    fn resolve(roles: &RoleColors) -> Self {
        let role = |role| roles.get(role);

        Self {
            enabled: ToggleColors {
                checked: role(super::role::Role::BrandBase),
                unchecked: role(super::role::Role::LineStrong),
                foreground: role(super::role::Role::FgDefault),
            },
            disabled: ToggleColors {
                checked: role(super::role::Role::FgDisabled),
                unchecked: role(super::role::Role::LineDivider),
                foreground: role(super::role::Role::FgDisabled),
            },
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ProgressColors {
    pub track: Color,
    pub progress: Color,
}

impl ProgressColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            track: roles.get(super::role::Role::SurfaceControl),
            progress: roles.get(super::role::Role::BrandBase),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ScrollbarColors {
    pub normal: Color,
    pub active: Color,
}

impl ScrollbarColors {
    fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        let thumb = |alpha: f32| with_alpha(roles.get(super::role::Role::FgDefault), alpha);

        Self {
            normal: thumb(tokens.scrollbar.resting_alpha),
            active: thumb(tokens.scrollbar.active_alpha),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct TooltipColors {
    pub background: Color,
    pub foreground: Color,
}

impl TooltipColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            background: roles.get(super::role::Role::SurfaceInverse),
            foreground: roles.get(super::role::Role::FgOnInverse),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ChartColors {
    pub grid: Color,
    pub axis: Color,
    pub crosshair: Color,
    pub bullish: Color,
    pub bearish: Color,
    pub series: [Color; 6],
}

impl ChartColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            grid: roles.get(super::role::Role::ChartGrid),
            axis: roles.get(super::role::Role::ChartAxis),
            crosshair: roles.get(super::role::Role::ChartCrosshair),
            bullish: roles.get(super::role::Role::MarketUp),
            bearish: roles.get(super::role::Role::MarketDown),
            series: [
                roles.get(super::role::Role::ChartSeries0),
                roles.get(super::role::Role::ChartSeries1),
                roles.get(super::role::Role::ChartSeries2),
                roles.get(super::role::Role::ChartSeries3),
                roles.get(super::role::Role::ChartSeries4),
                roles.get(super::role::Role::ChartSeries5),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use ui_core::math::px;

    use super::{ComponentTokens, ControlSize, InteractiveVariant};
    use crate::theme::{
        appearance::Appearance, color::contrast, palette::Palette, resolved::Theme, role::Role,
        roles::resolve_roles,
    };

    #[test]
    fn the_active_scrollbar_thumb_stands_out_more_than_the_resting_one() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let colors = theme.components();
            let base = theme.role(Role::SurfaceCanvas);
            let resting = contrast(colors.scrollbar.normal, base);
            let active = contrast(colors.scrollbar.active, base);

            assert!(
                active > resting,
                "{:?}: 活动拇指 {active:.2}:1 ≤ 静止拇指 {resting:.2}:1",
                appearance.appearance
            );
        }
    }

    #[test]
    fn a_washed_fill_is_opaque_exactly_when_the_variant_fills_something() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let colors = theme.components();

            for state in [
                colors.interactive.tonal.hover,
                colors.interactive.tonal.active,
            ] {
                assert!(
                    crate::theme::color::oklch_components(state.background)[3] == 1.0,
                    "{:?}: tonal 的 hover 是不透明填充上的洗色",
                    appearance.appearance
                );
            }

            for state in [
                colors.interactive.outline.hover,
                colors.interactive.outline.active,
                colors.interactive.ghost.hover,
                colors.interactive.ghost.active,
            ] {
                assert!(
                    crate::theme::color::oklch_components(state.background)[3] < 1.0,
                    "{:?}: outline/ghost 的洗色要保留 alpha",
                    appearance.appearance
                );
            }
        }
    }

    #[test]
    fn a_tonal_control_draws_its_edge_against_its_fill_and_the_overlay() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let roles = resolve_roles(&appearance);
            let tonal = theme.components().interactive.tonal;

            assert_ne!(
                tonal.normal.border, tonal.normal.background,
                "{:?}: tonal 的静止边缘就是它自己的填充，控件没有边界",
                appearance.appearance
            );

            for (state, colors) in [
                ("normal", tonal.normal),
                ("hover", tonal.hover),
                ("active", tonal.active),
            ] {
                for (surface_name, surface) in [
                    ("surface.control", roles.get(Role::SurfaceControl)),
                    ("surface.overlay", roles.get(Role::SurfaceOverlay)),
                ] {
                    let ratio = contrast(colors.border, surface);

                    assert!(
                        ratio >= 3.0,
                        "{:?}: tonal {state} 边缘对 {surface_name} 只读 {ratio:.2}:1",
                        appearance.appearance
                    );
                }
            }
        }
    }

    #[test]
    fn a_menu_casts_a_light_translucent_black_shadow() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let colors = theme.components();
            let shadow = colors.menu.shadow;
            let [lightness, chroma, _, alpha] = crate::theme::color::oklch_components(shadow);

            assert!(alpha > 0.0 && alpha < 1.0, "菜单阴影必须可叠");
            assert!(lightness < 1e-3 && chroma < 1e-3, "阴影恒为黑");
            assert!(
                alpha < crate::theme::color::oklch_components(colors.dialog.shadow)[3],
                "菜单阴影不能比对话框的还重"
            );
        }
    }

    #[test]
    fn every_variant_and_state_resolves() {
        let theme = Theme::of(crate::theme::appearance::Appearance::Light);
        let components = theme.components();

        for variant in [
            InteractiveVariant::Filled,
            InteractiveVariant::Tonal,
            InteractiveVariant::Outline,
            InteractiveVariant::Ghost,
        ] {
            let states = match variant {
                InteractiveVariant::Filled => components.interactive.filled,
                InteractiveVariant::Tonal => components.interactive.tonal,
                InteractiveVariant::Outline => components.interactive.outline,
                InteractiveVariant::Ghost => components.interactive.ghost,
            };

            for state in [states.normal, states.hover, states.active, states.disabled] {
                let _ = (state.background, state.foreground, state.border);
            }
        }

        assert_eq!(
            crate::theme::color::oklch_components(components.interactive.ghost.normal.background)
                [3],
            0.0
        );
        assert_eq!(
            crate::theme::color::oklch_components(components.interactive.outline.normal.background)
                [3],
            0.0
        );
    }

    #[test]
    fn the_tonal_palette_is_locked_to_the_engine_linear_composite() {
        let cases: [(Appearance, &str, [u8; 3], [u8; 3], [u8; 3]); 8] = [
            (
                Appearance::Light,
                "normal",
                [255, 255, 255],
                [29, 31, 46],
                [140, 142, 150],
            ),
            (
                Appearance::Light,
                "hover",
                [248, 248, 248],
                [29, 31, 46],
                [87, 89, 101],
            ),
            (
                Appearance::Light,
                "active",
                [244, 244, 244],
                [29, 31, 46],
                [87, 89, 101],
            ),
            (
                Appearance::Light,
                "disabled",
                [255, 255, 255],
                [156, 158, 165],
                [189, 190, 195],
            ),
            (
                Appearance::Dark,
                "normal",
                [27, 33, 40],
                [235, 241, 248],
                [107, 114, 121],
            ),
            (
                Appearance::Dark,
                "hover",
                [78, 82, 87],
                [235, 241, 248],
                [129, 135, 143],
            ),
            (
                Appearance::Dark,
                "active",
                [99, 103, 108],
                [235, 241, 248],
                [129, 135, 143],
            ),
            (
                Appearance::Dark,
                "disabled",
                [27, 33, 40],
                [93, 99, 106],
                [72, 78, 86],
            ),
        ];

        for (appearance, state, background, foreground, border) in cases {
            let tonal = Theme::of(appearance).components().interactive.tonal;
            let colors = match state {
                "normal" => tonal.normal,
                "hover" => tonal.hover,
                "active" => tonal.active,
                "disabled" => tonal.disabled,
                other => panic!("未知状态 {other}"),
            };

            for (field, color, expected) in [
                ("background", colors.background, background),
                ("foreground", colors.foreground, foreground),
                ("border", colors.border, border),
            ] {
                let actual = crate::theme::color::to_srgb8(color);
                assert_eq!(
                    [actual[0], actual[1], actual[2]],
                    expected,
                    "{appearance:?} tonal {state}.{field} 的引擎线性合成基线"
                );
            }
        }
    }

    #[test]
    fn the_tonal_washes_register_the_old_srgb_versus_new_linear_difference() {
        let cases: [(Appearance, &str, [u8; 3], [u8; 3], [i32; 3]); 4] = [
            (
                Appearance::Light,
                "hover",
                [241, 242, 242],
                [248, 248, 248],
                [7, 6, 6],
            ),
            (
                Appearance::Light,
                "active",
                [232, 233, 234],
                [244, 244, 244],
                [12, 11, 10],
            ),
            (
                Appearance::Dark,
                "hover",
                [44, 50, 57],
                [78, 82, 87],
                [34, 32, 30],
            ),
            (
                Appearance::Dark,
                "active",
                [56, 62, 69],
                [99, 103, 108],
                [43, 41, 39],
            ),
        ];

        for (appearance, state, old_expected, new_expected, registered) in cases {
            let theme = Theme::of(appearance);
            let new_washes = theme.components().interactive.tonal;
            let new_color = match state {
                "hover" => new_washes.hover.background,
                "active" => new_washes.active.background,
                other => panic!("未知状态 {other}"),
            };

            let old = old_expected;
            let new = crate::theme::color::to_srgb8(new_color);
            assert_eq!(
                [new[0], new[1], new[2]],
                new_expected,
                "{appearance:?} tonal {state} 的新线性合成值"
            );
            let delta = [
                i32::from(new[0]) - i32::from(old[0]),
                i32::from(new[1]) - i32::from(old[1]),
                i32::from(new[2]) - i32::from(old[2]),
            ];
            assert_eq!(delta, registered, "{appearance:?} tonal {state} 的登记差异");
        }
    }

    const FROZEN_SIZE_LADDER: [(ControlSize, f32, f32, f32, f32, f32); 3] = [
        (ControlSize::Small, 20.0, 4.0, 4.0, 12.0, 8.0),
        (ControlSize::Medium, 30.0, 6.0, 6.0, 14.0, 12.0),
        (ControlSize::Large, 40.0, 8.0, 10.0, 18.0, 16.0),
    ];

    #[test]
    fn the_size_token_ladder_matches_the_frozen_legacy_ladder() {
        let tokens = ComponentTokens::standard();
        for (size, height, gap, padding_x, font_size, icon_size) in FROZEN_SIZE_LADDER {
            let token = tokens.for_size(size);
            assert_eq!(token.height, px(height), "高度");
            assert_eq!(token.gap, px(gap), "间隙");
            assert_eq!(token.padding_x, px(padding_x), "水平内边距");
            assert_eq!(
                token.font_size,
                px(font_size),
                "字号（旧 rems × 16 px 根字号）"
            );
            assert_eq!(token.icon_size, px(icon_size), "图标边长");
        }

        assert_eq!(tokens.border_width, px(1.0));
        assert_eq!(tokens.animation_tick, std::time::Duration::from_millis(150));
        assert_eq!(tokens.shadow.dialog_alpha, 0.25);
        assert_eq!(tokens.shadow.menu_alpha, 0.12);
        assert_eq!(tokens.scrollbar.resting_alpha, 0.22);
        assert_eq!(tokens.scrollbar.active_alpha, 0.40);
    }
}
