//! What a pane drawing the Strait of Hormuz knows, and how it is drawn.
//!
//! The map is largely fixed — a coastline, a lane sketch, nineteen labels — and the
//! only part that moves is the vessel layer. This module owns the fixed half of that
//! picture (bounds, projection, label anchors, lanes), the four buckets vessels are
//! coloured by, the flag-state table, the camera and its clamping, and the store one
//! AIS subscription fills. None of it needs a GPUI context: a colour is read from the
//! resolved theme and a push is applied without notifying anything. The frames those
//! changes are drawn in belong to the view.
//!
//! # A picture held between pushes
//!
//! The service publishes deltas, so the client holds the state: a keyframe replaces
//! it, and a delta is applied only when it continues the exact picture already held —
//! the same server epoch and the exact sequence applied last. A push that does not
//! match is not guessed at; the store notes a gap and the next keyframe heals it.
//! Staleness is derived from that gap and the local clock rather than written into a
//! field by a timer: a client in a gap keeps receiving deltas it must ignore, so a
//! timer would keep resetting the badge into silence while the screen stayed frozen.
//!
//! # Text
//!
//! The values a hover card reads are formatted here so the formatting is covered by
//! unit tests instead of by tests that have to build a view. A missing value or a
//! sentinel stays `None` and the view draws its own "unavailable" text; nothing in
//! this module invents a sentence. Unit symbols (`m`, `kn`, `°`) travel with their
//! value, and every natural-language word stays in the catalogue.

use std::time::{Duration, Instant};

use ahash::AHashMap;
use base_ui::style::theme::Theme;
use gpui_kit::Hsla;
use kytrix_service_provider::service::global_shipping_chokepoint::{
    GlobalShippingChokepointPosition, GlobalShippingChokepointStatic,
    GlobalShippingChokepointUpdate,
};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

/// The west edge of the map, in degrees east.
pub(super) const MAP_WEST: f32 = 48.0;
/// The east edge of the map, in degrees east.
pub(super) const MAP_EAST: f32 = 62.0;
/// The south edge of the map, in degrees north.
pub(super) const MAP_SOUTH: f32 = 21.5;
/// The north edge of the map, in degrees north.
pub(super) const MAP_NORTH: f32 = 28.5;

/// The most vessels one picture holds.
///
/// The service picks which vessels those are, by distance; this is only the bound a
/// keyframe is checked against, because a client that kept every vessel it ever saw
/// would accumulate a set the service no longer holds.
pub(super) const MAX_VESSELS: usize = 1_000;

/// The furthest out the camera goes: the whole map fitted to the viewport.
pub(super) const MIN_ZOOM: f32 = 1.0;
/// The furthest in the camera goes.
pub(super) const MAX_ZOOM: f32 = 4.0;

/// How far a pointer may be from a vessel and still hit it, in pixels.
pub(super) const HIT_RADIUS_PX: f32 = 8.0;

/// How long a picture may go without a push and still be called live.
///
/// The service sends an empty delta every second, so silence means a broken link
/// rather than an empty strait.
pub(super) const STALE_AFTER: Duration = Duration::from_secs(30);

/// The speed-over-ground value that means "not available".
const SOG_UNAVAILABLE: u32 = 1_023;
/// The course-over-ground value that means "not available".
const COG_UNAVAILABLE: u32 = 3_600;
/// The heading value that means "not available".
const HEADING_UNAVAILABLE: u32 = 511;
/// The first navigation status that means "undefined"; the catalogue names 0–14.
const NAV_STATUS_UNDEFINED: u8 = 15;

/// The lanes the map sketches, as lines of `(longitude, latitude)` in degrees.
///
/// They are a schematic of the traffic separation scheme's direction of travel, not
/// a surveyed chart boundary: each line is the middle of a lane and the map dashes
/// it. The first two run the length of the strait in opposite directions, the third
/// is the Bandar Abbas approach and the fourth the Fujairah anchorage approach.
pub(super) const TSS_LANES: &[&[(f32, f32)]] = &[
    &[
        (57.35, 25.15),
        (56.72, 25.85),
        (56.35, 26.38),
        (55.95, 26.62),
        (55.30, 26.78),
    ],
    &[
        (55.10, 26.68),
        (55.65, 26.50),
        (56.15, 26.45),
        (56.55, 26.10),
        (57.10, 25.55),
        (57.75, 25.05),
    ],
    &[(56.30, 27.12), (56.33, 26.80), (56.38, 26.55)],
    &[(56.34, 25.15), (56.42, 25.58), (56.48, 25.98)],
];

/// One label the map draws: the name it reads in the catalogue and where it sits.
#[derive(Clone, Copy, Debug)]
pub(super) struct MapLabel {
    /// The catalogue key, e.g. `widget.global_shipping_chokepoint.label.city.fujairah`.
    pub key: &'static str,
    /// The anchor, as longitude and latitude in degrees.
    pub at: [f32; 2],
}

/// The five countries the map names.
pub(super) const COUNTRY_LABELS: [MapLabel; 5] = [
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.country.iran",
        at: [53.2, 27.85],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.country.oman",
        at: [56.6, 22.9],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.country.uae",
        at: [54.4, 23.6],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.country.qatar",
        at: [51.2, 25.15],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.country.bahrain",
        at: [50.55, 26.05],
    },
];

/// The twelve cities the map names.
pub(super) const CITY_LABELS: [MapLabel; 12] = [
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.bandar_abbas",
        at: [56.27, 27.18],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.qeshm",
        at: [55.95, 26.95],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.bandar_lengeh",
        at: [55.45, 26.55],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.khasab",
        at: [56.25, 26.18],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.fujairah",
        at: [56.35, 25.12],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.ras_al_khaimah",
        at: [55.95, 25.79],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.sharjah",
        at: [55.40, 25.35],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.al_jubayl",
        at: [49.65, 27.00],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.al_hofuf",
        at: [49.59, 25.36],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.chabahar",
        at: [60.62, 25.29],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.muscat",
        at: [58.40, 23.60],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.city.sur",
        at: [59.53, 22.57],
    },
];

/// The two bodies of water the map names.
pub(super) const WATER_LABELS: [MapLabel; 2] = [
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.water.persian_gulf",
        at: [51.5, 26.6],
    },
    MapLabel {
        key: "widget.global_shipping_chokepoint.label.water.gulf_of_oman",
        at: [58.6, 24.9],
    },
];

/// Projects longitude and latitude in degrees to spherical Web Mercator.
///
/// The map is drawn in this projection, so a vessel's position is converted when the
/// painted list is built and the frame only applies the camera's affine transform.
pub(super) fn project(longitude: f32, latitude: f32) -> [f32; 2] {
    [
        longitude.to_radians(),
        (std::f32::consts::FRAC_PI_4 + latitude.to_radians() / 2.0)
            .tan()
            .ln(),
    ]
}

/// The map's Mercator extent, as `[west, south, east, north]`.
pub(super) fn mercator_bounds() -> [f32; 4] {
    let [west, south] = project(MAP_WEST, MAP_SOUTH);
    let [east, north] = project(MAP_EAST, MAP_NORTH);
    [west, south, east, north]
}

/// The pixels one Mercator unit is drawn at when the whole map is fitted.
///
/// The camera's scale is this times [`Camera::zoom`]. A viewport with no area — a
/// pane laid out at zero size, before its first frame — has no fit at all and reads
/// as zero, which the camera treats as the degenerate viewport it is.
pub(super) fn fit_px_per_merc(viewport: [f32; 2]) -> f32 {
    let [width, height] = viewport;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return 0.0;
    }

    let [west, south, east, north] = mercator_bounds();
    (width / (east - west)).min(height / (north - south))
}

/// Where the map is looking, and how far in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Camera {
    /// The Mercator point drawn at the centre of the viewport.
    pub center: [f32; 2],
    /// How far the fitted map is scaled up, between [`MIN_ZOOM`] and [`MAX_ZOOM`].
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            center: map_centre(),
            zoom: MIN_ZOOM,
        }
    }
}

