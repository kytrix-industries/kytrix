use ui_core::paint::color::Color;

use super::appearance::Appearance;

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct StatusPalette {
    pub danger: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    pub elevated: Color,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct MarketPalette {
    pub up: Color,
    pub down: Color,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct FearGreedPalette {
    pub extreme_fear: Color,
    pub fear: Color,
    pub neutral: Color,
    pub greed: Color,
    pub extreme_greed: Color,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Palette {
    pub appearance: Appearance,
    pub canvas: Color,
    pub surface_raised: Color,
    pub surface_sunken: Color,
    pub overlay: Color,
    pub control: Color,
    pub text: Color,
    pub brand: Color,
    pub status: StatusPalette,
    pub market: MarketPalette,
    pub fear_greed: FearGreedPalette,
    pub series: [Color; 6],
    pub map_ocean: Color,
    pub map_land: Color,
    pub map_border: Color,
    pub map_road: Color,
    pub map_road_minor: Color,
    pub on_brand: Color,
    pub on_inverse: Color,
    pub scrim_alpha: f32,
    pub veil_hover: f32,
    pub veil_active: f32,
}

impl Palette {
    pub fn light() -> Self {
        let opacity = 1.0;

        Self {
            appearance: Appearance::Light,
            canvas: Color::oklch(0.978, 0.0, 0.0, opacity),
            surface_raised: Color::oklch(1.0, 0.0, 0.0, opacity),
            surface_sunken: Color::oklch(0.948, 0.0, 0.0, opacity),
            overlay: Color::oklch(1.0, 0.0, 0.0, opacity),
            control: Color::oklch(1.0, 0.0, 0.0, opacity),
            text: Color::oklch(0.245, 0.028, 278.0, opacity),
            brand: Color::oklch(0.526, 0.152, 249.5, opacity),
            status: StatusPalette {
                danger: Color::oklch(0.532, 0.190, 26.0, opacity),
                warning: Color::oklch(0.545, 0.116, 76.0, opacity),
                success: Color::oklch(0.520, 0.124, 154.0, opacity),
                info: Color::oklch(0.530, 0.104, 232.0, opacity),
                elevated: Color::oklch(0.545, 0.148, 45.0, opacity),
            },
            market: MarketPalette {
                up: Color::oklch(0.514, 0.128, 152.0, opacity),
                down: Color::oklch(0.542, 0.180, 22.0, opacity),
            },
            fear_greed: FearGreedPalette {
                extreme_fear: Color::oklch(0.4958, 0.1905, 22.83, opacity),
                fear: Color::oklch(0.6529, 0.2110, 18.20, opacity),
                neutral: Color::oklch(0.8724, 0.1636, 94.59, opacity),
                greed: Color::oklch(0.8148, 0.1705, 161.41, opacity),
                extreme_greed: Color::oklch(0.6306, 0.1263, 162.21, opacity),
            },
            series: [
                Color::oklch(0.620, 0.140, 62.0, opacity),
                Color::oklch(0.500, 0.150, 276.0, opacity),
                Color::oklch(0.560, 0.130, 194.0, opacity),
                Color::oklch(0.560, 0.170, 350.0, opacity),
                Color::oklch(0.540, 0.130, 120.0, opacity),
                Color::oklch(0.500, 0.010, 0.0, opacity),
            ],
            map_ocean: Color::oklch(0.841, 0.046, 217.5, opacity),
            map_land: Color::oklch(0.953, 0.009, 84.6, opacity),
            map_border: Color::oklch(0.647, 0.133, 311.1, opacity),
            map_road: Color::oklch(0.591, 0.213, 7.2, opacity),
            map_road_minor: Color::oklch(0.570, 0.120, 74.9, opacity),
            on_brand: Color::oklch(0.978, 0.0, 0.0, opacity),
            on_inverse: Color::oklch(0.978, 0.0, 0.0, opacity),
            scrim_alpha: 0.42,
            veil_hover: 0.06,
            veil_active: 0.10,
        }
    }

    pub fn dark() -> Self {
        let opacity = 1.0;

        Self {
            appearance: Appearance::Dark,
            canvas: Color::oklch(0.215, 0.016, 252.4, opacity),
            surface_raised: Color::oklch(0.262, 0.016, 252.4, opacity),
            surface_sunken: Color::oklch(0.176, 0.016, 252.4, opacity),
            overlay: Color::oklch(0.278, 0.016, 252.4, opacity),
            control: Color::oklch(0.245, 0.016, 252.4, opacity),
            text: Color::oklch(0.955, 0.012, 255.0, opacity),
            brand: Color::oklch(0.722, 0.130, 242.0, opacity),
            status: StatusPalette {
                danger: Color::oklch(0.708, 0.170, 20.0, opacity),
                warning: Color::oklch(0.822, 0.150, 84.0, opacity),
                success: Color::oklch(0.762, 0.150, 160.0, opacity),
                info: Color::oklch(0.752, 0.118, 226.0, opacity),
                elevated: Color::oklch(0.760, 0.150, 52.0, opacity),
            },
            market: MarketPalette {
                up: Color::oklch(0.782, 0.158, 150.0, opacity),
                down: Color::oklch(0.702, 0.186, 20.0, opacity),
            },
            fear_greed: FearGreedPalette {
                extreme_fear: Color::oklch(0.5600, 0.1900, 22.83, opacity),
                fear: Color::oklch(0.7000, 0.1800, 18.20, opacity),
                neutral: Color::oklch(0.8724, 0.1636, 94.59, opacity),
                greed: Color::oklch(0.8148, 0.1705, 161.41, opacity),
                extreme_greed: Color::oklch(0.6800, 0.1263, 162.21, opacity),
            },
            series: [
                Color::oklch(0.800, 0.150, 78.0, opacity),
                Color::oklch(0.720, 0.130, 282.0, opacity),
                Color::oklch(0.780, 0.120, 196.0, opacity),
                Color::oklch(0.740, 0.170, 350.0, opacity),
                Color::oklch(0.700, 0.140, 120.0, opacity),
                Color::oklch(0.660, 0.010, 0.0, opacity),
            ],
            map_ocean: Color::oklch(0.150, 0.000, 0.0, opacity),
            map_land: Color::oklch(0.283, 0.000, 0.0, opacity),
            map_border: Color::oklch(0.813, 0.090, 311.0, opacity),
            map_road: Color::oklch(0.633, 0.000, 0.0, opacity),
            map_road_minor: Color::oklch(0.530, 0.000, 0.0, opacity),
            on_brand: Color::oklch(0.176, 0.020, 245.0, opacity),
            on_inverse: Color::oklch(0.176, 0.016, 252.4, opacity),
            scrim_alpha: 0.55,
            veil_hover: 0.08,
            veil_active: 0.14,
        }
    }

    pub fn of(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light => Self::light(),
            Appearance::Dark => Self::dark(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Palette};
    use crate::theme::color::oklch_components;

    const LIGHT_LEGACY: [(&str, [f32; 4]); 35] = [
        ("canvas", [0.978, 0.0, 0.0, 1.0]),
        ("surface_raised", [1.0, 0.0, 0.0, 1.0]),
        ("surface_sunken", [0.948, 0.0, 0.0, 1.0]),
        ("overlay", [1.0, 0.0, 0.0, 1.0]),
        ("control", [1.0, 0.0, 0.0, 1.0]),
        ("text", [0.245, 0.028, 278.0, 1.0]),
        ("brand", [0.526, 0.152, 249.5, 1.0]),
        ("status.danger", [0.532, 0.19, 26.0, 1.0]),
        ("status.warning", [0.545, 0.116, 76.0, 1.0]),
        ("status.success", [0.52, 0.124, 154.0, 1.0]),
        ("status.info", [0.53, 0.104, 232.0, 1.0]),
        ("status.elevated", [0.545, 0.148, 45.0, 1.0]),
        ("market.up", [0.514, 0.128, 152.0, 1.0]),
        ("market.down", [0.542, 0.18, 22.0, 1.0]),
        ("fear_greed.extreme_fear", [0.4958, 0.1905, 22.83, 1.0]),
        ("fear_greed.fear", [0.6529, 0.211, 18.2, 1.0]),
        ("fear_greed.neutral", [0.8724, 0.1636, 94.59, 1.0]),
        ("fear_greed.greed", [0.8148, 0.1705, 161.41, 1.0]),
        ("fear_greed.extreme_greed", [0.6306, 0.1263, 162.21, 1.0]),
        ("series[0]", [0.62, 0.14, 62.0, 1.0]),
        ("series[1]", [0.5, 0.15, 276.0, 1.0]),
        ("series[2]", [0.56, 0.13, 194.0, 1.0]),
        ("series[3]", [0.56, 0.17, 350.0, 1.0]),
        ("series[4]", [0.54, 0.13, 120.0, 1.0]),
        ("series[5]", [0.5, 0.01, 0.0, 1.0]),
        ("map_ocean", [0.841, 0.046, 217.5, 1.0]),
        ("map_land", [0.953, 0.009, 84.6, 1.0]),
        ("map_border", [0.647, 0.133, 311.1, 1.0]),
        ("map_road", [0.591, 0.213, 7.2, 1.0]),
        ("map_road_minor", [0.57, 0.12, 74.9, 1.0]),
        ("on_brand", [0.978, 0.0, 0.0, 1.0]),
        ("on_inverse", [0.978, 0.0, 0.0, 1.0]),
        ("scrim_alpha", [0.42, 0.0, 0.0, 0.0]),
        ("veil_hover", [0.06, 0.0, 0.0, 0.0]),
        ("veil_active", [0.1, 0.0, 0.0, 0.0]),
    ];

    const DARK_LEGACY: [(&str, [f32; 4]); 35] = [
        ("canvas", [0.215, 0.016, 252.4, 1.0]),
        ("surface_raised", [0.262, 0.016, 252.4, 1.0]),
        ("surface_sunken", [0.176, 0.016, 252.4, 1.0]),
        ("overlay", [0.278, 0.016, 252.4, 1.0]),
        ("control", [0.245, 0.016, 252.4, 1.0]),
        ("text", [0.955, 0.012, 255.0, 1.0]),
        ("brand", [0.722, 0.13, 242.0, 1.0]),
        ("status.danger", [0.708, 0.17, 20.0, 1.0]),
        ("status.warning", [0.822, 0.15, 84.0, 1.0]),
        ("status.success", [0.762, 0.15, 160.0, 1.0]),
        ("status.info", [0.752, 0.118, 226.0, 1.0]),
        ("status.elevated", [0.76, 0.15, 52.0, 1.0]),
        ("market.up", [0.782, 0.158, 150.0, 1.0]),
        ("market.down", [0.702, 0.186, 20.0, 1.0]),
        ("fear_greed.extreme_fear", [0.56, 0.19, 22.83, 1.0]),
        ("fear_greed.fear", [0.7, 0.18, 18.2, 1.0]),
        ("fear_greed.neutral", [0.8724, 0.1636, 94.59, 1.0]),
        ("fear_greed.greed", [0.8148, 0.1705, 161.41, 1.0]),
        ("fear_greed.extreme_greed", [0.68, 0.1263, 162.21, 1.0]),
        ("series[0]", [0.8, 0.15, 78.0, 1.0]),
        ("series[1]", [0.72, 0.13, 282.0, 1.0]),
        ("series[2]", [0.78, 0.12, 196.0, 1.0]),
        ("series[3]", [0.74, 0.17, 350.0, 1.0]),
        ("series[4]", [0.7, 0.14, 120.0, 1.0]),
        ("series[5]", [0.66, 0.01, 0.0, 1.0]),
        ("map_ocean", [0.15, 0.0, 0.0, 1.0]),
        ("map_land", [0.283, 0.0, 0.0, 1.0]),
        ("map_border", [0.813, 0.09, 311.0, 1.0]),
        ("map_road", [0.633, 0.0, 0.0, 1.0]),
        ("map_road_minor", [0.53, 0.0, 0.0, 1.0]),
        ("on_brand", [0.176, 0.02, 245.0, 1.0]),
        ("on_inverse", [0.176, 0.016, 252.4, 1.0]),
        ("scrim_alpha", [0.55, 0.0, 0.0, 0.0]),
        ("veil_hover", [0.08, 0.0, 0.0, 0.0]),
        ("veil_active", [0.14, 0.0, 0.0, 0.0]),
    ];

    fn components(palette: &Palette) -> [(&'static str, [f32; 4]); 35] {
        [
            ("canvas", oklch_components(palette.canvas)),
            ("surface_raised", oklch_components(palette.surface_raised)),
            ("surface_sunken", oklch_components(palette.surface_sunken)),
            ("overlay", oklch_components(palette.overlay)),
            ("control", oklch_components(palette.control)),
            ("text", oklch_components(palette.text)),
            ("brand", oklch_components(palette.brand)),
            ("status.danger", oklch_components(palette.status.danger)),
            ("status.warning", oklch_components(palette.status.warning)),
            ("status.success", oklch_components(palette.status.success)),
            ("status.info", oklch_components(palette.status.info)),
            ("status.elevated", oklch_components(palette.status.elevated)),
            ("market.up", oklch_components(palette.market.up)),
            ("market.down", oklch_components(palette.market.down)),
            (
                "fear_greed.extreme_fear",
                oklch_components(palette.fear_greed.extreme_fear),
            ),
            ("fear_greed.fear", oklch_components(palette.fear_greed.fear)),
            (
                "fear_greed.neutral",
                oklch_components(palette.fear_greed.neutral),
            ),
            (
                "fear_greed.greed",
                oklch_components(palette.fear_greed.greed),
            ),
            (
                "fear_greed.extreme_greed",
                oklch_components(palette.fear_greed.extreme_greed),
            ),
            ("series[0]", oklch_components(palette.series[0])),
            ("series[1]", oklch_components(palette.series[1])),
            ("series[2]", oklch_components(palette.series[2])),
            ("series[3]", oklch_components(palette.series[3])),
            ("series[4]", oklch_components(palette.series[4])),
            ("series[5]", oklch_components(palette.series[5])),
            ("map_ocean", oklch_components(palette.map_ocean)),
            ("map_land", oklch_components(palette.map_land)),
            ("map_border", oklch_components(palette.map_border)),
            ("map_road", oklch_components(palette.map_road)),
            ("map_road_minor", oklch_components(palette.map_road_minor)),
            ("on_brand", oklch_components(palette.on_brand)),
            ("on_inverse", oklch_components(palette.on_inverse)),
            ("scrim_alpha", [palette.scrim_alpha, 0.0, 0.0, 0.0]),
            ("veil_hover", [palette.veil_hover, 0.0, 0.0, 0.0]),
            ("veil_active", [palette.veil_active, 0.0, 0.0, 0.0]),
        ]
    }

    #[test]
    fn every_palette_value_matches_the_frozen_legacy_table() {
        for (appearance, legacy) in [
            (Appearance::Light, LIGHT_LEGACY),
            (Appearance::Dark, DARK_LEGACY),
        ] {
            let new = Palette::of(appearance);
            for ((legacy_name, old), (name, new)) in legacy.into_iter().zip(components(&new)) {
                assert_eq!(legacy_name, name, "{appearance:?} 的字段顺序漂移");
                for index in 0..4 {
                    assert!(
                        (old[index] - new[index]).abs() < 1e-6,
                        "{appearance:?} {name} 分量 {index}: 旧 {} vs 新 {}",
                        old[index],
                        new[index]
                    );
                }
            }
        }
    }

    #[test]
    fn both_appearances_have_the_same_shape_and_their_own_values() {
        let light = Palette::light();
        let dark = Palette::dark();

        assert_eq!(light.appearance, Appearance::Light);
        assert_eq!(dark.appearance, Appearance::Dark);
        assert_ne!(
            oklch_components(light.canvas),
            oklch_components(dark.canvas)
        );
        assert_ne!(light.scrim_alpha, dark.scrim_alpha);
        assert_eq!(light.series.len(), 6);
    }
}
