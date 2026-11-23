use ui_core::paint::color::Color;

use super::{
    appearance::Appearance,
    color::{black_with_alpha, contrast, mix_oklab, step, tint, with_alpha},
    palette::Palette,
    role::Role,
};

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct RoleColors {
    colors: [Color; Role::COUNT],
}

impl RoleColors {
    pub fn get(&self, role: Role) -> Color {
        self.colors[role.index()]
    }

    pub fn iter(&self) -> impl Iterator<Item = (Role, Color)> + '_ {
        Role::ALL
            .into_iter()
            .map(|role| (role, self.colors[role.index()]))
    }

    pub(crate) fn contrast_floors(&self) -> [(Role, Role, f32); 58] {
        [
            (Role::FgDefault, Role::SurfaceCanvas, 12.0),
            (Role::FgDefault, Role::SurfaceSunken, 12.0),
            (Role::FgMuted, Role::SurfaceCanvas, 4.5),
            (Role::FgMuted, Role::SurfaceSunken, 4.5),
            (Role::FgSubtle, Role::SurfaceCanvas, 3.0),
            (Role::FgSubtle, Role::SurfaceRaised, 3.0),
            (Role::FgDisabled, Role::SurfaceCanvas, 2.0),
            (Role::LineDivider, Role::SurfaceCanvas, 1.5),
            (Role::LineDivider, Role::SurfaceRaised, 1.5),
            (Role::LineDivider, Role::SurfaceSunken, 1.5),
            (Role::LineDivider, Role::SurfaceOverlay, 1.5),
            (Role::LineBorder, Role::SurfaceControl, 3.0),
            (Role::LineBorder, Role::SurfaceCanvas, 3.0),
            (Role::LineStrong, Role::SurfaceCanvas, 3.7),
            (Role::MarketUp, Role::SurfaceCanvas, 4.5),
            (Role::MarketUp, Role::SurfaceSunken, 4.5),
            (Role::MarketDown, Role::SurfaceCanvas, 4.5),
            (Role::MarketDown, Role::SurfaceSunken, 4.5),
            (Role::StatusDangerBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusWarningBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusSuccessBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusInfoBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusElevatedBase, Role::SurfaceCanvas, 4.5),
            (Role::BrandBase, Role::SurfaceCanvas, 4.5),
            (Role::BrandBase, Role::SurfaceSunken, 4.5),
            (Role::BrandBase, Role::SurfaceRaised, 4.5),
            (Role::FgMuted, Role::SurfaceRaised, 4.5),
            (Role::StatusDangerBase, Role::StatusDangerSurface, 4.5),
            (Role::StatusWarningBase, Role::StatusWarningSurface, 4.5),
            (Role::StatusSuccessBase, Role::StatusSuccessSurface, 4.5),
            (Role::StatusInfoBase, Role::StatusInfoSurface, 4.5),
            (Role::StatusElevatedBase, Role::StatusElevatedSurface, 4.5),
            (Role::MarketUp, Role::MarketUpSurface, 4.5),
            (Role::MarketDown, Role::MarketDownSurface, 4.5),
            (Role::FgOnBrand, Role::BrandBase, 4.5),
            (Role::FgOnBrand, Role::BrandHover, 4.5),
            (Role::FgOnInverse, Role::SurfaceInverse, 12.0),
            (Role::LineFocus, Role::SurfaceCanvas, 3.0),
            (Role::ChartAxis, Role::SurfaceSunken, 3.0),
            (Role::ChartGrid, Role::SurfaceSunken, 1.15),
            (Role::ChartSeries0, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries1, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries2, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries3, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries4, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries5, Role::SurfaceSunken, 3.0),
            (Role::ChartMapLand, Role::ChartMapOcean, MAP_FILL_FLOOR),
            (Role::ChartMapOcean, Role::SurfaceSunken, 1.03),
            (
                Role::ChartMapBorder,
                Role::ChartMapLand,
                MAP_STROKE_LAND_FLOOR,
            ),
            (
                Role::ChartMapBorder,
                Role::ChartMapOcean,
                MAP_STROKE_OCEAN_FLOOR,
            ),
            (
                Role::ChartMapRoad,
                Role::ChartMapLand,
                MAP_STROKE_LAND_FLOOR,
            ),
            (
                Role::ChartMapRoad,
                Role::ChartMapOcean,
                MAP_STROKE_OCEAN_FLOOR,
            ),
            (
                Role::ChartMapRoadMinor,
                Role::ChartMapLand,
                MAP_STROKE_LAND_FLOOR,
            ),
            (
                Role::ChartMapRoadMinor,
                Role::ChartMapOcean,
                MAP_STROKE_OCEAN_FLOOR,
            ),
            (Role::SurfaceRaised, Role::SurfaceCanvas, 1.02),
            (Role::SurfaceSunken, Role::SurfaceCanvas, 1.05),
            (Role::SurfaceOverlay, Role::SurfaceCanvas, 1.02),
            (Role::SurfaceInverse, Role::SurfaceCanvas, 7.0),
        ]
    }

    pub(crate) fn assert_contrast_floors(&self) {
        if cfg!(debug_assertions) {
            for (foreground, background, floor) in self.contrast_floors() {
                let ratio = contrast(self.get(foreground), self.get(background));

                debug_assert!(
                    ratio >= floor,
                    "{} on {} reads {ratio:.2}:1, where its floor is {floor:.2}:1",
                    foreground.name(),
                    background.name()
                );
            }
        }
    }
}