impl Camera {
    /// The camera kept on the map for a viewport of `viewport` pixels.
    ///
    /// An axis the map is narrower than is centred on the map rather than left free
    /// to drag into empty space; an axis the map fills is clamped so the map's edge
    /// never comes inside the viewport. A viewport with no fit at all (see
    /// [`fit_px_per_merc`]) centres the whole map. Zoom is clamped here because it is
    /// the one field a scroll wheel writes.
    pub fn clamped(self, viewport: [f32; 2]) -> Self {
        let zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let scale = fit_px_per_merc(viewport) * zoom;
        let mut camera = Self {
            center: self.center,
            zoom,
        };

        if !scale.is_finite() || scale <= 0.0 {
            camera.center = map_centre();
            return camera;
        }

        let [west, south, east, north] = mercator_bounds();
        let half = [viewport[0] / (2.0 * scale), viewport[1] / (2.0 * scale)];
        camera.center = [
            clamp_center(camera.center[0], half[0], west, east),
            clamp_center(camera.center[1], half[1], south, north),
        ];
        camera
    }
}

/// The centre of the map, in Mercator coordinates.
fn map_centre() -> [f32; 2] {
    let [west, south, east, north] = mercator_bounds();
    [(west + east) / 2.0, (south + north) / 2.0]
}

/// One axis of [`Camera::clamped`]: centred when the map is narrower than the view.
fn clamp_center(center: f32, half: f32, low: f32, high: f32) -> f32 {
    let (low_edge, high_edge) = (low + half, high - half);
    if low_edge >= high_edge {
        (low + high) / 2.0
    } else {
        center.clamp(low_edge, high_edge)
    }
}

/// The four buckets vessels are drawn in.
///
/// AIS has no authoritative container/bulk split: `ship_type` says `70–79` is cargo
/// and nothing more. The split below is therefore a display approximation —
/// `70–74` read as container, `75–79` as bulk, `80–89` as tanker, everything else
/// (including the "not available" zero) as other — chosen so that every bucket is
/// reachable from a real report, which is what the four colours and every legend
/// entry need.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ShipClass {
    /// A tanker: the `80–89` range of `ship_type`.
    Tanker,
    /// A container ship: the `70–74` range of `ship_type`.
    Container,
    /// A bulk carrier: the `75–79` range of `ship_type`.
    Bulk,
    /// Everything else, including a report that names no type at all.
    Other,
}

/// The bucket one AIS `ship_type` is drawn in.
pub(super) const fn class_of(ship_type: u32) -> ShipClass {
    match ship_type {
        70..=74 => ShipClass::Container,
        75..=79 => ShipClass::Bulk,
        80..=89 => ShipClass::Tanker,
        _ => ShipClass::Other,
    }
}

/// The colour one class is drawn in, read from the theme the frame is drawn with.
///
/// Nothing is cached: the appearance can change between frames, and a cached colour
/// is a pane that keeps drawing the previous theme's ships.
pub(super) fn colour_for(theme: &Theme, class: ShipClass) -> Hsla {
    let series = &theme.roles.chart.series;
    match class {
        ShipClass::Tanker => series[0].hsla,
        ShipClass::Container => series[1].hsla,
        ShipClass::Bulk => series[2].hsla,
        ShipClass::Other => series[4].hsla,
    }
}

/// The MID-to-flag table, ordered by MID so a lookup is a binary search.
///
/// A MID is the first three digits of an MMSI and names the flag state the vessel is
/// registered in. The table holds the five coastal states of the strait, their
/// neighbours on the Gulf, and the open registries and large merchant fleets a
/// strait's traffic actually flies. It is kept sorted, and a test pins that.
const FLAGS: &[(u32, &str)] = &[
    (205, "BE"),
    (209, "CY"),
    (210, "CY"),
    (211, "DE"),
    (212, "CY"),
    (215, "MT"),
    (218, "DE"),
    (219, "DK"),
    (220, "DK"),
    (224, "ES"),
    (225, "ES"),
    (226, "FR"),
    (227, "FR"),
    (228, "FR"),
    (229, "MT"),
    (230, "FI"),
    (231, "FO"),
    (232, "IM"),
    (233, "GB"),
    (234, "GB"),
    (235, "GB"),
    (236, "GI"),
    (237, "GR"),
    (238, "HR"),
    (239, "GR"),
    (240, "GR"),
    (241, "GR"),
    (244, "NL"),
    (245, "NL"),
    (246, "NL"),
    (247, "IT"),
    (248, "MT"),
    (249, "MT"),
    (250, "IE"),
    (251, "IS"),
    (256, "MT"),
    (257, "NO"),
    (258, "NO"),
    (259, "NO"),
    (261, "PL"),
    (263, "PT"),
    (265, "SE"),
    (266, "SE"),
    (269, "CH"),
    (271, "TR"),
    (272, "UA"),
    (273, "RU"),
    (275, "LV"),
    (276, "EE"),
    (277, "LT"),
    (303, "US"),
    (304, "AG"),
    (305, "AG"),
    (308, "BS"),
    (309, "BS"),
    (310, "BM"),
    (311, "BS"),
    (316, "CA"),
    (319, "KY"),
    (338, "US"),
    (345, "MX"),
    (351, "PA"),
    (352, "PA"),
    (353, "PA"),
    (354, "PA"),
    (355, "PA"),
    (356, "PA"),
    (357, "PA"),
    (366, "US"),
    (367, "US"),
    (368, "US"),
    (369, "US"),
    (370, "PA"),
    (371, "PA"),
    (372, "PA"),
    (373, "PA"),
    (403, "SA"),
    (405, "BD"),
    (408, "BH"),
    (412, "CN"),
    (413, "CN"),
    (414, "CN"),
    (416, "TW"),
    (417, "LK"),
    (419, "IN"),
    (422, "IR"),
    (423, "AZ"),
    (425, "IQ"),
    (428, "IL"),
    (431, "JP"),
    (432, "JP"),
    (436, "KZ"),
    (440, "KR"),
    (441, "KR"),
    (445, "KP"),
    (447, "KW"),
    (450, "LB"),
    (455, "MV"),
    (461, "OM"),
    (463, "PK"),
    (466, "QA"),
    (470, "AE"),
    (473, "YE"),
    (477, "HK"),
    (503, "AU"),
    (512, "NZ"),
    (525, "ID"),
    (533, "MY"),
    (538, "MH"),
    (548, "PH"),
    (563, "SG"),
    (564, "SG"),
    (565, "SG"),
    (566, "SG"),
    (567, "TH"),
    (574, "VN"),
    (576, "VU"),
    (601, "ZA"),
    (603, "AO"),
    (605, "DZ"),
    (622, "EG"),
    (636, "LR"),
    (637, "LR"),
    (642, "LY"),
    (645, "MU"),
    (657, "NG"),
    (663, "SN"),
    (664, "SC"),
    (672, "TN"),
];

/// The flag state one MID names, as its two-letter ISO 3166-1 code.
///
/// The code rather than a flag emoji: Windows draws a regional-indicator pair as two
/// letter boxes rather than as a flag, so the code is what every platform shows and
/// it is what a maritime reader expects beside a vessel. `0` (the report names no
/// state) and a MID the table does not know both read as `None`.
pub(super) fn flag_for_mid(mid: u32) -> Option<&'static str> {
    FLAGS
        .binary_search_by_key(&mid, |(candidate, _)| *candidate)
        .ok()
        .map(|index| FLAGS[index].1)
}

/// One AIS navigation status, as the index the catalogue names it by.
///
/// The catalogue has one key per status (`widget.global_shipping_chokepoint.nav_status.0`
/// through `.14`), and the value the wire carries picks it; no status text lives
/// here.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct NavStatus(u8);

impl NavStatus {
    /// The index the catalogue's key is spelled with.
    pub const fn index(self) -> u8 {
        self.0
    }
}

/// The navigation status one position report names.
///
/// `15` means the report does not know, and any value the table does not define is
/// treated the same way.
pub(super) fn nav_status_of(nav_status: u32) -> Option<NavStatus> {
    u8::try_from(nav_status)
        .ok()
        .filter(|status| *status < NAV_STATUS_UNDEFINED)
        .map(NavStatus)
}

