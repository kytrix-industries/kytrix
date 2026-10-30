//! The AIS ship types the map distinguishes, and the colours it draws them in.
//!
//! AIS publishes one raw `ship_type` code per vessel. The code's main type is what the
//! map needs: a tanker, a cargo ship, a passenger ship and so on. The sub-codes that
//! qualify a main type — hazard grades and reserved values — are folded into it, because
//! a reader looking at a marker wants to know what kind of ship it is, not which hazard
//! grade its cargo carries. A code of zero means the report names no type at all, and a
//! vessel whose static record has not arrived has no code either; both are
//! [`ShipType::Unknown`].
//!
//! The legend and the header filter offer only the types the picture actually holds, so
//! the set is derived from the vessels rather than from this table. The table is what
//! gives each type its name, its stable key and its colour.
//!
//! # Colours
//!
//! The theme has six chart series, and the map distinguishes twenty ship types. A ship's
//! colour is a fact about its AIS type rather than about the theme, so the palette is
//! authored here, one per appearance: the light values are dark enough to read on the
//! paper land and the pale sea, and the dark values are light enough to read on the night
//! basemap. Every type has one colour in each appearance and no two types share one.

use base_ui::style::palette::Appearance;
use gpui_kit::{Hsla, hsla};

/// Every ship type the map distinguishes, in the order the legend lists them.
pub(super) const SHIP_TYPES: [ShipType; 20] = [
    ShipType::Tanker,
    ShipType::Cargo,
    ShipType::Passenger,
    ShipType::Fishing,
    ShipType::Towing,
    ShipType::Dredging,
    ShipType::Military,
    ShipType::Sailing,
    ShipType::Pleasure,
    ShipType::HighSpeed,
    ShipType::Pilot,
    ShipType::SearchAndRescue,
    ShipType::PortTender,
    ShipType::AntiPollution,
    ShipType::LawEnforcement,
    ShipType::Medical,
    ShipType::Noncombatant,
    ShipType::WingInGround,
    ShipType::Other,
    ShipType::Unknown,
];

/// How many ship types the map distinguishes.
pub(super) const SHIP_TYPE_COUNT: usize = SHIP_TYPES.len();

/// One AIS main type, or the absence of one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShipType {
    /// A tanker: the `80–89` range of `ship_type`.
    Tanker,
    /// A cargo ship: the `70–79` range. AIS does not distinguish container from bulk,
    /// so neither does the map.
    Cargo,
    /// A passenger ship: the `60–69` range.
    Passenger,
    /// A fishing vessel: `30`.
    Fishing,
    /// A tug or a vessel towing: `31`, `32` and `52`.
    Towing,
    /// Dredging or diving work: `33` and `34`.
    Dredging,
    /// A military vessel: `35`.
    Military,
    /// A sailing vessel: `36`.
    Sailing,
    /// A pleasure craft: `37`.
    Pleasure,
    /// A high-speed craft: the `40–49` range.
    HighSpeed,
    /// A pilot vessel: `50`.
    Pilot,
    /// A search-and-rescue vessel: `51`.
    SearchAndRescue,
    /// A port tender: `53`.
    PortTender,
    /// Anti-pollution equipment: `54`.
    AntiPollution,
    /// A law-enforcement vessel: `55`.
    LawEnforcement,
    /// A medical transport: `58`.
    Medical,
    /// A noncombatant ship: `59`.
    Noncombatant,
    /// A wing-in-ground craft: the `20–29` range.
    WingInGround,
    /// A type the table does not name: the reserved codes and the `90–99` range.
    Other,
    /// No type at all: `ship_type` zero, or no static record.
    Unknown,
}

