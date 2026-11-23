use ui_core::paint::color::Color;
use ui_kit::theme::{resolved::Theme, role::Role};

use crate::ship::{SHIP_TYPE_COUNT, colours};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MapColors {
    pub ocean: Color,
    pub land: Color,
    pub map_border: Color,
    pub road: Color,
    pub road_minor: Color,
    pub ships: &'static [Color; SHIP_TYPE_COUNT],
    pub hover: Color,
    pub overlay: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub disabled: Color,
    pub halo: Color,
}

impl MapColors {
    pub(super) fn resolve(theme: &Theme) -> Self {
        Self {
            ocean: theme.role(Role::ChartMapOcean),
            land: theme.role(Role::ChartMapLand),
            map_border: theme.role(Role::ChartMapBorder),
            road: theme.role(Role::ChartMapRoad),
            road_minor: theme.role(Role::ChartMapRoadMinor),
            ships: colours(theme.appearance()),
            hover: theme.role(Role::LineFocus),
            overlay: theme.role(Role::SurfaceOverlay),
            border: theme.role(Role::LineBorder),
            text: theme.role(Role::FgDefault),
            muted: theme.role(Role::FgMuted),
            disabled: theme.role(Role::FgDisabled),
            halo: theme.role(Role::SurfaceOverlay),
        }
    }
}

#[cfg(test)]
mod tests {
    use ui_kit::theme::appearance::Appearance;

    use super::*;

    #[test]
    fn every_map_role_should_be_painted_from_the_theme() {
        let light = MapColors::resolve(&Theme::of(Appearance::Light));
        let theme = Theme::of(Appearance::Light);

        assert_eq!(light.ocean, theme.role(Role::ChartMapOcean));
        assert_eq!(light.land, theme.role(Role::ChartMapLand));
        assert_eq!(light.map_border, theme.role(Role::ChartMapBorder));
        assert_eq!(light.road, theme.role(Role::ChartMapRoad));
        assert_eq!(light.road_minor, theme.role(Role::ChartMapRoadMinor));
    }

    #[test]
    fn the_sea_and_the_land_should_be_different_fills() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            assert_ne!(colors.ocean, colors.land, "{appearance:?}");
            assert_ne!(colors.text, colors.overlay, "{appearance:?}");
        }
    }

    #[test]
    fn every_panel_ink_should_differ_from_the_panel_fill() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            assert_ne!(colors.text, colors.overlay, "{appearance:?}");
            assert_ne!(colors.muted, colors.overlay, "{appearance:?}");
            assert_ne!(colors.border, colors.overlay, "{appearance:?}");
        }
    }

    #[test]
    fn the_two_appearances_should_resolve_different_palettes() {
        let light = MapColors::resolve(&Theme::of(Appearance::Light));
        let dark = MapColors::resolve(&Theme::of(Appearance::Dark));

        assert_ne!(light.ocean, dark.ocean);
        assert_ne!(light.land, dark.land);
        assert_ne!(light.text, dark.text);
    }
}