/// One vessel the map paints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Vessel {
    /// The identity its position and its static part are joined on.
    pub mmsi: u32,
    /// The bucket its ship type falls in, or [`ShipClass::Other`] while the static
    /// part has not arrived.
    pub class: ShipClass,
    /// Its last position, in Mercator coordinates.
    pub position: [f32; 2],
    /// Course over ground in 0.1°, as the wire carries it; the marker turns by it and
    /// falls back to the heading when it is at its sentinel.
    pub cog: u32,
    /// True heading in degrees, as the wire carries it.
    pub heading: u32,
    /// Whether the static part has arrived. A vessel without one is still painted —
    /// as an unknown — because its position is enough to draw a dot.
    pub has_static: bool,
}

impl Vessel {
    /// The direction the marker points, in degrees clockwise from north.
    ///
    /// A course over ground that is at its sentinel falls back to the true heading, and
    /// a heading at its own sentinel reads as north: a report that knows neither is
    /// still drawn, facing the way a report that omitted them would.
    pub(super) fn facing(self) -> f32 {
        if self.cog != COG_UNAVAILABLE {
            self.cog as f32 / 10.0
        } else if self.heading != HEADING_UNAVAILABLE {
            self.heading as f32
        } else {
            0.0
        }
    }
}

/// The fifteen values a hover card reads, one field per catalogue key.
///
/// The card's labels and its "unavailable" text belong to the view: a field is `None`
/// when the report does not carry the value, either because its sentinel says so or
/// because the static part has not arrived. The order of the fields is the order the
/// design lists the items in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VesselDetails {
    /// 1. The name, or the MMSI when the report carries an empty one; `None` before
    ///    the static part arrives.
    pub name: Option<SmolStr>,
    /// 2. The MMSI, which every vessel has.
    pub mmsi: SmolStr,
    /// 3. The IMO number, or `None` when the report carries zero.
    pub imo: Option<SmolStr>,
    /// 4. The call sign, or `None` when it is empty.
    pub call_sign: Option<SmolStr>,
    /// 5. The bucket the ship type falls in, or `None` when it names no type.
    pub ship_class: Option<ShipClass>,
    /// 6. The flag state's two-letter code, or `None` when the MID names none.
    pub flag: Option<&'static str>,
    /// 7. The destination, or `None` when it is empty.
    pub destination: Option<SmolStr>,
    /// 8. The estimated time of arrival as `MM-DD HH:MM`, or `None` when all four
    ///    fields are zero.
    pub eta: Option<SmolStr>,
    /// 9. The draught in metres, or `None` when it is zero.
    pub draught: Option<SmolStr>,
    /// 10. Length × breadth in metres, or `None` when all four dimensions are zero.
    pub dimensions: Option<SmolStr>,
    /// 11. Speed over ground in knots, or `None` at the sentinel.
    pub speed: Option<SmolStr>,
    /// 12. Course over ground in degrees, or `None` at the sentinel.
    pub course: Option<SmolStr>,
    /// 13. True heading in degrees, or `None` at the sentinel.
    pub heading: Option<SmolStr>,
    /// 14. The navigation status, or `None` when the report says it does not know.
    pub nav_status: Option<NavStatus>,
    /// 15. The server's receipt time of the position, in the pane's data zone, or
    ///     `None` when the report carries zero.
    pub updated: Option<SmolStr>,
}

impl VesselDetails {
    fn new(
        mmsi: u32,
        statics: Option<&GlobalShippingChokepointStatic>,
        position: &GlobalShippingChokepointPosition,
        zone: UtcOffset,
    ) -> Self {
        Self {
            name: statics.map(|statics| {
                if statics.name.is_empty() {
                    format_smolstr!("{mmsi}")
                } else {
                    statics.name.clone()
                }
            }),
            mmsi: format_smolstr!("{mmsi}"),
            imo: statics
                .and_then(|statics| (statics.imo != 0).then(|| format_smolstr!("{}", statics.imo))),
            call_sign: statics.and_then(|statics| {
                (!statics.call_sign.is_empty()).then(|| statics.call_sign.clone())
            }),
            ship_class: statics
                .and_then(|statics| (statics.ship_type != 0).then(|| class_of(statics.ship_type))),
            flag: statics.and_then(|statics| flag_for_mid(statics.flag_mid)),
            destination: statics.and_then(|statics| {
                (!statics.destination.is_empty()).then(|| statics.destination.clone())
            }),
            eta: statics.and_then(|statics| {
                let carries_eta = statics.eta_month != 0
                    || statics.eta_day != 0
                    || statics.eta_hour != 0
                    || statics.eta_minute != 0;
                carries_eta.then(|| {
                    format_smolstr!(
                        "{:02}-{:02} {:02}:{:02}",
                        statics.eta_month,
                        statics.eta_day,
                        statics.eta_hour,
                        statics.eta_minute
                    )
                })
            }),
            draught: statics.and_then(|statics| {
                (statics.draught != 0)
                    .then(|| format_smolstr!("{}.{} m", statics.draught / 10, statics.draught % 10))
            }),
            dimensions: statics.and_then(|statics| {
                let length = statics.dim_to_bow.saturating_add(statics.dim_to_stern);
                let breadth = statics.dim_to_port.saturating_add(statics.dim_to_starboard);
                (length != 0 || breadth != 0).then(|| format_smolstr!("{length} × {breadth} m"))
            }),
            speed: (position.sog != SOG_UNAVAILABLE)
                .then(|| format_smolstr!("{}.{} kn", position.sog / 10, position.sog % 10)),
            course: (position.cog != COG_UNAVAILABLE)
                .then(|| format_smolstr!("{}.{}°", position.cog / 10, position.cog % 10)),
            heading: (position.heading != HEADING_UNAVAILABLE)
                .then(|| format_smolstr!("{}°", position.heading)),
            nav_status: nav_status_of(position.nav_status),
            updated: format_last_update(position.last_update, zone),
        }
    }
}

/// The receipt time as an absolute instant in `zone`, or `None` for no time at all.
///
/// An instant the calendar cannot hold — the far end of the wire's range moved past
/// the last day a date can express — reads as unavailable rather than as a date that
/// is not the one the service meant.
fn format_last_update(last_update: i64, zone: UtcOffset) -> Option<SmolStr> {
    if last_update == 0 {
        return None;
    }

    OffsetDateTime::from_unix_timestamp_nanos(i128::from(last_update) * 1_000_000)
        .ok()?
        .checked_to_offset(zone)
        .map(|time| {
            format_smolstr!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                time.year(),
                u8::from(time.month()),
                time.day(),
                time.hour(),
                time.minute(),
                time.second()
            )
        })
}

/// The picture one AIS subscription holds.
///
/// The two tables are the wire's two messages, keyed by MMSI, and they are joined
/// only when a frame paints them. `epoch` and `applied` are the baseline a delta has
/// to continue; `gap` says a push did not continue it, so what is held stays on the map
/// under the stale badge until a keyframe replaces it. `has_state` is what the pane reports as "there is a picture
/// to draw" — a picture cleared by a keyframe the server stamped with no time is not
/// one.
#[derive(Debug)]
pub(super) struct VesselStore {
    statics: AHashMap<u32, GlobalShippingChokepointStatic>,
    positions: AHashMap<u32, GlobalShippingChokepointPosition>,
    epoch: u64,
    applied: u64,
    gap: bool,
    received_at: Instant,
    has_state: bool,
}

impl Default for VesselStore {
    fn default() -> Self {
        Self::new()
    }
}

impl VesselStore {
    /// An empty picture, waiting for its first keyframe.
    pub fn new() -> Self {
        Self {
            statics: AHashMap::with_capacity(MAX_VESSELS),
            positions: AHashMap::with_capacity(MAX_VESSELS),
            epoch: 0,
            applied: 0,
            gap: false,
            received_at: Instant::now(),
            has_state: false,
        }
    }