pub(crate) fn resolve_roles(palette: &Palette) -> RoleColors {
    let dark = palette.appearance == Appearance::Dark;
    let mix = |from: Color, to: Color, weight: f32| mix_oklab(from, to, weight);
    let canvas = palette.canvas;
    let text = palette.text;
    let mut colors = [Color::TRANSPARENT; Role::COUNT];
    {
        let mut set = |role: Role, color: Color| colors[role.index()] = color;

        set(Role::SurfaceCanvas, canvas);
        set(Role::SurfaceRaised, palette.surface_raised);
        set(Role::SurfaceSunken, palette.surface_sunken);
        set(Role::SurfaceOverlay, palette.overlay);
        set(Role::SurfaceControl, palette.control);
        set(Role::SurfaceInverse, text);

        set(Role::FgDefault, text);
        set(Role::FgMuted, mix(text, canvas, 0.30));
        set(Role::FgSubtle, mix(text, canvas, 0.50));
        set(Role::FgDisabled, mix(text, canvas, 0.62));
        set(Role::FgOnBrand, palette.on_brand);
        set(Role::FgOnInverse, palette.on_inverse);

        set(
            Role::LineDivider,
            mix(text, canvas, if dark { 0.72 } else { 0.76 }),
        );
        set(Role::LineBorder, mix(text, canvas, 0.55));
        set(
            Role::LineStrong,
            mix(text, canvas, if dark { 0.45 } else { 0.30 }),
        );
        set(Role::LineFocus, palette.brand);

        set(Role::BrandBase, palette.brand);
        set(Role::BrandHover, step(palette.brand, 0.045, dark));
        set(Role::BrandActive, step(palette.brand, 0.075, dark));

        let status_role =
            |color: Color| [color, tint(color, canvas, dark), mix(color, canvas, 0.62)];
        let statuses = [
            (Role::StatusDangerBase, palette.status.danger),
            (Role::StatusWarningBase, palette.status.warning),
            (Role::StatusSuccessBase, palette.status.success),
            (Role::StatusInfoBase, palette.status.info),
            (Role::StatusElevatedBase, palette.status.elevated),
        ];
        for (base, color) in statuses {
            let [base_color, surface, border] = status_role(color);
            let index = base.index();
            set(base, base_color);
            set(Role::ALL[index + 1], surface);
            set(Role::ALL[index + 2], border);
        }

        set(Role::MarketUp, palette.market.up);
        set(Role::MarketDown, palette.market.down);
        set(Role::MarketFlat, mix(text, canvas, 0.50));
        set(Role::MarketUpSurface, tint(palette.market.up, canvas, dark));
        set(
            Role::MarketDownSurface,
            tint(palette.market.down, canvas, dark),
        );
        set(Role::MarketUpWeak, with_alpha(palette.market.up, 0.22));
        set(Role::MarketDownWeak, with_alpha(palette.market.down, 0.22));

        set(Role::FearGreedExtremeFear, palette.fear_greed.extreme_fear);
        set(Role::FearGreedFear, palette.fear_greed.fear);
        set(Role::FearGreedNeutral, palette.fear_greed.neutral);
        set(Role::FearGreedGreed, palette.fear_greed.greed);
        set(
            Role::FearGreedExtremeGreed,
            palette.fear_greed.extreme_greed,
        );

        set(Role::ChartGrid, mix(text, palette.surface_sunken, 0.86));
        set(Role::ChartAxis, mix(text, canvas, 0.50));
        set(
            Role::ChartCrosshair,
            mix(text, palette.surface_sunken, 0.45),
        );
        set(Role::ChartMapOcean, palette.map_ocean);
        set(Role::ChartMapLand, palette.map_land);
        set(Role::ChartMapBorder, palette.map_border);
        set(Role::ChartMapRoad, palette.map_road);
        set(Role::ChartMapRoadMinor, palette.map_road_minor);
        for (position, series) in [
            Role::ChartSeries0,
            Role::ChartSeries1,
            Role::ChartSeries2,
            Role::ChartSeries3,
            Role::ChartSeries4,
            Role::ChartSeries5,
        ]
        .into_iter()
        .zip(palette.series)
        {
            set(position, series);
        }

        set(Role::Selection, mix(palette.brand, canvas, 0.62));
        set(Role::VeilHover, with_alpha(text, palette.veil_hover));
        set(Role::VeilActive, with_alpha(text, palette.veil_active));
        set(Role::Scrim, black_with_alpha(palette.scrim_alpha));
    }

    let roles = RoleColors { colors };
    roles.assert_contrast_floors();

    roles
}