impl ShipType {
    /// The type one raw AIS `ship_type` code belongs to.
    pub(super) const fn from_code(code: u32) -> Self {
        match code {
            0 => Self::Unknown,
            20..=29 => Self::WingInGround,
            30 => Self::Fishing,
            31 | 32 | 52 => Self::Towing,
            33 | 34 => Self::Dredging,
            35 => Self::Military,
            36 => Self::Sailing,
            37 => Self::Pleasure,
            40..=49 => Self::HighSpeed,
            50 => Self::Pilot,
            51 => Self::SearchAndRescue,
            53 => Self::PortTender,
            54 => Self::AntiPollution,
            55 => Self::LawEnforcement,
            58 => Self::Medical,
            59 => Self::Noncombatant,
            60..=69 => Self::Passenger,
            70..=79 => Self::Cargo,
            80..=89 => Self::Tanker,
            _ => Self::Other,
        }
    }

    /// The position this type occupies in [`SHIP_TYPES`], the legend and the palette.
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Tanker => 0,
            Self::Cargo => 1,
            Self::Passenger => 2,
            Self::Fishing => 3,
            Self::Towing => 4,
            Self::Dredging => 5,
            Self::Military => 6,
            Self::Sailing => 7,
            Self::Pleasure => 8,
            Self::HighSpeed => 9,
            Self::Pilot => 10,
            Self::SearchAndRescue => 11,
            Self::PortTender => 12,
            Self::AntiPollution => 13,
            Self::LawEnforcement => 14,
            Self::Medical => 15,
            Self::Noncombatant => 16,
            Self::WingInGround => 17,
            Self::Other => 18,
            Self::Unknown => 19,
        }
    }

    /// The stable identifier the filter's menu entries and its selection use.
    pub(super) const fn key(self) -> &'static str {
        KEYS[self.index()]
    }

    /// The catalogue key of this type's name.
    pub(super) const fn label_key(self) -> &'static str {
        LABEL_KEYS[self.index()]
    }

    /// The type one stable key names, or `None` for a key this build does not know.
    pub(super) fn from_key(key: &str) -> Option<Self> {
        SHIP_TYPES
            .iter()
            .copied()
            .find(|ship_type| ship_type.key() == key)
    }
}

/// What the map is restricted to.
///
/// The chosen type is kept even when the picture no longer holds it: the filter is the
/// reader's decision, and a type that leaves the picture leaves an empty sea rather than
/// silently widening the selection back to everything.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ShipTypeFilter {
    /// Every type the picture holds.
    #[default]
    All,
    /// One type, which may have left the picture since it was chosen.
    Only(ShipType),
}

impl ShipTypeFilter {
    /// The stable identifier the filter's menu entries and its selection use.
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only(ship_type) => ship_type.key(),
        }
    }

    /// The filter one stable key names, or `None` for a key this build does not know.
    pub(super) fn from_key(key: &str) -> Option<Self> {
        if key == Self::All.key() {
            return Some(Self::All);
        }
        ShipType::from_key(key).map(Self::Only)
    }

    /// Whether a vessel of `ship_type` is drawn under this filter.
    pub(super) fn matches(self, ship_type: ShipType) -> bool {
        match self {
            Self::All => true,
            Self::Only(selected) => selected == ship_type,
        }
    }
}

/// The stable keys of [`SHIP_TYPES`], in the same order.
const KEYS: [&str; SHIP_TYPE_COUNT] = [
    "tanker",
    "cargo",
    "passenger",
    "fishing",
    "towing",
    "dredging",
    "military",
    "sailing",
    "pleasure",
    "high_speed",
    "pilot",
    "search_and_rescue",
    "port_tender",
    "anti_pollution",
    "law_enforcement",
    "medical",
    "noncombatant",
    "wing_in_ground",
    "other",
    "unknown",
];