    /// Applies one push, or notes that it cannot be applied.
    ///
    /// The four rules of the delta model, none of which may be dropped:
    ///
    /// 1. A keyframe replaces the whole picture, whatever arrived before it. One the
    ///    server stamped with no time means "I hold no state yet", not "the strait is
    ///    empty": it clears the picture without becoming a baseline, so the pane stays
    ///    in `Connecting` instead of drawing an empty sea.
    /// 2. A delta is applied only when it continues the picture already held: the
    ///    same server epoch and the exact sequence applied last. A delta that arrives
    ///    after a restart would otherwise be laid over state the process that
    ///    produced it never saw.
    /// 3. A delta that does not continue the picture only notes the gap — nothing is
    ///    guessed at and nothing is applied. What is already held stays on the map
    ///    under the stale badge, and the next keyframe replaces it. A new epoch also
    ///    drops the applied sequence, because it belonged to the process that no
    ///    longer exists.
    /// 4. Staleness is derived from the gap by [`is_stale`](Self::is_stale); it is
    ///    never written by a timer.
    ///
    /// `removed` deletes each listed MMSI from both tables, and a keyframe is checked
    /// against [`MAX_VESSELS`]. Deltas are not trimmed: which vessels the picture
    /// holds is the service's decision, and a client that trimmed its own set would
    /// drift from the set the server is tracking.
    pub fn apply(&mut self, update: &GlobalShippingChokepointUpdate) {
        if update.keyframe {
            assert!(
                update.positions.len() <= MAX_VESSELS,
                "a keyframe holds at most {MAX_VESSELS} positions",
            );
            self.statics.clear();
            self.positions.clear();
            self.epoch = update.epoch;

            // A keyframe the server stamped with no time says it holds no state yet,
            // not that the strait is empty: the picture stays cleared and no baseline
            // is established, so the pane keeps waiting instead of drawing an empty
            // sea. It is also not a usable message, so it does not refresh the clock
            // the staleness predicate reads.
            if update.timestamp == 0 {
                self.applied = 0;
                self.gap = true;
                self.has_state = false;
                return;
            }

            self.statics.extend(
                update
                    .statics
                    .iter()
                    .map(|entry| (entry.mmsi, entry.clone())),
            );
            self.positions
                .extend(update.positions.iter().map(|entry| (entry.mmsi, *entry)));
            self.applied = update.sequence;
            self.gap = false;
            self.has_state = true;
            self.received_at = Instant::now();
            return;
        }

        if update.epoch == self.epoch && update.base == self.applied {
            for statics in &update.statics {
                self.statics.insert(statics.mmsi, statics.clone());
            }
            for position in &update.positions {
                self.positions.insert(position.mmsi, *position);
            }
            for mmsi in &update.removed {
                self.statics.remove(mmsi);
                self.positions.remove(mmsi);
            }
            self.applied = update.sequence;
            self.gap = false;
            self.has_state = true;
        } else {
            // The picture already held is deliberately not cleared: it is what the
            // pane keeps drawing, under the stale badge, until a keyframe replaces
            // it. `gap` is that badge's other half, and the derivation in `is_stale`
            // is why no timer can hide a gap that outlives one message.
            self.gap = true;
            if update.epoch != self.epoch {
                self.applied = 0;
            }
        }

        // An ignored delta still proves the link is alive, which is exactly what the
        // staleness predicate asks about, so it refreshes the clock as well.
        self.received_at = Instant::now();
    }

    /// Whether a usable picture has arrived, which is what the pane has content from.
    pub fn has_state(&self) -> bool {
        self.has_state
    }

    /// Whether the picture is no longer live.
    ///
    /// Derived, not stored: a gap is a picture that stopped being continued, and the
    /// clock is local because the server's timestamps are not to be compared with it.
    pub fn is_stale(&self) -> bool {
        self.gap || self.received_at.elapsed() > STALE_AFTER
    }

    /// Whether a push did not continue the picture, which is what the stale badge names
    /// when the link is still delivering.
    pub fn has_gap(&self) -> bool {
        self.gap
    }

    /// How long the store has been silent, which is what the stale badge counts once the
    /// whole window has gone by without a push.
    pub fn silent_for(&self) -> Duration {
        self.received_at.elapsed()
    }

    /// How many vessels the picture holds.
    pub fn vessel_count(&self) -> usize {
        self.positions.len()
    }

    /// Empties the picture and resets the baseline with it.
    ///
    /// Resetting `epoch` and `applied` is what keeps a late delta of the session
    /// being abandoned from being applied: it no longer continues anything this
    /// store holds. A store that was cleared is not stale — it is empty — so the gap
    /// goes back to its initial value too.
    pub fn clear(&mut self) {
        self.statics.clear();
        self.positions.clear();
        self.epoch = 0;
        self.applied = 0;
        self.gap = false;
        self.has_state = false;
        self.received_at = Instant::now();
    }

    /// Every vessel that has a position, joined with its static part by MMSI.
    ///
    /// A vessel whose static part has not arrived is still painted — as an unknown
    /// dot — while one that has only a static part is not painted at all, because
    /// there is nowhere to paint it. The order is the store's, not a ranking.
    ///
    /// Building the list projects every position, so a painter keeps it between
    /// pushes and rebuilds it when one arrives.
    pub fn vessels(&self) -> Vec<Vessel> {
        let mut vessels = Vec::with_capacity(self.positions.len());
        for position in self.positions.values() {
            let statics = self.statics.get(&position.mmsi);
            vessels.push(Vessel {
                mmsi: position.mmsi,
                class: statics.map_or(ShipClass::Other, |statics| class_of(statics.ship_type)),
                position: project(
                    position.longitude as f32 / 1_000_000.0,
                    position.latitude as f32 / 1_000_000.0,
                ),
                cog: position.cog,
                heading: position.heading,
                has_static: statics.is_some(),
            });
        }
        vessels
    }

    /// What the hover card reads for one vessel, or `None` when it is not on the map.
    ///
    /// Resolved on the frame it is drawn, so a vessel the picture no longer holds
    /// stops having a card; `zone` is the data time zone of that frame.
    pub fn display(&self, mmsi: u32, zone: UtcOffset) -> Option<VesselDetails> {
        let position = self.positions.get(&mmsi)?;
        Some(VesselDetails::new(
            position.mmsi,
            self.statics.get(&mmsi),
            position,
            zone,
        ))
    }
}

/// The vessel nearest the pointer, when one is close enough to hit.
///
/// A linear scan over the painted points is cheaper than keeping an index for the
/// thousand a picture holds. Distances are compared squared, and a point exactly at
/// [`HIT_RADIUS_PX`] does not count: the radius is "less than", which is what a
/// pointer at the outer edge of a marker means.
pub(super) fn hit_test(points: &[(u32, [f32; 2])], at: [f32; 2]) -> Option<u32> {
    let radius = HIT_RADIUS_PX * HIT_RADIUS_PX;
    points
        .iter()
        .filter_map(|(mmsi, point)| {
            let dx = point[0] - at[0];
            let dy = point[1] - at[1];
            let distance = dx * dx + dy * dy;
            (distance < radius).then_some((distance, *mmsi))
        })
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, mmsi)| mmsi)
}

#[cfg(test)]
mod tests {
    use base_ui::style::palette::Appearance;

    use super::*;

    /// The west edge of the area the service reports positions for, in degrees east.
    ///
    /// It belongs to the service's subscription rather than to this module: the map draws
    /// no boundary for it, so it is only the rectangle the fixtures below are held to.
    const AIS_WEST: f32 = 53.5;
    /// The east edge of that area.
    const AIS_EAST: f32 = 59.0;
    /// The south edge of that area.
    const AIS_SOUTH: f32 = 24.0;
    /// The north edge of that area.
    const AIS_NORTH: f32 = 28.3;

    /// One position in the strait, with every value a report can carry.
    fn position(mmsi: u32) -> GlobalShippingChokepointPosition {
        GlobalShippingChokepointPosition {
            mmsi,
            latitude: 26_500_000,
            longitude: 56_300_000,
            sog: 123,
            cog: 456,
            heading: 87,
            nav_status: 1,
            last_update: 1_700_000_000_000,
        }
    }

    /// One static part, with every value a report can carry.
    fn statics(mmsi: u32) -> GlobalShippingChokepointStatic {
        GlobalShippingChokepointStatic {
            mmsi,
            imo: 9_074_729,
            call_sign: SmolStr::new_static("A8AB2"),
            name: SmolStr::new_static("SEA JEWEL"),
            destination: SmolStr::new_static("FUJAIRAH"),
            ship_type: 80,
            flag_mid: 636,
            draught: 92,
            dim_to_bow: 120,
            dim_to_stern: 30,
            dim_to_port: 15,
            dim_to_starboard: 17,
            eta_month: 3,
            eta_day: 14,
            eta_hour: 9,
            eta_minute: 5,
        }
    }