const MAP_FILL_FLOOR: f32 = 1.35;

const MAP_STROKE_LAND_FLOOR: f32 = 2.5;

const MAP_STROKE_OCEAN_FLOOR: f32 = 2.0;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        MAP_FILL_FLOOR, MAP_STROKE_LAND_FLOOR, MAP_STROKE_OCEAN_FLOOR, contrast, resolve_roles,
    };
    use crate::theme::{appearance::Appearance, palette::Palette, resolved::Theme, role::Role};

    fn palettes() -> [Palette; 2] {
        [Palette::light(), Palette::dark()]
    }

    #[test]
    fn every_contrast_floor_holds_in_both_appearances() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for (foreground, background, floor) in roles.contrast_floors() {
                let ratio = contrast(roles.get(foreground), roles.get(background));

                assert!(
                    ratio >= floor,
                    "{:?}: {} on {} reads {ratio:.2}:1, where its floor is {floor:.2}:1",
                    palette.appearance,
                    foreground.name(),
                    background.name()
                );
            }
        }
    }

    #[test]
    fn the_contrast_floor_table_covers_fifty_eight_pairs() {
        use std::collections::HashSet;

        let roles = resolve_roles(&Palette::light());
        let floors = roles.contrast_floors();
        assert_eq!(floors.len(), 58, "旧实现的 58 条对比度地板必须逐条保留");

        let foregrounds: HashSet<_> = floors
            .iter()
            .map(|(foreground, _, _)| *foreground)
            .collect();
        let backgrounds: HashSet<_> = floors
            .iter()
            .map(|(_, background, _)| *background)
            .collect();
        for role in [
            Role::StatusDangerBase,
            Role::StatusWarningBase,
            Role::StatusSuccessBase,
            Role::StatusInfoBase,
            Role::StatusElevatedBase,
            Role::ChartSeries0,
            Role::ChartSeries1,
            Role::ChartSeries2,
            Role::ChartSeries3,
            Role::ChartSeries4,
            Role::ChartSeries5,
            Role::MarketUp,
            Role::MarketDown,
            Role::BrandBase,
            Role::FgDefault,
            Role::FgMuted,
        ] {
            assert!(
                foregrounds.contains(&role),
                "{} 必须出现在地板前景里",
                role.name()
            );
        }
        for role in [
            Role::SurfaceCanvas,
            Role::SurfaceRaised,
            Role::SurfaceSunken,
            Role::SurfaceOverlay,
            Role::SurfaceControl,
            Role::SurfaceInverse,
        ] {
            assert!(
                backgrounds.contains(&role),
                "{} 必须出现在地板背景里",
                role.name()
            );
        }
        assert!(backgrounds.contains(&Role::ChartMapLand));
        assert!(backgrounds.contains(&Role::ChartMapOcean));
    }

    #[test]
    #[cfg(debug_assertions)]
    fn a_palette_that_breaks_a_floor_is_rejected_when_the_theme_loads() {
        let mut broken = Palette::light();
        broken.text = ui_core::paint::color::Color::oklch(0.9, 0.0, 0.0, 1.0);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolve_roles(&broken)));
        assert!(
            result.is_err(),
            "debug 构建必须在 `resolve_roles` 里断言对比度地板（空实现 = 审查 F3-1 回归）"
        );
    }

    #[test]
    fn surfaces_stack_raised_above_canvas_above_sunken() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let lightness = |role| crate::theme::color::oklch_components(roles.get(role))[0];
            let raised = lightness(Role::SurfaceRaised);
            let canvas = lightness(Role::SurfaceCanvas);
            let sunken = lightness(Role::SurfaceSunken);

            assert!(
                raised > canvas,
                "{:?}: 面板头 L {raised:.3} 不高于页面 L {canvas:.3}",
                palette.appearance
            );
            assert!(
                canvas > sunken,
                "{:?}: 页面 L {canvas:.3} 不低于内容面 L {sunken:.3}",
                palette.appearance
            );
        }
    }

    #[test]
    fn brand_is_three_times_more_chromatic_than_text() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let chroma = |role| crate::theme::color::oklch_components(roles.get(role))[1];
            let brand = chroma(Role::BrandBase);
            let text = chroma(Role::FgDefault);

            assert!(
                brand >= 3.0 * text,
                "{:?}: 品牌色 C {brand:.3} 不到正文 C {text:.3} 的三倍",
                palette.appearance
            );
        }
    }

    #[test]
    fn chart_series_are_told_apart_by_oklab_distance() {
        let mut compared = 0;

        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let series: Vec<_> = Role::ALL
                .into_iter()
                .filter(|role| {
                    matches!(
                        role,
                        Role::ChartSeries0
                            | Role::ChartSeries1
                            | Role::ChartSeries2
                            | Role::ChartSeries3
                            | Role::ChartSeries4
                            | Role::ChartSeries5
                    )
                })
                .collect();

            assert_eq!(series.len(), 6);

            for (first, first_role) in series.iter().enumerate() {
                for second_role in series.iter().skip(first + 1) {
                    let first = crate::theme::color::oklab(roles.get(*first_role));
                    let second = crate::theme::color::oklab(roles.get(*second_role));
                    let distance = ((first[0] - second[0]).powi(2)
                        + (first[1] - second[1]).powi(2)
                        + (first[2] - second[2]).powi(2))
                    .sqrt();

                    compared += 1;
                    assert!(
                        distance >= 0.13,
                        "{:?}: {} 与 {} 在 Oklab 里只差 {distance:.3}",
                        palette.appearance,
                        first_role.name(),
                        second_role.name()
                    );
                }
            }
        }

        assert_eq!(compared, 30, "两种外观各比较 15 对");
    }

    #[test]
    fn every_chart_series_reads_on_the_sunken_surface() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for series in [
                Role::ChartSeries0,
                Role::ChartSeries1,
                Role::ChartSeries2,
                Role::ChartSeries3,
                Role::ChartSeries4,
                Role::ChartSeries5,
            ] {
                let ratio = contrast(roles.get(series), roles.get(Role::SurfaceSunken));

                assert!(
                    ratio >= 3.0,
                    "{:?}: {} reads {ratio:.2}:1 on surface.sunken",
                    palette.appearance,
                    series.name()
                );
            }
        }
    }

    #[test]
    fn the_fear_greed_scale_is_told_apart_and_reads_on_the_dark_face() {
        let scale = [
            Role::FearGreedExtremeFear,
            Role::FearGreedFear,
            Role::FearGreedNeutral,
            Role::FearGreedGreed,
            Role::FearGreedExtremeGreed,
        ];

        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for (first, first_role) in scale.iter().enumerate() {
                for second_role in scale.iter().skip(first + 1) {
                    let first = crate::theme::color::oklab(roles.get(*first_role));
                    let second = crate::theme::color::oklab(roles.get(*second_role));
                    let distance = ((first[0] - second[0]).powi(2)
                        + (first[1] - second[1]).powi(2)
                        + (first[2] - second[2]).powi(2))
                    .sqrt();

                    assert!(
                        distance >= 0.13,
                        "{:?}: {} 与 {} 在 Oklab 里只差 {distance:.3}",
                        palette.appearance,
                        first_role.name(),
                        second_role.name()
                    );
                }
            }
        }

        let dark = resolve_roles(&Palette::dark());
        for role in scale {
            let ratio = contrast(dark.get(role), dark.get(Role::SurfaceSunken));
            assert!(
                ratio >= 3.0,
                "深色外观：{} 在内容面上只读 {ratio:.2}:1",
                role.name()
            );
        }
    }

    #[test]
    fn only_the_veils_the_scrim_and_the_weak_market_colors_are_translucent() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for (role, color) in roles.iter() {
                let alpha = crate::theme::color::oklch_components(color)[3];
                let translucent = matches!(
                    role,
                    Role::VeilHover
                        | Role::VeilActive
                        | Role::Scrim
                        | Role::MarketUpWeak
                        | Role::MarketDownWeak
                );

                if translucent {
                    assert!(
                        alpha > 0.0 && alpha < 1.0,
                        "{:?}: {} alpha {alpha:.2}，它必须可叠",
                        palette.appearance,
                        role.name()
                    );
                } else {
                    assert!(
                        (alpha - 1.0).abs() < 1e-6,
                        "{:?}: {} alpha {alpha:.2}，它必须不透明才能被读",
                        palette.appearance,
                        role.name()
                    );
                }
            }
        }
    }

    #[test]
    fn map_land_and_map_ocean_stay_apart_in_both_appearances() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let ratio = contrast(
                roles.get(Role::ChartMapLand),
                roles.get(Role::ChartMapOcean),
            );

            assert!(
                ratio >= MAP_FILL_FLOOR,
                "{:?}: 海岸线只读 {ratio:.2}:1，地板 {MAP_FILL_FLOOR:.2}:1",
                palette.appearance
            );
        }
    }

    #[test]
    fn map_strokes_read_on_both_fills_in_both_appearances() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for stroke in [
                Role::ChartMapBorder,
                Role::ChartMapRoad,
                Role::ChartMapRoadMinor,
            ] {
                for (fill, floor) in [
                    (Role::ChartMapLand, MAP_STROKE_LAND_FLOOR),
                    (Role::ChartMapOcean, MAP_STROKE_OCEAN_FLOOR),
                ] {
                    let ratio = contrast(roles.get(stroke), roles.get(fill));

                    assert!(
                        ratio >= floor,
                        "{:?}: {} on {} reads {ratio:.2}:1, where the map floor is {floor:.2}:1",
                        palette.appearance,
                        stroke.name(),
                        fill.name()
                    );
                }
            }
        }
    }

    #[test]
    fn the_fear_greed_scale_paints_its_authored_colors() {
        let cases: [(Appearance, Role, [u8; 3]); 8] = [
            (
                Appearance::Light,
                Role::FearGreedExtremeFear,
                [0xB6, 0x14, 0x29],
            ),
            (Appearance::Light, Role::FearGreedFear, [0xF6, 0x46, 0x5D]),
            (
                Appearance::Light,
                Role::FearGreedNeutral,
                [0xF8, 0xD3, 0x3A],
            ),
            (Appearance::Light, Role::FearGreedGreed, [0x2F, 0xE4, 0x9F]),
            (
                Appearance::Light,
                Role::FearGreedExtremeGreed,
                [0x27, 0xA1, 0x72],
            ),
            (
                Appearance::Dark,
                Role::FearGreedExtremeFear,
                [0xCC, 0x32, 0x3C],
            ),
            (Appearance::Dark, Role::FearGreedFear, [0xFA, 0x67, 0x73]),
            (
                Appearance::Dark,
                Role::FearGreedExtremeGreed,
                [0x3C, 0xB0, 0x80],
            ),
        ];

        for (appearance, role, expected) in cases {
            let theme = Theme::of(appearance);
            let actual = crate::theme::color::to_srgb8(theme.role(role));

            for channel in 0..3 {
                let difference = i32::from(actual[channel]) - i32::from(expected[channel]);
                assert!(
                    difference.abs() <= 2,
                    "{appearance:?} {} 通道 {channel}: 引擎 {} vs 旧实现 {}",
                    role.name(),
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    fn every_role_resolves_in_both_appearances() {
        let light = resolve_roles(&Palette::light());
        let dark = resolve_roles(&Palette::dark());
        let shared_by_design = [Role::FearGreedNeutral, Role::FearGreedGreed];
        let mut seen = HashSet::with_capacity(Role::COUNT);
        let mut differing = 0;

        for role in Role::ALL {
            assert!(seen.insert(role.index()));
            let light_color = light.get(role);
            let dark_color = dark.get(role);
            assert!(
                matches!(light_color, ui_core::paint::color::Color::Oklch { .. }),
                "{} 必须是 OKLCH 分量",
                role.name()
            );
            let differs = light_color != dark_color;
            if shared_by_design.contains(&role) {
                assert!(
                    !differs,
                    "{} 是两套外观共享的刻度中间档，不应换色",
                    role.name()
                );
            } else {
                assert!(differs, "{} 在两种外观下必须换一个颜色", role.name());
                differing += 1;
            }
        }

        assert_eq!(seen.len(), Role::COUNT);
        assert_eq!(differing, Role::COUNT - shared_by_design.len());
    }
}
