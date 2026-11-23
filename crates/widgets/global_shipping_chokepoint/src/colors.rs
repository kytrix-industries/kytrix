//! The colours one frame of the map is drawn with, resolved from the theme.
//!
//! A canvas bakes its colours in, so the map is rebuilt when the appearance moves; the
//! colours are resolved in the same pass that decides whether the canvas has to be
//! rebuilt, which is what makes an appearance switch one of the rebuilding key's inputs.
//!
//! The map and the panels come from the theme; the ship palette is the component's own
//! ([`crate::ship::colours`]) because the theme has six chart series and the map
//! distinguishes twenty types.

use base_ui::theme::{resolved::Theme, role::Role};
use ui_core::paint::color::Color;

use crate::ship::{SHIP_TYPE_COUNT, colours};

/// The colours one frame is drawn with.
///
/// [`crate::map`] reads the map fields; the struct's own `pub(super)` keeps them inside
/// the crate even though the fields themselves are `pub`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MapColors {
    /// The sea, which is the canvas' own first quad.
    pub ocean: Color,
    /// The land the coastline is filled with.
    pub land: Color,
    /// The land borders drawn over the land.
    pub map_border: Color,
    /// Motorways and trunks, drawn over the land and the sea.
    pub road: Color,
    /// Primary roads, the finer of the two road strokes.
    pub road_minor: Color,
    /// One colour per ship type in [`crate::ship::SHIP_TYPES`] order.
    pub ships: &'static [Color; SHIP_TYPE_COUNT],
    /// The ring around the vessel under the pointer.
    pub hover: Color,
    /// The fill of the legend, the count card, the credit and the hover card.
    pub overlay: Color,
    /// The edge of those panels and of the card.
    pub border: Color,
    /// Text and the city dots.
    pub text: Color,
    /// Secondary text: the legend's title, the count's label, the card's labels.
    pub muted: Color,
    /// The legend's entries the filter draws nothing for.
    ///
    /// A legend entry is listed as long as the picture holds that type, so narrowing the
    /// filter leaves the other entries in place; they go to the disabled step rather than
    /// disappear, which is what says "this type is here, you are just not looking at it".
    pub disabled: Color,
    /// The ring a place name is drawn with.
    ///
    /// A name is drawn straight on the picture, and the vessel markers under it are the
    /// same dark as the letters. The outline is the map's own paper colour — the same
    /// overlay the panels use — so it reads as the fill showing through rather than as a
    /// second colour on the map.
    pub halo: Color,
}

impl MapColors {
    /// The colours the theme resolves for the appearance the frame is drawn in.
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
    use base_ui::theme::appearance::Appearance;

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

    /// The sea and the land must not be the same colour, or the map would be one blank
    /// fill: the coastline is what says where the water ends.
    #[test]
    fn the_sea_and_the_land_should_be_different_fills() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            assert_ne!(colors.ocean, colors.land, "{appearance:?}");
            assert_ne!(colors.text, colors.overlay, "{appearance:?}");
        }
    }

    /// The panel text sits on the overlay fill, so the two colours have to differ in every
    /// appearance for the legend and the hover card to be readable.
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