/// The catalogue keys of [`SHIP_TYPES`]'s names, in the same order.
const LABEL_KEYS: [&str; SHIP_TYPE_COUNT] = [
    "widget.global_shipping_chokepoint.ship_type.tanker",
    "widget.global_shipping_chokepoint.ship_type.cargo",
    "widget.global_shipping_chokepoint.ship_type.passenger",
    "widget.global_shipping_chokepoint.ship_type.fishing",
    "widget.global_shipping_chokepoint.ship_type.towing",
    "widget.global_shipping_chokepoint.ship_type.dredging",
    "widget.global_shipping_chokepoint.ship_type.military",
    "widget.global_shipping_chokepoint.ship_type.sailing",
    "widget.global_shipping_chokepoint.ship_type.pleasure",
    "widget.global_shipping_chokepoint.ship_type.high_speed",
    "widget.global_shipping_chokepoint.ship_type.pilot",
    "widget.global_shipping_chokepoint.ship_type.search_and_rescue",
    "widget.global_shipping_chokepoint.ship_type.port_tender",
    "widget.global_shipping_chokepoint.ship_type.anti_pollution",
    "widget.global_shipping_chokepoint.ship_type.law_enforcement",
    "widget.global_shipping_chokepoint.ship_type.medical",
    "widget.global_shipping_chokepoint.ship_type.noncombatant",
    "widget.global_shipping_chokepoint.ship_type.wing_in_ground",
    "widget.global_shipping_chokepoint.ship_type.other",
    "widget.global_shipping_chokepoint.ship_type.unknown",
];

/// One authored colour, with the hue in degrees.
const fn colour(hue: f32, saturation: f32, lightness: f32) -> Hsla {
    hsla(hue / 360.0, saturation, lightness, 1.0)
}

/// The colours the light appearance draws each type in, in [`SHIP_TYPES`] order.
///
/// The values are dark enough to stay legible on both the paper land and the pale sea.
static LIGHT: [Hsla; SHIP_TYPE_COUNT] = [
    colour(30.0, 0.82, 0.40),  // Tanker: amber
    colour(215.0, 0.70, 0.40), // Cargo: blue
    colour(275.0, 0.62, 0.42), // Passenger: violet
    colour(140.0, 0.68, 0.36), // Fishing: green
    colour(0.0, 0.72, 0.40),   // Towing: red
    colour(50.0, 0.78, 0.32),  // Dredging: yellow
    colour(350.0, 0.55, 0.32), // Military: dark red
    colour(190.0, 0.68, 0.38), // Sailing: cyan
    colour(320.0, 0.72, 0.40), // Pleasure: magenta
    colour(85.0, 0.78, 0.34),  // High-speed: yellow-green
    colour(240.0, 0.65, 0.42), // Pilot: blue-violet
    colour(15.0, 0.78, 0.40),  // Search and rescue: red-orange
    colour(170.0, 0.55, 0.36), // Port tender: teal
    colour(100.0, 0.58, 0.38), // Anti-pollution: green-yellow
    colour(225.0, 0.60, 0.32), // Law enforcement: dark blue
    colour(335.0, 0.55, 0.44), // Medical: pink
    colour(260.0, 0.35, 0.40), // Noncombatant: muted purple
    colour(120.0, 0.48, 0.42), // Wing in ground: light green
    colour(30.0, 0.18, 0.36),  // Other: brown-grey
    colour(0.0, 0.00, 0.44),   // Unknown: mid grey
];

/// The colours the dark appearance draws each type in, in [`SHIP_TYPES`] order.
///
/// The values are light enough to stay legible on the near-black sea and the dark land.
static DARK: [Hsla; SHIP_TYPE_COUNT] = [
    colour(30.0, 0.78, 0.76),
    colour(215.0, 0.66, 0.70),
    colour(275.0, 0.58, 0.78),
    colour(140.0, 0.64, 0.66),
    colour(0.0, 0.68, 0.74),
    colour(50.0, 0.74, 0.78),
    colour(350.0, 0.52, 0.62),
    colour(190.0, 0.64, 0.70),
    colour(320.0, 0.68, 0.76),
    colour(85.0, 0.74, 0.72),
    colour(240.0, 0.61, 0.78),
    colour(15.0, 0.74, 0.76),
    colour(170.0, 0.52, 0.68),
    colour(100.0, 0.54, 0.70),
    colour(225.0, 0.56, 0.62),
    colour(335.0, 0.51, 0.80),
    colour(260.0, 0.32, 0.74),
    colour(120.0, 0.44, 0.76),
    colour(30.0, 0.15, 0.66),
    colour(0.0, 0.00, 0.82),
];