    /// A static part that carries nothing, as a report with every sentinel set.
    fn empty_statics(mmsi: u32) -> GlobalShippingChokepointStatic {
        GlobalShippingChokepointStatic {
            mmsi,
            imo: 0,
            call_sign: SmolStr::new_static(""),
            name: SmolStr::new_static(""),
            destination: SmolStr::new_static(""),
            ship_type: 0,
            flag_mid: 0,
            draught: 0,
            dim_to_bow: 0,
            dim_to_stern: 0,
            dim_to_port: 0,
            dim_to_starboard: 0,
            eta_month: 0,
            eta_day: 0,
            eta_hour: 0,
            eta_minute: 0,
        }
    }

    /// A position that carries nothing, as a report with every sentinel set.
    fn empty_position(mmsi: u32) -> GlobalShippingChokepointPosition {
        GlobalShippingChokepointPosition {
            mmsi,
            latitude: 26_500_000,
            longitude: 56_300_000,
            sog: SOG_UNAVAILABLE,
            cog: COG_UNAVAILABLE,
            heading: HEADING_UNAVAILABLE,
            nav_status: u32::from(NAV_STATUS_UNDEFINED),
            last_update: 0,
        }
    }

    /// A keyframe of `positions`, stamped with a time the server holds.
    fn keyframe(
        epoch: u64,
        sequence: u64,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch,
            keyframe: true,
            sequence,
            base: 0,
            statics: Vec::new(),
            positions,
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        }
    }

    /// A delta of `positions`, continuing sequence `base`.
    fn delta(
        epoch: u64,
        base: u64,
        sequence: u64,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch,
            keyframe: false,
            sequence,
            base,
            statics: Vec::new(),
            positions,
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        }
    }

    fn assert_close(left: f32, right: f32, what: &str) {
        assert!(
            (left - right).abs() < 1e-5,
            "{what}: {left} should be {right}",
        );
    }

    #[test]
    fn every_bucket_should_be_reachable_from_a_ship_type() {
        assert_eq!(class_of(70), ShipClass::Container);
        assert_eq!(class_of(74), ShipClass::Container);
        assert_eq!(class_of(75), ShipClass::Bulk);
        assert_eq!(class_of(79), ShipClass::Bulk);
        assert_eq!(class_of(80), ShipClass::Tanker);
        assert_eq!(class_of(89), ShipClass::Tanker);
        assert_eq!(class_of(35), ShipClass::Other, "a military vessel");
        assert_eq!(class_of(60), ShipClass::Other, "a passenger vessel");
        assert_eq!(class_of(0), ShipClass::Other, "a type the report omits");
        assert_eq!(class_of(90), ShipClass::Other, "the range beyond tankers");
        assert_eq!(class_of(u32::MAX), ShipClass::Other);
    }

    #[test]
    fn a_mid_should_read_as_the_flag_state_it_names() {
        for (mid, flag) in [
            (422_u32, "IR"),
            (461, "OM"),
            (470, "AE"),
            (466, "QA"),
            (408, "BH"),
        ] {
            assert_eq!(
                flag_for_mid(mid),
                Some(flag),
                "a coastal state of the strait",
            );
        }

        for (mid, flag) in [
            (351_u32, "PA"),
            (636, "LR"),
            (538, "MH"),
            (215, "MT"),
            (563, "SG"),
            (477, "HK"),
        ] {
            assert_eq!(
                flag_for_mid(mid),
                Some(flag),
                "a flag the fleets in the gulf fly",
            );
        }

        assert_eq!(
            flag_for_mid(0),
            None,
            "a report that names no state has no flag",
        );
        assert_eq!(flag_for_mid(999), None, "an unknown MID is not guessed at");
    }

    #[test]
    fn the_flag_table_should_be_sorted_unique_and_two_letter_codes() {
        for pair in FLAGS.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "the table is looked up by binary search, so it is ordered by MID",
            );
        }

        for (mid, flag) in FLAGS {
            assert!(
                flag.len() == 2 && flag.chars().all(|letter| letter.is_ascii_uppercase()),
                "{mid} reads as {flag}, which is not an ISO 3166-1 alpha-2 code",
            );
        }
    }

    #[test]
    fn a_navigation_status_should_carry_the_index_the_catalogue_names() {
        // The catalogue names 0 through 14; every one of them has to reach the view
        // as its own index so the key `nav_status.{n}` is the one the wire value
        // picked.
        for status in 0..u32::from(NAV_STATUS_UNDEFINED) {
            assert_eq!(
                nav_status_of(status).map(NavStatus::index),
                Some(status as u8),
                "navigation status {status} lost its index",
            );
        }
        assert_eq!(
            nav_status_of(u32::from(NAV_STATUS_UNDEFINED)),
            None,
            "the report says it does not know",
        );
        assert_eq!(nav_status_of(u32::MAX), None);
    }

    #[test]
    fn mercator_should_map_every_corner_to_the_value_the_formula_names() {
        // The projection is pinned against its own definition rather than against its
        // inverse: the map is drawn from these values, so a longitude or a latitude that
        // arrived at the wrong place would be a coastline in the wrong sea.
        let project_by_formula = |longitude: f32, latitude: f32| {
            [
                longitude.to_radians(),
                (std::f32::consts::FRAC_PI_4 + latitude.to_radians() / 2.0)
                    .tan()
                    .ln(),
            ]
        };

        for (longitude, latitude) in [
            (MAP_WEST, MAP_SOUTH),
            (MAP_EAST, MAP_SOUTH),
            (MAP_WEST, MAP_NORTH),
            (MAP_EAST, MAP_NORTH),
            (56.3, 26.4),
            (0.0, 0.0),
        ] {
            let [x, y] = project(longitude, latitude);
            let [expected_x, expected_y] = project_by_formula(longitude, latitude);

            assert_close(x, expected_x, "the x a longitude projects to");
            assert_close(y, expected_y, "the y a latitude projects to");
        }

        // The prime meridian is the origin of x, and the equator is the origin of y: the
        // two anchors that say the formula above is the one being used and not merely
        // its own copy.
        let [x, y] = project(0.0, 0.0);
        assert_close(x, 0.0, "the prime meridian");
        assert_close(y, 0.0, "the equator");
    }

    #[test]
    fn mercator_should_grow_with_both_coordinates() {
        let [west, south, east, north] = mercator_bounds();
        let [west_corner, south_corner] = project(MAP_WEST, MAP_SOUTH);
        let [east_corner, north_corner] = project(MAP_EAST, MAP_NORTH);
        assert_eq!([west, south], [west_corner, south_corner]);
        assert_eq!([east, north], [east_corner, north_corner]);
        assert!(west < east, "east is to the right of west");
        assert!(south < north, "north is above south");

        // Sampled rather than only at the corners, so the projection is pinned as
        // monotonic in each coordinate instead of merely consistent with itself.
        let mut previous: Option<[f32; 2]> = None;
        for step in 0..=8 {
            let fraction = step as f32 / 8.0;
            let longitude = MAP_WEST + (MAP_EAST - MAP_WEST) * fraction;
            let latitude = MAP_SOUTH + (MAP_NORTH - MAP_SOUTH) * fraction;
            let point = project(longitude, latitude);

            if let Some(previous) = previous {
                assert!(point[0] > previous[0], "x grows with longitude");
                assert!(point[1] > previous[1], "y grows with latitude");
            }
            previous = Some(point);
        }
    }

    #[test]
    fn a_camera_should_stay_inside_the_map_on_every_edge() {
        let viewport = [800.0, 600.0];
        let zoom = 2.0;
        let bounds = mercator_bounds();
        let centre = map_centre();
        let scale = fit_px_per_merc(viewport) * zoom;
        let half_width = viewport[0] / (2.0 * scale);
        let half_height = viewport[1] / (2.0 * scale);

        let west = Camera {
            center: [bounds[0] - 5.0, centre[1]],
            zoom,
        }
        .clamped(viewport);
        assert_close(
            west.center[0],
            bounds[0] + half_width,
            "the west edge is where the map starts",
        );

        let east = Camera {
            center: [bounds[2] + 5.0, centre[1]],
            zoom,
        }
        .clamped(viewport);
        assert_close(
            east.center[0],
            bounds[2] - half_width,
            "the east edge is where the map ends",
        );

        let south = Camera {
            center: [centre[0], bounds[1] - 5.0],
            zoom,
        }
        .clamped(viewport);
        assert_close(
            south.center[1],
            bounds[1] + half_height,
            "the south edge is where the map starts",
        );

        let north = Camera {
            center: [centre[0], bounds[3] + 5.0],
            zoom,
        }
        .clamped(viewport);
        assert_close(
            north.center[1],
            bounds[3] - half_height,
            "the north edge is where the map ends",
        );
    }

    #[test]
    fn a_camera_should_centre_an_axis_the_map_cannot_fill() {
        // A portrait viewport: the map's height is narrower than the visible height,
        // so that axis is centred while the width is still clamped.
        let viewport = [400.0, 1_200.0];
        let zoom = 2.0;
        let bounds = mercator_bounds();
        let scale = fit_px_per_merc(viewport) * zoom;
        let half_width = viewport[0] / (2.0 * scale);

        let camera = Camera {
            center: [bounds[0] + half_width, bounds[1] - 100.0],
            zoom,
        }
        .clamped(viewport);

        assert_close(
            camera.center[0],
            bounds[0] + half_width,
            "the width is still clamped",
        );
        assert_close(
            camera.center[1],
            map_centre()[1],
            "the height is centred on the map",
        );
    }

    #[test]
    fn a_degenerate_viewport_should_centre_the_whole_map() {
        let bounds = mercator_bounds();
        let centre = [(bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0];

        for viewport in [[0.0, 0.0], [800.0, 0.0], [f32::NAN, 600.0]] {
            let camera = Camera {
                center: [0.0, 0.0],
                zoom: 2.0,
            }
            .clamped(viewport);

            assert_eq!(camera.center, centre, "a viewport with no area has no fit");
            assert_eq!(camera.zoom, 2.0, "and the zoom the pane asked for is kept");
        }
    }

    #[test]
    fn a_camera_should_keep_its_zoom_between_the_two_ends() {
        let viewport = [800.0, 600.0];
        let too_far_in = Camera {
            center: map_centre(),
            zoom: 8.0,
        }
        .clamped(viewport);
        assert_eq!(too_far_in.zoom, MAX_ZOOM, "the wheel cannot go past four");

        let too_far_out = Camera {
            center: map_centre(),
            zoom: 0.5,
        }
        .clamped(viewport);
        assert_eq!(too_far_out.zoom, MIN_ZOOM, "nor closer than the whole map");
    }

    #[test]
    fn the_nearest_vessel_should_be_the_one_the_pointer_is_on() {
        let points = [(1, [10.0, 10.0]), (2, [11.5, 10.0]), (3, [30.0, 30.0])];
        assert_eq!(hit_test(&points, [11.5, 10.0]), Some(2));
        assert_eq!(hit_test(&points, [10.2, 10.2]), Some(1));
    }

    #[test]
    fn a_pointer_should_hit_nothing_beyond_the_radius_or_on_an_empty_map() {
        assert_eq!(hit_test(&[], [0.0, 0.0]), None);
        assert_eq!(
            hit_test(&[(1, [HIT_RADIUS_PX, 0.0])], [0.0, 0.0]),
            None,
            "a point exactly at the radius is outside it",
        );
        assert_eq!(
            hit_test(&[(1, [HIT_RADIUS_PX - 0.1, 0.0])], [0.0, 0.0]),
            Some(1),
            "one just inside it is a hit",
        );
        assert_eq!(
            hit_test(&[(1, [100.0, 100.0])], [0.0, 0.0]),
            None,
            "a marker across the map is not under the pointer",
        );
    }

    #[test]
    fn a_keyframe_should_replace_the_whole_picture() {
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![position(1), position(2)]);
        frame.statics = vec![statics(1), statics(2)];
        store.apply(&frame);
        assert!(store.has_state());
        assert_eq!(store.vessel_count(), 2);
        assert!(!store.is_stale());

        store.apply(&keyframe(7, 2, vec![position(3)]));

        assert!(store.has_state());
        assert_eq!(
            store.vessel_count(),
            1,
            "the picture is exactly the keyframe"
        );
        assert!(!store.statics.contains_key(&1));
        assert!(!store.positions.contains_key(&1));
    }

    #[test]
    fn a_delta_on_the_applied_base_should_be_applied() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 10, vec![position(1)]));

        let mut update = delta(7, 10, 11, vec![position(2)]);
        update.statics = vec![statics(2)];
        store.apply(&update);

        assert!(store.has_state());
        assert!(!store.is_stale());
        assert_eq!(
            store.vessel_count(),
            2,
            "the delta is laid over the keyframe"
        );
        assert!(store.statics.contains_key(&2));
    }

    #[test]
    fn a_removal_should_empty_both_tables() {
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![position(1), position(2)]);
        frame.statics = vec![statics(1), statics(2)];
        store.apply(&frame);

        let mut update = delta(7, 1, 2, Vec::new());
        update.removed = vec![1];
        store.apply(&update);

        assert_eq!(store.vessel_count(), 1);
        assert!(!store.statics.contains_key(&1), "the static part goes too");
        assert!(store.statics.contains_key(&2));
    }

    #[test]
    fn a_delta_that_skips_a_sequence_should_only_leave_a_gap() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 10, vec![position(1)]));

        store.apply(&delta(7, 5, 6, vec![position(2)]));

        assert!(
            store.is_stale(),
            "a picture that was not continued is stale"
        );
        assert!(
            store.has_state(),
            "the old picture is still what can be drawn"
        );
        assert_eq!(store.vessel_count(), 1, "the ignored delta changed nothing");
        assert_eq!(store.applied, 10, "nor did it move the baseline");
    }

    #[test]
    fn a_new_epoch_should_drop_the_baseline_and_hold_the_picture_until_a_keyframe() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 10, vec![position(1)]));

        store.apply(&delta(8, 10, 11, vec![position(2)]));

        // §9.4's authoritative code only notes the gap on an epoch change: the old
        // picture is retained and the stale badge (derived from that gap) is what
        // tells the reader it is no longer being continued. §13's "clear" wording
        // contradicts §9.4; the retained picture is deliberate and is replaced by the
        // next keyframe.
        assert!(
            store.gap,
            "the badge says the held picture is not continued"
        );
        assert!(
            store.is_stale(),
            "and staleness is derived from the gap, not from a timer",
        );
        assert!(
            store.has_state(),
            "the old picture stays on the map until a keyframe replaces it",
        );
        assert_eq!(store.applied, 0, "the sequence belonged to a dead process");
        assert_eq!(
            store.vessel_count(),
            1,
            "the picture is held until a keyframe replaces it",
        );
        assert_eq!(
            store.epoch, 7,
            "and the old epoch is not adopted on a guess"
        );
    }

    #[test]
    fn a_keyframe_after_a_gap_should_clear_it() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 1, vec![position(1)]));
        store.apply(&delta(7, 5, 6, vec![position(2)]));
        assert!(store.is_stale());

        store.apply(&keyframe(8, 7, vec![position(3)]));

        assert!(!store.is_stale(), "the keyframe healed the gap");
        assert!(store.has_state());
        assert_eq!(store.vessel_count(), 1);
        assert_eq!(store.applied, 7);
        assert_eq!(store.epoch, 8);
    }

    #[test]
    fn a_keyframe_without_a_time_should_clear_the_picture_and_not_establish_a_baseline() {
        let mut store = VesselStore::new();
        let mut seeded = keyframe(7, 10, vec![position(1)]);
        seeded.statics = vec![statics(1)];
        store.apply(&seeded);
        assert_eq!(
            store.vessel_count(),
            1,
            "there is a picture for the no-state keyframe to clear",
        );

        let mut frame = keyframe(8, 11, vec![position(2)]);
        frame.statics = vec![statics(2)];
        frame.timestamp = 0;
        store.apply(&frame);

        assert!(!store.has_state(), "no state yet is not an empty sea");
        assert!(store.is_stale(), "and it is not a live picture either");
        assert_eq!(store.vessel_count(), 0, "the stale picture was cleared");
        assert!(
            store.statics.is_empty(),
            "the static part was cleared with it",
        );
        assert_eq!(
            store.applied, 0,
            "a delta cannot continue a baseline that is not"
        );
        assert_eq!(store.epoch, 8);
    }

    #[test]
    fn staleness_should_be_derived_from_the_gap_and_the_local_clock() {
        let mut store = VesselStore::new();
        assert!(
            !store.is_stale(),
            "a store that never received a push is not stale"
        );

        store.received_at = Instant::now() - (STALE_AFTER - Duration::from_secs(1));
        assert!(
            !store.is_stale(),
            "a picture inside the window is still live",
        );

        // Kept a second beyond the window: at exactly 30 s the strict `>` would race
        // the clock's resolution rather than test the predicate.
        store.received_at = Instant::now() - (STALE_AFTER + Duration::from_secs(1));
        assert!(
            store.is_stale(),
            "no push for the whole window is a broken link, heartbeat included",
        );
        assert!(
            store.silent_for() > STALE_AFTER,
            "the badge counts the silence the link went through",
        );
        assert!(
            !store.has_gap(),
            "a silent link is silence rather than a picture that was not continued",
        );

        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 1, vec![position(1)]));
        assert!(!store.is_stale(), "a push just arrived");
        assert!(!store.has_gap());
        assert!(store.silent_for() < STALE_AFTER);
    }

    #[test]
    fn a_cleared_store_should_not_apply_a_late_delta() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 10, vec![position(1)]));

        store.clear();

        assert!(!store.has_state());
        assert!(!store.is_stale(), "a stopped store is empty, not stale");
        assert_eq!(store.vessel_count(), 0);
        assert_eq!(store.applied, 0);
        assert_eq!(store.epoch, 0);

        store.apply(&delta(7, 10, 11, vec![position(2)]));

        assert!(
            !store.has_state(),
            "the abandoned session's delta no longer continues anything",
        );
        assert_eq!(store.vessel_count(), 0);
        assert!(
            store.is_stale(),
            "the late delta is noted as a gap rather than applied",
        );
    }

    #[test]
    #[should_panic(expected = "a keyframe holds at most 1000 positions")]
    fn a_keyframe_over_capacity_should_assert() {
        let positions = (0..=MAX_VESSELS as u32).map(position).collect();
        VesselStore::new().apply(&keyframe(7, 1, positions));
    }

    #[test]
    fn a_delta_should_not_trim_the_picture_to_the_capacity() {
        let mut store = VesselStore::new();
        let frame = keyframe(7, 1, (0..MAX_VESSELS as u32).map(position).collect());
        store.apply(&frame);
        assert_eq!(store.vessel_count(), MAX_VESSELS);

        // Which vessels are held is the service's decision; a client that trimmed its
        // own set would drift from the set the server is tracking. Only a keyframe is
        // checked against the bound.
        store.apply(&delta(7, 1, 2, vec![position(MAX_VESSELS as u32)]));

        assert!(!store.is_stale());
        assert_eq!(
            store.vessel_count(),
            MAX_VESSELS + 1,
            "an applied delta is laid over the picture, not clipped to capacity",
        );
    }

    #[test]
    fn the_picture_should_be_joined_by_mmsi() {
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![position(1)]);
        frame.statics = vec![statics(1), statics(2)];
        store.apply(&frame);

        assert_eq!(
            store.vessel_count(),
            1,
            "a static part without a position has nowhere to be painted",
        );
        let vessels = store.vessels();
        let vessel = vessels
            .iter()
            .find(|vessel| vessel.mmsi == 1)
            .expect("the vessel with a position is painted");
        assert!(vessel.has_static);
        assert_eq!(vessel.class, ShipClass::Tanker);
        assert_eq!(vessel.position, project(56.3, 26.5));
        assert_eq!(vessel.cog, 456);
        assert_eq!(vessel.heading, 87);
        assert!(!vessels.iter().any(|vessel| vessel.mmsi == 2));

        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 1, vec![position(3)]));
        let vessel = store
            .vessels()
            .into_iter()
            .find(|vessel| vessel.mmsi == 3)
            .expect("a position without a static part is still a vessel");
        assert!(!vessel.has_static);
        assert_eq!(
            vessel.class,
            ShipClass::Other,
            "an unknown type is the other bucket",
        );
    }

    #[test]
    fn every_value_the_report_carries_should_read_as_a_reader_expects_it() {
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![position(42)]);
        frame.statics = vec![statics(42)];
        store.apply(&frame);

        let details = store
            .display(42, UtcOffset::UTC)
            .expect("the vessel is on the map");

        assert_eq!(details.name.as_deref(), Some("SEA JEWEL"));
        assert_eq!(details.mmsi.as_str(), "42");
        assert_eq!(details.imo.as_deref(), Some("9074729"));
        assert_eq!(details.call_sign.as_deref(), Some("A8AB2"));
        assert_eq!(details.ship_class, Some(ShipClass::Tanker));
        assert_eq!(details.flag, Some("LR"));
        assert_eq!(details.destination.as_deref(), Some("FUJAIRAH"));
        assert_eq!(details.eta.as_deref(), Some("03-14 09:05"));
        assert_eq!(details.draught.as_deref(), Some("9.2 m"));
        assert_eq!(details.dimensions.as_deref(), Some("150 × 32 m"));
        assert_eq!(details.speed.as_deref(), Some("12.3 kn"));
        assert_eq!(details.course.as_deref(), Some("45.6°"));
        assert_eq!(details.heading.as_deref(), Some("87°"));
        assert_eq!(details.nav_status.map(NavStatus::index), Some(1));
        assert_eq!(details.updated.as_deref(), Some("2023-11-14 22:13:20"));
    }

    #[test]
    fn a_report_without_a_value_should_read_as_unavailable() {
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![empty_position(7)]);
        frame.statics = vec![empty_statics(7)];
        store.apply(&frame);

        let details = store
            .display(7, UtcOffset::UTC)
            .expect("the vessel is on the map");

        assert_eq!(
            details.name.as_deref(),
            Some("7"),
            "an empty name falls back to the MMSI",
        );
        assert!(details.imo.is_none(), "IMO zero");
        assert!(details.call_sign.is_none(), "an empty call sign");
        assert!(details.ship_class.is_none(), "ship type zero");
        assert!(details.flag.is_none(), "flag MID zero");
        assert!(details.destination.is_none(), "an empty destination");
        assert!(details.eta.is_none(), "all four ETA fields zero");
        assert!(details.draught.is_none(), "draught zero");
        assert!(details.dimensions.is_none(), "all four dimensions zero");
        assert!(details.speed.is_none(), "SOG 1023");
        assert!(details.course.is_none(), "COG 3600");
        assert!(details.heading.is_none(), "heading 511");
        assert!(details.nav_status.is_none(), "navigation status 15");
        assert!(details.updated.is_none(), "receipt time zero");
        assert_eq!(details.mmsi.as_str(), "7");
    }

    #[test]
    fn dimensions_that_would_overflow_should_saturate_rather_than_panic() {
        // The dimensions are two independent u32 halves of a length and a breadth. A
        // malformed report can make each pair sum past u32::MAX, and the card is drawn for
        // whatever the wire carries: clamping keeps a bad report from panicking the pane.
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![position(42)]);
        let mut reported = statics(42);
        reported.dim_to_bow = u32::MAX;
        reported.dim_to_stern = 1;
        reported.dim_to_port = u32::MAX;
        reported.dim_to_starboard = 2;
        frame.statics = vec![reported];
        store.apply(&frame);

        let details = store
            .display(42, UtcOffset::UTC)
            .expect("the vessel is on the map");
        assert_eq!(
            details.dimensions.as_deref(),
            Some("4294967295 × 4294967295 m"),
        );
    }

    #[test]
    fn a_vessel_without_a_static_part_should_read_only_what_its_position_carries() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 1, vec![position(9)]));

        let details = store
            .display(9, UtcOffset::UTC)
            .expect("a position is enough to have a card");

        assert!(details.name.is_none());
        assert!(details.imo.is_none());
        assert!(details.call_sign.is_none());
        assert!(details.ship_class.is_none());
        assert!(details.flag.is_none());
        assert!(details.destination.is_none());
        assert!(details.eta.is_none());
        assert!(details.draught.is_none());
        assert!(details.dimensions.is_none());
        assert_eq!(details.mmsi.as_str(), "9");
        assert_eq!(details.speed.as_deref(), Some("12.3 kn"));
        assert_eq!(details.course.as_deref(), Some("45.6°"));
        assert_eq!(details.heading.as_deref(), Some("87°"));
        assert_eq!(details.nav_status.map(NavStatus::index), Some(1));
        assert_eq!(details.updated.as_deref(), Some("2023-11-14 22:13:20"));

        assert!(
            store.display(10, UtcOffset::UTC).is_none(),
            "a vessel the picture does not hold has no card",
        );
    }

    #[test]
    fn the_receipt_time_should_be_read_in_the_zone_the_pane_is_in() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 1, vec![position(42)]));

        let utc = store.display(42, UtcOffset::UTC).expect("on the map");
        let east = store
            .display(
                42,
                UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset"),
            )
            .expect("on the map");

        assert_eq!(utc.updated.as_deref(), Some("2023-11-14 22:13:20"));
        assert_eq!(east.updated.as_deref(), Some("2023-11-15 06:13:20"));
    }

    #[test]
    fn a_receipt_time_the_calendar_cannot_hold_should_read_as_unavailable() {
        let mut store = VesselStore::new();
        let mut position = position(42);
        position.last_update = i64::MAX;
        store.apply(&keyframe(7, 1, vec![position]));

        let details = store.display(42, UtcOffset::UTC).expect("on the map");
        assert!(
            details.updated.is_none(),
            "an instant past the last date is not a date this can write",
        );
    }

    #[test]
    fn every_class_should_be_drawn_in_a_colour_of_its_own() {
        let theme = Theme::of(Appearance::Light);
        let colours = [
            ShipClass::Tanker,
            ShipClass::Container,
            ShipClass::Bulk,
            ShipClass::Other,
        ]
        .map(|class| colour_for(&theme, class));

        for (first, first_colour) in colours.iter().enumerate() {
            for (second, second_colour) in colours.iter().enumerate().skip(first + 1) {
                assert_ne!(
                    first_colour, second_colour,
                    "classes {first} and {second} are drawn alike",
                );
            }
        }

        assert_eq!(
            colour_for(&theme, ShipClass::Tanker),
            theme.roles.chart.series[0].hsla
        );
        assert_eq!(
            colour_for(&theme, ShipClass::Container),
            theme.roles.chart.series[1].hsla,
        );
        assert_eq!(
            colour_for(&theme, ShipClass::Bulk),
            theme.roles.chart.series[2].hsla
        );
        assert_eq!(
            colour_for(&theme, ShipClass::Other),
            theme.roles.chart.series[4].hsla
        );
    }

    #[test]
    fn a_class_should_be_drawn_in_the_appearance_the_pane_is_in() {
        let light = Theme::of(Appearance::Light);
        let dark = Theme::of(Appearance::Dark);

        for class in [
            ShipClass::Tanker,
            ShipClass::Container,
            ShipClass::Bulk,
            ShipClass::Other,
        ] {
            assert_ne!(
                colour_for(&light, class),
                colour_for(&dark, class),
                "{class:?} draws one colour in both appearances",
            );
        }
    }

    #[test]
    fn the_global_shipping_chokepoint_area_should_sit_inside_the_map() {
        // Projected rather than compared as constants: the comparison is then also
        // the check that both rectangles go through the same projection.
        let [
            global_shipping_chokepoint_west,
            global_shipping_chokepoint_south,
        ] = project(AIS_WEST, AIS_SOUTH);
        let [
            global_shipping_chokepoint_east,
            global_shipping_chokepoint_north,
        ] = project(AIS_EAST, AIS_NORTH);
        let [map_west, map_south] = project(MAP_WEST, MAP_SOUTH);
        let [map_east, map_north] = project(MAP_EAST, MAP_NORTH);

        assert!(
            map_west <= global_shipping_chokepoint_west
                && global_shipping_chokepoint_east <= map_east,
            "the reported area runs past the map east to west",
        );
        assert!(
            map_south <= global_shipping_chokepoint_south
                && global_shipping_chokepoint_north <= map_north,
            "and north to south",
        );
    }

    #[test]
    fn the_lanes_should_sit_inside_the_picture_they_are_drawn_on() {
        assert!(
            (3..=5).contains(&TSS_LANES.len()),
            "the design sketches three to five lanes",
        );

        for (index, lane) in TSS_LANES.iter().enumerate() {
            assert!(lane.len() >= 2, "lane {index} needs a direction");
            for (longitude, latitude) in lane.iter() {
                assert!(
                    (AIS_WEST..=AIS_EAST).contains(longitude),
                    "lane {index} is off the reported area at {longitude}",
                );
                assert!(
                    (AIS_SOUTH..=AIS_NORTH).contains(latitude),
                    "lane {index} is off the reported area at {latitude}",
                );
            }
        }
    }

    #[test]
    fn the_labels_should_be_the_ones_the_catalogue_names() {
        assert_eq!(
            COUNTRY_LABELS.map(|label| label.key),
            [
                "widget.global_shipping_chokepoint.label.country.iran",
                "widget.global_shipping_chokepoint.label.country.oman",
                "widget.global_shipping_chokepoint.label.country.uae",
                "widget.global_shipping_chokepoint.label.country.qatar",
                "widget.global_shipping_chokepoint.label.country.bahrain",
            ],
        );
        assert_eq!(
            CITY_LABELS.map(|label| label.key),
            [
                "widget.global_shipping_chokepoint.label.city.bandar_abbas",
                "widget.global_shipping_chokepoint.label.city.qeshm",
                "widget.global_shipping_chokepoint.label.city.bandar_lengeh",
                "widget.global_shipping_chokepoint.label.city.khasab",
                "widget.global_shipping_chokepoint.label.city.fujairah",
                "widget.global_shipping_chokepoint.label.city.ras_al_khaimah",
                "widget.global_shipping_chokepoint.label.city.sharjah",
                "widget.global_shipping_chokepoint.label.city.al_jubayl",
                "widget.global_shipping_chokepoint.label.city.al_hofuf",
                "widget.global_shipping_chokepoint.label.city.chabahar",
                "widget.global_shipping_chokepoint.label.city.muscat",
                "widget.global_shipping_chokepoint.label.city.sur",
            ],
        );
        assert_eq!(
            WATER_LABELS.map(|label| label.key),
            [
                "widget.global_shipping_chokepoint.label.water.persian_gulf",
                "widget.global_shipping_chokepoint.label.water.gulf_of_oman",
            ],
        );
    }

    #[test]
    fn every_label_should_sit_inside_the_map_on_a_point_of_its_own() {
        let mut anchors: Vec<[f32; 2]> = COUNTRY_LABELS
            .iter()
            .chain(CITY_LABELS.iter())
            .chain(WATER_LABELS.iter())
            .map(|label| {
                let [longitude, latitude] = label.at;
                assert!(
                    (MAP_WEST..=MAP_EAST).contains(&longitude),
                    "{} is off the map at {longitude}",
                    label.key,
                );
                assert!(
                    (MAP_SOUTH..=MAP_NORTH).contains(&latitude),
                    "{} is off the map at {latitude}",
                    label.key,
                );
                label.at
            })
            .collect();

        anchors.sort_by(|left, right| {
            left[0]
                .total_cmp(&right[0])
                .then(left[1].total_cmp(&right[1]))
        });
        anchors.dedup();
        assert_eq!(
            anchors.len(),
            19,
            "two labels are anchored on the same point"
        );
    }

    #[test]
    fn a_marker_should_face_its_course_and_fall_back_to_its_heading() {
        let mut vessel = Vessel {
            mmsi: 1,
            class: ShipClass::Other,
            position: [0.0, 0.0],
            cog: 900,
            heading: 347,
            has_static: true,
        };
        assert_close(
            vessel.facing(),
            90.0,
            "a course over ground is tenths of a degree",
        );

        vessel.cog = COG_UNAVAILABLE;
        assert_close(
            vessel.facing(),
            347.0,
            "the heading takes over at the sentinel",
        );

        vessel.heading = HEADING_UNAVAILABLE;
        assert_close(
            vessel.facing(),
            0.0,
            "a report that knows neither faces north",
        );
    }
}