/// The palette one appearance draws the ship types with.
pub(super) fn colours(appearance: Appearance) -> &'static [Hsla; SHIP_TYPE_COUNT] {
    match appearance {
        Appearance::Light => &LIGHT,
        Appearance::Dark => &DARK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ais_code_should_fall_into_its_main_type() {
        for (code, expected) in [
            (0, ShipType::Unknown),
            (1, ShipType::Other),
            (19, ShipType::Other),
            (20, ShipType::WingInGround),
            (29, ShipType::WingInGround),
            (30, ShipType::Fishing),
            (31, ShipType::Towing),
            (32, ShipType::Towing),
            (33, ShipType::Dredging),
            (34, ShipType::Dredging),
            (35, ShipType::Military),
            (36, ShipType::Sailing),
            (37, ShipType::Pleasure),
            (38, ShipType::Other),
            (39, ShipType::Other),
            (40, ShipType::HighSpeed),
            (49, ShipType::HighSpeed),
            (50, ShipType::Pilot),
            (51, ShipType::SearchAndRescue),
            (52, ShipType::Towing),
            (53, ShipType::PortTender),
            (54, ShipType::AntiPollution),
            (55, ShipType::LawEnforcement),
            (56, ShipType::Other),
            (57, ShipType::Other),
            (58, ShipType::Medical),
            (59, ShipType::Noncombatant),
            (60, ShipType::Passenger),
            (69, ShipType::Passenger),
            (70, ShipType::Cargo),
            (79, ShipType::Cargo),
            (80, ShipType::Tanker),
            (89, ShipType::Tanker),
            (90, ShipType::Other),
            (99, ShipType::Other),
            (100, ShipType::Other),
            (u32::MAX, ShipType::Other),
        ] {
            assert_eq!(ShipType::from_code(code), expected, "code {code}");
        }
    }

    #[test]
    fn every_type_should_keep_its_place_in_the_palette_and_the_legend() {
        for (index, ship_type) in SHIP_TYPES.iter().enumerate() {
            assert_eq!(ship_type.index(), index, "{ship_type:?}");
        }
    }

    #[test]
    fn every_type_should_have_a_colour_of_its_own_in_each_appearance() {
        for palette in [&LIGHT, &DARK] {
            for (first, first_colour) in palette.iter().enumerate() {
                for (second, second_colour) in palette.iter().enumerate().skip(first + 1) {
                    assert_ne!(
                        first_colour, second_colour,
                        "types {first} and {second} are drawn alike",
                    );
                }
            }
        }
    }

    #[test]
    fn the_two_appearances_should_draw_different_colours() {
        for (index, (light, dark)) in LIGHT.iter().zip(&DARK).enumerate() {
            assert_ne!(
                light, dark,
                "type {index} draws one colour in both appearances"
            );
        }
    }

    #[test]
    fn every_key_should_round_trip() {
        for ship_type in SHIP_TYPES {
            assert_eq!(ShipType::from_key(ship_type.key()), Some(ship_type));
            assert_eq!(
                ShipTypeFilter::from_key(ship_type.key()),
                Some(ShipTypeFilter::Only(ship_type)),
            );
            assert_eq!(ShipTypeFilter::Only(ship_type).key(), ship_type.key());
        }

        assert_eq!(ShipTypeFilter::from_key("all"), Some(ShipTypeFilter::All));
        assert_eq!(ShipTypeFilter::All.key(), "all");
        assert_eq!(ShipTypeFilter::from_key("nonsense"), None);
    }

    #[test]
    fn the_filter_should_match_only_its_type() {
        assert!(ShipTypeFilter::All.matches(ShipType::Tanker));
        assert!(ShipTypeFilter::All.matches(ShipType::Unknown));
        assert!(ShipTypeFilter::Only(ShipType::Cargo).matches(ShipType::Cargo));
        assert!(!ShipTypeFilter::Only(ShipType::Cargo).matches(ShipType::Tanker));
    }

    #[test]
    fn every_label_key_should_be_unique() {
        for (first, first_key) in LABEL_KEYS.iter().enumerate() {
            for second_key in LABEL_KEYS.iter().skip(first + 1) {
                assert_ne!(first_key, second_key, "two types share a label key");
            }
        }
    }
}
